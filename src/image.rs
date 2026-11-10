use std::error::Error;
use std::fs;
use std::path::Path;

use wayland_client::protocol::wl_shm::Format;

use crate::wayland::screencopy::{CapturedOutput, CaptureOutputRegion};

#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
    /// Pixel bytes.
    ///
    /// May be *longer* than `stride * height`: the preview path recycles a
    /// buffer across frames, and the allocation is a high-water mark so a
    /// window that shrinks does not have to reallocate. Readers must bound
    /// themselves by `stride * height`, which is what every one of them
    /// already does; nothing may assume the two lengths are equal.
    pub data: Vec<u8>,
}

pub fn write_png(path: &Path, image: &Image) -> Result<(), Box<dyn Error>> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    // Write to a sibling temp file and rename into place: the final path must
    // never hold a partial PNG. The clipboard provider is armed while the
    // encode may still be running (see app.rs), and a paste inside that
    // window has to fail cleanly on a missing file instead of reading a
    // half-written image. Writing through `&File` (no BufWriter) keeps write
    // errors on the `?` path — a buffered writer would flush its remainder
    // silently on drop.
    let temp = tempfile::NamedTempFile::new_in(dir)?;
    let mut encoder = png::Encoder::new(temp.as_file(), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // The pair `capture-frames` measured at 27 ms per 4K frame against 84 ms
    // for the encoder's default profile. The encode runs on a background
    // thread after the UI has closed; this keeps even that exit tail short
    // for multi-screen composites.
    encoder.set_compression(png::Compression::Fast);
    encoder.set_filter(png::Filter::Sub);
    encoder.write_header()?.write_image_data(&to_rgba(image)?)?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn composite_captured_region(
    regions: &[CaptureOutputRegion],
    width: u32,
    height: u32,
    outputs: &[CapturedOutput],
) -> Result<Image, Box<dyn Error>> {
    if width == 0 || height == 0 {
        return Err("composite capture size must be non-zero".into());
    }
    let stride = width.checked_mul(4).ok_or("composite stride overflow")?;
    let mut composite = Image {
        width,
        height,
        stride,
        format: Format::Xrgb8888,
        data: vec![0; stride as usize * height as usize],
    };
    for region in regions {
        let source = outputs
            .iter()
            .find(|output| output.output_name == region.output_name)
            .ok_or("selected output was not captured before selection")?;
        blit_image_region(&source.image, region, &mut composite)?;
    }
    Ok(composite)
}

pub fn to_rgba(image: &Image) -> Result<Vec<u8>, Box<dyn Error>> {
    validate_image(image)?;
    if image.format != Format::Argb8888 && image.format != Format::Xrgb8888 {
        return Err(format!("unsupported shm format: {:?}", image.format).into());
    }
    let mut out = vec![0; image.width as usize * image.height as usize * 4];
    for y in 0..image.height as usize {
        let src_row = y * image.stride as usize;
        let dst_row = y * image.width as usize * 4;
        for x in 0..image.width as usize {
            let src = src_row + x * 4;
            let dst = dst_row + x * 4;
            out[dst] = image.data[src + 2];
            out[dst + 1] = image.data[src + 1];
            out[dst + 2] = image.data[src];
            out[dst + 3] = if image.format == Format::Argb8888 { image.data[src + 3] } else { 255 };
        }
    }
    Ok(out)
}

fn blit_image_region(source: &Image, region: &CaptureOutputRegion, dest: &mut Image) -> Result<(), Box<dyn Error>> {
    validate_image(source)?;
    validate_image(dest)?;
    if source.format != Format::Argb8888 && source.format != Format::Xrgb8888 {
        return Err(format!("unsupported source shm format: {:?}", source.format).into());
    }
    let sx = u32::try_from(region.region.x).map_err(|_| "capture region has negative x")?;
    let sy = u32::try_from(region.region.y).map_err(|_| "capture region has negative y")?;
    let width = u32::try_from(region.region.width).map_err(|_| "capture region has negative width")?;
    let height = u32::try_from(region.region.height).map_err(|_| "capture region has negative height")?;
    if sx.checked_add(width).ok_or("source x overflow")? > source.width
        || sy.checked_add(height).ok_or("source y overflow")? > source.height
        || region.dst_x.checked_add(width).ok_or("destination x overflow")? > dest.width
        || region.dst_y.checked_add(height).ok_or("destination y overflow")? > dest.height
    {
        return Err("capture region exceeds image bounds".into());
    }

    for row in 0..height as usize {
        let src = (sy as usize + row) * source.stride as usize + sx as usize * 4;
        let dst = (region.dst_y as usize + row) * dest.stride as usize + region.dst_x as usize * 4;
        let bytes = width as usize * 4;
        dest.data[dst..dst + bytes].copy_from_slice(&source.data[src..src + bytes]);
    }
    Ok(())
}

fn validate_image(image: &Image) -> Result<(), Box<dyn Error>> {
    if image.width == 0 || image.height == 0 || image.stride < image.width.saturating_mul(4) {
        return Err(format!("invalid image geometry {}x{} stride {}", image.width, image.height, image.stride).into());
    }
    let required_len = (image.stride as usize).checked_mul(image.height as usize).ok_or("image size overflow")?;
    if image.data.len() < required_len {
        return Err("image data is shorter than geometry requires".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use crate::wayland::screencopy::Region;

    #[test]
    fn xrgb8888_to_rgba_sets_alpha_to_255() {
        let image = Image {
            width: 2,
            height: 1,
            stride: 8,
            format: Format::Xrgb8888,
            data: vec![3, 2, 1, 0, 6, 5, 4, 7],
        };

        assert_eq!(to_rgba(&image).unwrap(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn argb8888_to_rgba_preserves_source_alpha() {
        let image = Image { width: 1, height: 1, stride: 4, format: Format::Argb8888, data: vec![30, 20, 10, 40] };

        assert_eq!(to_rgba(&image).unwrap(), vec![10, 20, 30, 40]);
    }

    /// `write_png` uses the fast encode profile (`Compression::Fast` +
    /// `Filter::Sub`); this pins that its output still decodes to exactly the
    /// source pixels — speed settings must never change a byte of content.
    #[test]
    fn written_png_round_trips_pixel_for_pixel() {
        let image = Image {
            width: 5,
            height: 3,
            stride: 20,
            format: Format::Xrgb8888,
            data: (0..15)
                .flat_map(|i| [(i * 7) as u8, (i * 13 + 1) as u8, (i * 29 + 2) as u8, 0])
                .collect(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round-trip.png");

        write_png(&path, &image).unwrap();

        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut bytes = vec![0; reader.output_buffer_size().expect("decoder info")];
        let info = reader.next_frame(&mut bytes).unwrap();
        assert_eq!((info.width, info.height), (5, 3));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&bytes[..info.buffer_size()], &to_rgba(&image).unwrap());
    }

    #[test]
    fn validation_rejects_short_buffers_and_invalid_geometry() {
        let short = Image { width: 2, height: 2, stride: 8, format: Format::Xrgb8888, data: vec![0; 15] };
        let zero_width = Image { width: 0, height: 1, stride: 4, format: Format::Xrgb8888, data: vec![0; 4] };
        let short_stride = Image { width: 2, height: 1, stride: 4, format: Format::Xrgb8888, data: vec![0; 4] };

        assert!(validate_image(&short).is_err());
        assert!(validate_image(&zero_width).is_err());
        assert!(validate_image(&short_stride).is_err());
    }

    #[test]
    fn composite_captured_region_blits_subregion_to_destination() {
        let source = CapturedOutput {
            output_name: 7,
            image: Image {
                width: 3,
                height: 2,
                stride: 12,
                format: Format::Xrgb8888,
                data: vec![
                    1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255,
                    4, 4, 4, 255, 5, 5, 5, 255, 6, 6, 6, 255,
                ],
            },
        };
        let region = CaptureOutputRegion { output_name: 7, region: Region { x: 1, y: 0, width: 2, height: 2 }, dst_x: 1, dst_y: 1 };

        let image = composite_captured_region(&[region], 4, 4, &[source]).unwrap();

        let mut expected = vec![0; 4 * 4 * 4];
        expected[20..28].copy_from_slice(&[2, 2, 2, 255, 3, 3, 3, 255]);
        expected[36..44].copy_from_slice(&[5, 5, 5, 255, 6, 6, 6, 255]);
        assert_eq!(image.data, expected);
    }
}
