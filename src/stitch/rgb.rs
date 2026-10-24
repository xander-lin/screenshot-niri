use std::error::Error;

use wayland_client::protocol::wl_shm::Format;

use crate::image::Image;

use super::engine::BinLayout;
use super::parallel::helper_threads;
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
        for (index, destination) in out
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .take(count as usize)
            .enumerate()
        {
            let Some(column) = x.checked_add(index as u32) else {
                return;
            };
            let pixel = self.pixel_rgb(column, y);
            destination.copy_from_slice(&pixel);
        }
    }

    /// Reads every `step`-th pixel of row `y`, starting at column `x`, into
    /// `out` (3 bytes per sample).
    ///
    /// The near-duplicate and contrast checks sample a grid — every fourth
    /// pixel of every fourth row. Through `pixel_rgb` each sample costs a
    /// virtual call and, on the canvas, a tile lookup plus a working-set touch:
    /// at 4K that bookkeeping was 2.3 ms of a frame's 18.7 ms. A source that
    /// can read a row at once overrides this and pays it once per row instead.
    fn row_rgb_sampled_into(&self, x: u32, y: u32, step: u32, out: &mut [u8]) {
        for (index, pixel) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            *pixel = self.pixel_rgb(x + index as u32 * step, y);
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
            // `out` may be longer than the source supplies; copying the common
            // prefix keeps the accessor total instead of panicking on a short
            // destination.
            let available = row.len().min(out.len());
            out[..available].copy_from_slice(&row[..available]);
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

    fn row_rgb_sampled_into(&self, x: u32, y: u32, step: u32, out: &mut [u8]) {
        let start = y as usize * self.stride as usize + x as usize * 3;
        let row = self.data.get(start..).unwrap_or(&[]);
        for (index, pixel) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let offset = index * step as usize * 3;
            match row.get(offset..offset + 3) {
                Some(bytes) => pixel.copy_from_slice(bytes),
                None => pixel.copy_from_slice(&[0, 0, 0]),
            }
        }
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

    fn row_rgb_sampled_into(&self, x: u32, y: u32, step: u32, out: &mut [u8]) {
        let row = self.crop.y + y;
        let start = row as usize * self.image.stride as usize
            + (self.crop.x + x) as usize * 4;
        let row = self.image.data.get(start..).unwrap_or(&[]);
        for (index, pixel) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let offset = index * step as usize * 4;
            match row.get(offset..offset + 4) {
                // ARGB8888 in memory is B, G, R, X; the packed form is R, G, B.
                Some(bytes) => {
                    pixel[0] = bytes[2];
                    pixel[1] = bytes[1];
                    pixel[2] = bytes[0];
                }
                None => pixel.copy_from_slice(&[0, 0, 0]),
            }
        }
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

    fn row_rgb_sampled_into(&self, x: u32, y: u32, step: u32, out: &mut [u8]) {
        self.source
            .row_rgb_sampled_into(self.crop.x + x, self.crop.y + y, step, out)
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
    // Every fourth pixel of every fourth row, read a row at a time: on the
    // canvas each `pixel_rgb` was a tile lookup and a working-set touch, and at
    // 4K that bookkeeping was 2.3 ms of an 18.7 ms frame.
    const STEP: u32 = 4;
    let per_row = width.div_ceil(STEP) as usize;
    let mut a_row = vec![0u8; per_row * 3];
    let mut b_row = vec![0u8; per_row * 3];
    for y in (0..height).step_by(STEP as usize) {
        a.row_rgb_sampled_into(0, y, STEP, &mut a_row);
        b.row_rgb_sampled_into(0, y, STEP, &mut b_row);
        for (pa, pb) in a_row
            .as_chunks::<3>()
            .0
            .iter()
            .zip(b_row.as_chunks::<3>().0.iter())
        {
            total += (i32::from(pa[0]) - i32::from(pb[0])).unsigned_abs() as u64
                + (i32::from(pa[1]) - i32::from(pb[1])).unsigned_abs() as u64
                + (i32::from(pa[2]) - i32::from(pb[2])).unsigned_abs() as u64;
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
/// The near-duplicate difference and the duplicate threshold's input, from one
/// pass over the frame.
///
/// Both need exactly the same samples — every fourth pixel of every fourth row
/// of `b` — so reading `b` twice (`average_difference_same_source` then
/// `luminance_range_source`) cost a redundant sampled pass: measured 3% of a
/// 1080p push. The answers are identical to the separate functions.
pub(super) fn sampled_difference_and_luminance_range(
    a: &impl RgbSource,
    b: &impl RgbSource,
) -> (f64, f64) {
    let width = a.width().min(b.width());
    let height = a.height().min(b.height());
    let mut total = 0u64;
    let mut samples = 0u64;
    let mut lo = 255u32;
    let mut hi = 0u32;
    const STEP: u32 = 4;
    let per_row = width.div_ceil(STEP) as usize;
    let mut a_row = vec![0u8; per_row * 3];
    let mut b_row = vec![0u8; per_row * 3];
    for y in (0..height).step_by(STEP as usize) {
        a.row_rgb_sampled_into(0, y, STEP, &mut a_row);
        b.row_rgb_sampled_into(0, y, STEP, &mut b_row);
        for (pa, pb) in a_row
            .as_chunks::<3>()
            .0
            .iter()
            .zip(b_row.as_chunks::<3>().0.iter())
        {
            total += (i32::from(pa[0]) - i32::from(pb[0])).unsigned_abs() as u64
                + (i32::from(pa[1]) - i32::from(pb[1])).unsigned_abs() as u64
                + (i32::from(pa[2]) - i32::from(pb[2])).unsigned_abs() as u64;
            let l =
                (77 * u32::from(pb[0]) + 150 * u32::from(pb[1]) + 29 * u32::from(pb[2])) >> 8;
            lo = lo.min(l);
            hi = hi.max(l);
            samples += 1;
        }
    }
    let difference = if samples == 0 {
        f64::MAX
    } else {
        total as f64 / samples as f64 / 3.0
    };
    let range = if hi >= lo { (hi - lo) as f64 } else { 0.0 };
    (difference, range)
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
    buffer: Vec<u8>,
) -> Result<RgbFrame, Box<dyn Error>> {
    rgb_frame_from_source_reusing_with(source, buffer, |_, _| {})
}

/// Builds a frame, reusing `buffer`, and hands each converted row to `row_hook`.
///
/// The stitcher's descriptor is binned luminance along the scroll axis, and it
/// used to be built by reading the finished frame a second time — another 25 MB
/// pass at 4K, over memory that had just left the cache. The hook lets the
/// caller accumulate its bins from the row that was just written, while that row
/// is still hot, so the frame is converted and described in one pass. The hook
/// sees exactly the bytes the frame holds: a fused descriptor is the same
/// descriptor.
pub(super) fn rgb_frame_from_source_reusing_with<F>(
    source: &impl RgbSource,
    mut buffer: Vec<u8>,
    mut row_hook: F,
) -> Result<RgbFrame, Box<dyn Error>>
where
    F: FnMut(u32, &[u8]),
{
    validate_rgb_source(source)?;
    let stride = source.width().checked_mul(3).ok_or("RGB stride overflow")?;
    let size = (stride as usize)
        .checked_mul(source.height() as usize)
        .ok_or("RGB frame size overflow")?;
    buffer.resize(size, 0);
    let mut data = buffer;
    // Row at a time, straight into the frame: the per-pixel form paid a virtual
    // call and two bounds checks per pixel to move three bytes, and reading each
    // row into a scratch buffer first would double the traffic (6.2 MB copied
    // twice per 1080p frame) for no benefit. `resize` above leaves the bytes
    // past `size` untouched, and every byte up to `size` is written below, so a
    // recycled buffer never carries a stale pixel into the new frame.
    for y in 0..source.height() {
        let start = y as usize * stride as usize;
        let row = &mut data[start..start + stride as usize];
        source.row_rgb_into(0, y, source.width(), row);
        row_hook(y, row);
    }
    Ok(RgbFrame {
        width: source.width(),
        height: source.height(),
        stride,
        data,
    })
}

/// Pixels a helper should own before one is spawned.
///
/// A 1080p frame (2.1 MP) splits four ways and already takes ~3 ms to convert
/// and describe; the smallest corpus region (163 k px) finishes in tens of
/// microseconds and would only pay thread wake-ups.
const FUSED_MIN_PIXELS_PER_THREAD: u64 = 250_000;

/// Converts a frame and builds its descriptor bins in one parallel pass.
///
/// The row loop is split into contiguous bands: helper `n` reads its own rows
/// of `source` and writes its own `data` and `means` slices, so nothing is
/// shared but the read-only source. The arithmetic per row is identical to the
/// serial path (`BinLayout::means_into` is the same definition of the
/// descriptor), so the result is bit-identical — the corpora replay to the same
/// canvases.
pub(super) fn rgb_frame_from_source_fused<S>(
    source: &S,
    buffer: Vec<u8>,
    layout: BinLayout,
) -> Result<(RgbFrame, Vec<f32>), Box<dyn Error>>
where
    S: RgbSource + Sync,
{
    validate_rgb_source(source)?;
    let (width, height) = (source.width(), source.height());
    let stride = width.checked_mul(3).ok_or("RGB stride overflow")?;
    let size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("RGB frame size overflow")?;
    let bins = layout.bins() as usize;
    let mut data = buffer;
    data.resize(size, 0);
    let mut means = vec![0f32; height as usize * bins];
    let rows = height as usize;

    let threads = helper_threads(
        u64::from(width) * u64::from(height),
        FUSED_MIN_PIXELS_PER_THREAD,
    );
    if threads <= 1 {
        for y in 0..height {
            let start = y as usize * stride as usize;
            let row = &mut data[start..start + stride as usize];
            source.row_rgb_into(0, y, width, row);
            layout.means_into(row, &mut means[y as usize * bins..(y as usize + 1) * bins]);
        }
    } else {
        let rows_per_thread = rows.div_ceil(threads);
        std::thread::scope(|scope| {
            let mut data_rest = data.as_mut_slice();
            let mut means_rest = means.as_mut_slice();
            let mut first_row = 0usize;
            while first_row < rows {
                let take = rows_per_thread.min(rows - first_row);
                let (row_data, next_data) = data_rest.split_at_mut(take * stride as usize);
                let (row_means, next_means) = means_rest.split_at_mut(take * bins);
                data_rest = next_data;
                means_rest = next_means;
                scope.spawn(move || {
                    for local in 0..take {
                        let y = (first_row + local) as u32;
                        let row = &mut row_data[local * stride as usize..(local + 1) * stride as usize];
                        source.row_rgb_into(0, y, width, row);
                        layout.means_into(row, &mut row_means[local * bins..(local + 1) * bins]);
                    }
                });
                first_row += take;
            }
        });
    }
    Ok((
        RgbFrame {
            width,
            height,
            stride,
            data,
        },
        means,
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The duplicate and contrast checks read their samples a row at a time.
    /// That is an access-path change only if it returns exactly what the
    /// per-pixel accessor returns.
    #[test]
    fn sampled_rows_match_the_per_pixel_accessor() {
        let frame = RgbFrame {
            width: 9,
            height: 5,
            stride: 27,
            data: (0..9 * 5 * 3).map(|value| (value * 7 % 251) as u8).collect(),
        };
        let mut row = vec![0u8; 9usize.div_ceil(4) * 3];
        for y in (0..frame.height).step_by(4) {
            frame.row_rgb_sampled_into(0, y, 4, &mut row);
            for (index, pixel) in row.as_chunks::<3>().0.iter().enumerate() {
                assert_eq!(*pixel, frame.pixel_rgb(index as u32 * 4, y), "({},{y})", index * 4);
            }
        }

        // The compose frame is sampled through its RGBA view (BGRA in memory),
        // including a crop that does not start on a sample boundary.
        let image = Image {
            width: 12,
            height: 6,
            stride: 48,
            format: Format::Argb8888,
            data: (0..12 * 6 * 4).map(|value| (value * 11 % 253) as u8).collect(),
        };
        let view = ImageRgbView::with_crop(
            &image,
            ComposeCrop { x: 3, y: 1, width: 8, height: 4 },
        )
        .unwrap();
        let mut row = vec![0u8; 8usize.div_ceil(4) * 3];
        for y in (0..view.height()).step_by(4) {
            view.row_rgb_sampled_into(0, y, 4, &mut row);
            for (index, pixel) in row.as_chunks::<3>().0.iter().enumerate() {
                assert_eq!(*pixel, view.pixel_rgb(index as u32 * 4, y), "({},{y})", index * 4);
            }
        }
    }
}
