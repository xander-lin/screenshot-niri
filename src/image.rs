use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;

use wayland_client::protocol::wl_shm::Format;

use crate::wayland::screencopy::{CapturedOutput, CaptureOutputRegion};

#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
    pub data: Vec<u8>,
}

pub fn write_png(path: &Path, image: &Image) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&to_rgba(image)?)?;
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

fn to_rgba(image: &Image) -> Result<Vec<u8>, Box<dyn Error>> {
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
