use std::error::Error;

use wayland_client::protocol::wl_shm::Format;

use crate::image::Image;

use super::perceptual::PerceptualFrame;
use super::{ComposeCrop, RgbFrame};

#[derive(Debug, Clone, Copy)]
pub struct ImageRgbView<'a> {
    image: &'a Image,
    crop: ComposeCrop,
}

pub trait RgbSource {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3];

    /// Reads `count` pixels of row `y`, starting at column `x`, into `out` as
    /// packed RGB (`3 * count` bytes).
    ///
    /// The descriptor builder sums a whole row of bins at a time and was calling
    /// `pixel_rgb` once per pixel: a virtual call, a bounds check and a
    /// four-byte stride, all to add one luminance to an accumulator. On a 1080p
    /// frame that was 14.6% of the frame's cost. The default keeps every source
    /// correct — including the test mocks — and the two real sources override it
    /// with a single slice copy per row.
    fn row_rgb_into(&self, x: u32, y: u32, count: u32, out: &mut [u8]) {
        // The contract is that `out` holds `count` pixels. A caller that passes
        // a short buffer gets the prefix it can hold rather than a panic: this
        // runs per row of every frame, and the bounds are already implied by the
        // row being inside the source.
        for (index, destination) in out.chunks_exact_mut(3).take(count as usize).enumerate() {
            let Some(column) = x.checked_add(index as u32) else {
                return;
            };
            let pixel = self.pixel_rgb(column, y);
            destination.copy_from_slice(&pixel);
        }
    }
}

pub(super) struct CroppedRgbSource<'a, S> {
    source: &'a S,
    crop: ComposeCrop,
}

impl<'a> ImageRgbView<'a> {
    pub fn new(image: &'a Image) -> Result<Self, Box<dyn Error>> {
        Self::with_crop(
            image,
            ComposeCrop {
                x: 0,
                y: 0,
                width: image.width,
                height: image.height,
            },
        )
    }

    pub fn with_crop(image: &'a Image, crop: ComposeCrop) -> Result<Self, Box<dyn Error>> {
        validate_image_rgb_source(image)?;
        validate_crop(image.width, image.height, crop, "image RGB view")?;
        Ok(Self { image, crop })
    }
}

impl RgbSource for RgbFrame {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn row_rgb_into(&self, x: u32, y: u32, count: u32, out: &mut [u8]) {
        let start = y as usize * self.stride as usize + x as usize * 3;
        let end = start + count as usize * 3;
        if let Some(row) = self.data.get(start..end) {
            out[..row.len()].copy_from_slice(row);
        }
    }

    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let offset = pixel_offset(self, x, y);
        [
            self.data[offset],
            self.data[offset + 1],
            self.data[offset + 2],
        ]
    }
}

impl RgbSource for ImageRgbView<'_> {
    fn width(&self) -> u32 {
        self.crop.width
    }

    fn height(&self) -> u32 {
        self.crop.height
    }

    fn row_rgb_into(&self, x: u32, y: u32, count: u32, out: &mut [u8]) {
        let row = self.crop.y + y;
        let start = row as usize * self.image.stride as usize
            + (self.crop.x + x) as usize * 4;
        let end = start + count as usize * 4;
        let Some(source) = self.image.data.get(start..end) else {
            return;
        };
        // ARGB8888 in memory is B, G, R, X; the packed form is R, G, B.
        for (pixel, destination) in source
            .as_chunks::<4>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<3>().0.iter_mut())
        {
            destination[0] = pixel[2];
            destination[1] = pixel[1];
            destination[2] = pixel[0];
        }
    }

    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let src_x = self.crop.x + x;
        let src_y = self.crop.y + y;
        let offset = src_y as usize * self.image.stride as usize + src_x as usize * 4;
        [
            self.image.data[offset + 2],
            self.image.data[offset + 1],
            self.image.data[offset],
        ]
    }
}

impl<'a, S> CroppedRgbSource<'a, S>
where
    S: RgbSource,
{
    pub(super) fn new(source: &'a S, crop: ComposeCrop) -> Result<Self, Box<dyn Error>> {
        validate_crop(source.width(), source.height(), crop, "RGB source")?;
        Ok(Self { source, crop })
    }
}

impl<S> RgbSource for CroppedRgbSource<'_, S>
where
    S: RgbSource,
{
    fn width(&self) -> u32 {
        self.crop.width
    }

    fn height(&self) -> u32 {
        self.crop.height
    }

    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        self.source.pixel_rgb(self.crop.x + x, self.crop.y + y)
    }

    fn row_rgb_into(&self, x: u32, y: u32, count: u32, out: &mut [u8]) {
        self.source
            .row_rgb_into(self.crop.x + x, self.crop.y + y, count, out)
    }
}

#[allow(dead_code)]
pub fn rgb_frame_from_image(image: &Image) -> Result<RgbFrame, Box<dyn Error>> {
    if image.format != Format::Argb8888 && image.format != Format::Xrgb8888 {
        return Err(format!("unsupported shm format for stitching: {:?}", image.format).into());
    }
    if image.width == 0 || image.height == 0 || image.stride < image.width.saturating_mul(4) {
        return Err(format!(
            "invalid source image geometry {}x{} stride {}",
            image.width, image.height, image.stride
        )
        .into());
    }
    let required_len = (image.stride as usize)
        .checked_mul(image.height as usize)
        .ok_or("source image size overflow")?;
    if image.data.len() < required_len {
        return Err("source image data is shorter than stride * height".into());
    }

    let stride = image.width.checked_mul(3).ok_or("RGB stride overflow")?;
    let size = (stride as usize)
        .checked_mul(image.height as usize)
        .ok_or("RGB frame size overflow")?;
    let mut data = vec![0; size];
    for y in 0..image.height as usize {
        let src_row = y * image.stride as usize;
        let dst_row = y * stride as usize;
        for x in 0..image.width as usize {
            let src = src_row + x * 4;
            let dst = dst_row + x * 3;
            data[dst] = image.data[src + 2];
            data[dst + 1] = image.data[src + 1];
            data[dst + 2] = image.data[src];
        }
    }
    Ok(RgbFrame {
        width: image.width,
        height: image.height,
        stride,
        data,
    })
}

pub fn average_difference_same(a: &RgbFrame, b: &RgbFrame) -> f64 {
    average_difference_same_source(a, b)
}

pub(super) fn average_difference_same_source(a: &impl RgbSource, b: &impl RgbSource) -> f64 {
    let width = a.width().min(b.width());
    let height = a.height().min(b.height());
    let mut total = 0u64;
    let mut samples = 0u64;
    for y in (0..height).step_by(4) {
        for x in (0..width).step_by(4) {
            total += pixel_difference_source(a, x, y, b, x, y) as u64;
            samples += 1;
        }
    }
    if samples == 0 {
        f64::MAX
    } else {
        total as f64 / samples as f64 / 3.0
    }
}

/// Luminance range of a source, sampled every 4 px. Near-duplicate detection
/// uses it to judge "identical" relative to how much contrast the content
/// actually has: an absolute threshold treats real motion on low-contrast
/// content as a duplicate frame and silently drops the capture.
pub(super) fn luminance_range_source(source: &impl RgbSource) -> f64 {
    let mut lo = 255u32;
    let mut hi = 0u32;
    for y in (0..source.height()).step_by(4) {
        for x in (0..source.width()).step_by(4) {
            let [r, g, b] = source.pixel_rgb(x, y);
            let l = (77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8;
            lo = lo.min(l);
            hi = hi.max(l);
        }
    }
    if hi >= lo {
        (hi - lo) as f64
    } else {
        0.0
    }
}

pub(super) fn average_luminance_difference_source(
    previous: &PerceptualFrame,
    current: &impl RgbSource,
) -> f64 {
    let width = previous.width.min(current.width());
    let height = previous.height.min(current.height());
    let mut total = 0.0;
    let mut samples = 0u64;
    for y in (0..height).step_by(4) {
        for x in (0..width).step_by(4) {
            let [r, g, b] = current.pixel_rgb(x, y);
            let current_luminance =
                ((77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8) as f64;
            total += (f64::from(previous.luminance(x, y)) - current_luminance).abs();
            samples += 1;
        }
    }
    if samples == 0 {
        f64::MAX
    } else {
        total / samples as f64
    }
}

pub(super) fn pixel_difference_source(
    a: &impl RgbSource,
    ax: u32,
    ay: u32,
    b: &impl RgbSource,
    bx: u32,
    by: u32,
) -> u32 {
    let a = a.pixel_rgb(ax, ay);
    let b = b.pixel_rgb(bx, by);
    (i32::from(a[0]) - i32::from(b[0])).unsigned_abs()
        + (i32::from(a[1]) - i32::from(b[1])).unsigned_abs()
        + (i32::from(a[2]) - i32::from(b[2])).unsigned_abs()
}

pub(super) fn rgb_frame_from_source(source: &impl RgbSource) -> Result<RgbFrame, Box<dyn Error>> {
    rgb_frame_from_source_reusing(source, Vec::new())
}

/// Builds a frame, reusing `buffer`'s allocation when it is large enough.
///
/// A long capture converts one frame per push, and a 1080p frame is 6.2 MB.
/// Allocating and freeing that every frame hands the allocator a fresh set of
/// pages each time, so the first write to each page takes a kernel page fault —
/// measured at 4.7% of a frame, plus the allocation itself. The stitcher keeps
/// exactly two frames alive (the current one and the previous one), so two
/// buffers cycle forever once the session is warm.
pub(super) fn rgb_frame_from_source_reusing(
    source: &impl RgbSource,
    mut buffer: Vec<u8>,
) -> Result<RgbFrame, Box<dyn Error>> {
    validate_rgb_source(source)?;
    let stride = source.width().checked_mul(3).ok_or("RGB stride overflow")?;
    let size = (stride as usize)
        .checked_mul(source.height() as usize)
        .ok_or("RGB frame size overflow")?;
    buffer.resize(size, 0);
    let mut data = buffer;
    // Row at a time through the bulk accessor: the per-pixel form paid a
    // virtual call and two bounds checks per pixel to move three bytes.
    let mut row = vec![0u8; source.width() as usize * 3];
    for y in 0..source.height() {
        source.row_rgb_into(0, y, source.width(), &mut row);
        let start = y as usize * stride as usize;
        data[start..start + row.len()].copy_from_slice(&row);
    }
    Ok(RgbFrame {
        width: source.width(),
        height: source.height(),
        stride,
        data,
    })
}

pub(super) fn validate_rgb_source(source: &impl RgbSource) -> Result<(), Box<dyn Error>> {
    if source.width() == 0 || source.height() == 0 {
        return Err("invalid RGB source geometry".into());
    }
    Ok(())
}

fn validate_image_rgb_source(image: &Image) -> Result<(), Box<dyn Error>> {
    if image.format != Format::Argb8888 && image.format != Format::Xrgb8888 {
        return Err(format!("unsupported shm format for stitching: {:?}", image.format).into());
    }
    if image.width == 0 || image.height == 0 || image.stride < image.width.saturating_mul(4) {
        return Err(format!(
            "invalid source image geometry {}x{} stride {}",
            image.width, image.height, image.stride
        )
        .into());
    }
    let required_len = (image.stride as usize)
        .checked_mul(image.height as usize)
        .ok_or("source image size overflow")?;
    if image.data.len() < required_len {
        return Err("source image data is shorter than stride * height".into());
    }
    Ok(())
}

fn validate_crop(
    source_width: u32,
    source_height: u32,
    crop: ComposeCrop,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    if crop.width == 0 || crop.height == 0 {
        return Err(format!("invalid {label} crop geometry").into());
    }
    if crop
        .x
        .checked_add(crop.width)
        .ok_or("RGB crop x overflow")?
        > source_width
        || crop
            .y
            .checked_add(crop.height)
            .ok_or("RGB crop y overflow")?
            > source_height
    {
        return Err(format!("{label} crop exceeds source bounds").into());
    }
    Ok(())
}

pub(super) fn pixel_offset(frame: &RgbFrame, x: u32, y: u32) -> usize {
    y as usize * frame.stride as usize + x as usize * 3
}

pub(super) fn validate_rgb_frame(frame: &RgbFrame) -> Result<(), Box<dyn Error>> {
    if frame.width == 0 || frame.height == 0 || frame.stride < frame.width.saturating_mul(3) {
        return Err("invalid RGB frame geometry".into());
    }
    let required = (frame.stride as usize)
        .checked_mul(frame.height as usize)
        .ok_or("RGB frame size overflow")?;
    if frame.data.len() < required {
        return Err("RGB frame data too short".into());
    }
    Ok(())
}

pub(super) fn crop_rgb_frame(
    frame: &RgbFrame,
    crop: ComposeCrop,
) -> Result<RgbFrame, Box<dyn Error>> {
    validate_rgb_frame(frame)?;
    if crop.width == 0 || crop.height == 0 {
        return Err("invalid RGB crop geometry".into());
    }
    if crop
        .x
        .checked_add(crop.width)
        .ok_or("RGB crop x overflow")?
        > frame.width
        || crop
            .y
            .checked_add(crop.height)
            .ok_or("RGB crop y overflow")?
            > frame.height
    {
        return Err("RGB crop exceeds frame bounds".into());
    }

    let stride = crop
        .width
        .checked_mul(3)
        .ok_or("RGB crop stride overflow")?;
    let size = (stride as usize)
        .checked_mul(crop.height as usize)
        .ok_or("RGB crop size overflow")?;
    let mut data = vec![0; size];
    for row in 0..crop.height as usize {
        let src_row = (crop.y as usize + row) * frame.stride as usize + crop.x as usize * 3;
        let dst_row = row * stride as usize;
        data[dst_row..dst_row + stride as usize]
            .copy_from_slice(&frame.data[src_row..src_row + stride as usize]);
    }
    Ok(RgbFrame {
        width: crop.width,
        height: crop.height,
        stride,
        data,
    })
}
