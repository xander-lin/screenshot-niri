use std::error::Error;

use wayland_client::protocol::wl_shm::Format;

use crate::image::Image;

use super::rgb::{validate_rgb_frame, RgbSource};
use super::{ComposeCrop, FixedBands, FrameMatch, RgbFrame, StitchedFrame, ViewportRect};

impl StitchedFrame {
    #[allow(dead_code)]
    pub fn from_first_frame(frame: &RgbFrame) -> Self {
        let fixed_bands = FixedBands::default();
        let active_crop = fixed_bands
            .active_crop(frame.width, frame.height)
            .expect("valid first frame layout");
        Self::from_first_frame_with_layout(frame, active_crop, fixed_bands)
    }

    pub fn from_first_frame_with_layout(
        frame: &RgbFrame,
        active_crop: ComposeCrop,
        fixed_bands: FixedBands,
    ) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            stride: frame.width * 3,
            data: frame.data.clone(),
            current_origin_x: 0,
            current_origin_y: 0,
            anchor_origin_x: 0,
            anchor_origin_y: 0,
            compose_width: frame.width,
            compose_height: frame.height,
            active_crop,
            fixed_bands,
        }
    }
}

pub fn image_from_stitched_frame(frame: &StitchedFrame) -> Result<Image, Box<dyn Error>> {
    validate_stitched_frame(frame)?;
    let stride = frame.width.checked_mul(4).ok_or("output stride overflow")?;
    let size = (stride as usize)
        .checked_mul(frame.height as usize)
        .ok_or("output image size overflow")?;
    let mut data = vec![0; size];
    for y in 0..frame.height as usize {
        let src_row = y * frame.stride as usize;
        let dst_row = y * stride as usize;
        for x in 0..frame.width as usize {
            let src = src_row + x * 3;
            let dst = dst_row + x * 4;
            data[dst] = frame.data[src + 2];
            data[dst + 1] = frame.data[src + 1];
            data[dst + 2] = frame.data[src];
            data[dst + 3] = 255;
        }
    }
    Ok(Image {
        width: frame.width,
        height: frame.height,
        stride,
        format: Format::Xrgb8888,
        data,
    })
}

pub(super) fn append_frame_at_position(
    stitched: &mut StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    match_info: FrameMatch,
) -> Result<ViewportRect, Box<dyn Error>> {
    append_frame_at_position_with_overlap_mode(
        stitched,
        previous_rect,
        frame,
        match_info,
        OverlapWriteMode::Preserve,
    )
}

pub(super) fn append_frame_at_position_overwriting_overlap_rows(
    stitched: &mut StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    match_info: FrameMatch,
    overwrite_frame_y: u32,
    overwrite_rows: u32,
) -> Result<ViewportRect, Box<dyn Error>> {
    append_frame_at_position_with_overlap_mode(
        stitched,
        previous_rect,
        frame,
        match_info,
        OverlapWriteMode::Rows {
            frame_y: overwrite_frame_y,
            rows: overwrite_rows,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlapWriteMode {
    Preserve,
    Rows { frame_y: u32, rows: u32 },
}

fn append_frame_at_position_with_overlap_mode(
    stitched: &mut StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    match_info: FrameMatch,
    overlap_write_mode: OverlapWriteMode,
) -> Result<ViewportRect, Box<dyn Error>> {
    validate_stitched_frame(stitched)?;
    validate_rgb_frame(frame)?;
    if stitched.width > i32::MAX as u32
        || stitched.height > i32::MAX as u32
        || frame.width > i32::MAX as u32
        || frame.height > i32::MAX as u32
    {
        return Err("frame dimensions exceed supported coordinate range".into());
    }
    let frame_x = previous_rect
        .x
        .checked_add(match_info.delta_x)
        .ok_or("frame x overflow")?;
    let frame_y = previous_rect
        .y
        .checked_add(match_info.delta_y)
        .ok_or("frame y overflow")?;
    let min_x = 0.min(frame_x);
    let min_y = 0.min(frame_y);
    let max_x = (stitched.width as i32).max(
        frame_x
            .checked_add(frame.width as i32)
            .ok_or("frame max x overflow")?,
    );
    let max_y = (stitched.height as i32).max(
        frame_y
            .checked_add(frame.height as i32)
            .ok_or("frame max y overflow")?,
    );
    let output_width = u32::try_from(max_x - min_x)?;
    let output_height = u32::try_from(max_y - min_y)?;
    let output_stride = output_width
        .checked_mul(3)
        .ok_or("stitched stride overflow")?;
    let output_size = (output_stride as usize)
        .checked_mul(output_height as usize)
        .ok_or("stitched size overflow")?;
    let mut data = vec![0; output_size];

    let stitched_dst_x = (-min_x) as u32;
    let stitched_dst_y = (-min_y) as u32;
    let frame_dst_x = (frame_x - min_x) as u32;
    let frame_dst_y = (frame_y - min_y) as u32;
    blit_rgb(
        &stitched.data,
        stitched.width,
        stitched.height,
        stitched.stride,
        &mut data,
        output_stride,
        stitched_dst_x,
        stitched_dst_y,
    );
    let overlap_rect = intersect_rects(
        stitched_dst_x,
        stitched_dst_y,
        stitched.width,
        stitched.height,
        frame_dst_x,
        frame_dst_y,
        frame.width,
        frame.height,
    );
    match overlap_write_mode {
        OverlapWriteMode::Preserve | OverlapWriteMode::Rows { .. } => {
            blit_rgb_excluding_rect(
                &frame.data,
                frame.width,
                frame.height,
                frame.stride,
                &mut data,
                output_stride,
                frame_dst_x,
                frame_dst_y,
                overlap_rect,
            );
        }
    }
    if let OverlapWriteMode::Rows { frame_y, rows } = overlap_write_mode {
        blit_rgb_rows_in_rect(
            &frame.data,
            frame.width,
            frame.height,
            frame.stride,
            &mut data,
            output_stride,
            frame_dst_x,
            frame_dst_y,
            frame_y,
            rows,
            overlap_rect,
        );
    }

    stitched.width = output_width;
    stitched.height = output_height;
    stitched.stride = output_stride;
    stitched.data = data;
    stitched.current_origin_x = frame_x - min_x;
    stitched.current_origin_y = frame_y - min_y;
    stitched.anchor_origin_x -= min_x;
    stitched.anchor_origin_y -= min_y;
    Ok(ViewportRect {
        x: frame_x - min_x,
        y: frame_y - min_y,
        width: frame.width,
        height: frame.height,
    })
}

fn blit_rgb_rows_in_rect(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
    dst_stride: u32,
    dst_x: u32,
    dst_y: u32,
    start_y: u32,
    rows: u32,
    include: Option<(u32, u32, u32, u32)>,
) {
    let Some((x0, y0, x1, y1)) = include else {
        return;
    };
    let end_y = start_y.saturating_add(rows).min(height);
    for y in start_y..end_y {
        for x in 0..width {
            let out_x = dst_x + x;
            let out_y = dst_y + y;
            if out_x < x0 || out_x >= x1 || out_y < y0 || out_y >= y1 {
                continue;
            }
            let src_start = y as usize * stride as usize + x as usize * 3;
            let dst_start = out_y as usize * dst_stride as usize + out_x as usize * 3;
            dst[dst_start..dst_start + 3].copy_from_slice(&src[src_start..src_start + 3]);
        }
    }
}

pub(super) fn translate_viewport_rect(
    rect: ViewportRect,
    match_info: FrameMatch,
) -> Result<Option<ViewportRect>, Box<dyn Error>> {
    let x = rect
        .x
        .checked_add(match_info.delta_x)
        .ok_or("viewport x overflow")?;
    let y = rect
        .y
        .checked_add(match_info.delta_y)
        .ok_or("viewport y overflow")?;
    Ok(Some(ViewportRect {
        x,
        y,
        width: rect.width,
        height: rect.height,
    }))
}

pub(super) fn viewport_rect_within_stitched(
    stitched: &StitchedFrame,
    rect: ViewportRect,
) -> Result<bool, Box<dyn Error>> {
    let right = rect
        .x
        .checked_add(i32::try_from(rect.width)?)
        .ok_or("viewport right overflow")?;
    let bottom = rect
        .y
        .checked_add(i32::try_from(rect.height)?)
        .ok_or("viewport bottom overflow")?;
    Ok(rect.x >= 0
        && rect.y >= 0
        && right <= i32::try_from(stitched.width)?
        && bottom <= i32::try_from(stitched.height)?)
}

pub(super) fn extract_stitched_region(
    stitched: &StitchedFrame,
    rect: ViewportRect,
) -> Result<RgbFrame, Box<dyn Error>> {
    validate_stitched_frame(stitched)?;
    let x = u32::try_from(rect.x)?;
    let y = u32::try_from(rect.y)?;
    if x.checked_add(rect.width).ok_or("viewport x overflow")? > stitched.width
        || y.checked_add(rect.height).ok_or("viewport y overflow")? > stitched.height
    {
        return Err("viewport rectangle exceeds stitched frame".into());
    }
    let stride = rect
        .width
        .checked_mul(3)
        .ok_or("viewport stride overflow")?;
    let size = (stride as usize)
        .checked_mul(rect.height as usize)
        .ok_or("viewport size overflow")?;
    let mut data = vec![0; size];
    for row in 0..rect.height as usize {
        let src_row = (y as usize + row) * stitched.stride as usize + x as usize * 3;
        let dst_row = row * stride as usize;
        data[dst_row..dst_row + stride as usize]
            .copy_from_slice(&stitched.data[src_row..src_row + stride as usize]);
    }
    Ok(RgbFrame {
        width: rect.width,
        height: rect.height,
        stride,
        data,
    })
}

pub(super) fn canvas_overlap_rect(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    frame_x: i32,
    frame_y: i32,
) -> Option<(u32, u32, u32, u32)> {
    let frame_right = frame_x.checked_add(frame.width() as i32)?;
    let frame_bottom = frame_y.checked_add(frame.height() as i32)?;
    let x0 = 0.max(frame_x);
    let y0 = 0.max(frame_y);
    let x1 = (stitched.width as i32).min(frame_right);
    let y1 = (stitched.height as i32).min(frame_bottom);
    (x0 < x1 && y0 < y1).then_some((
        u32::try_from(x0).ok()?,
        u32::try_from(y0).ok()?,
        u32::try_from(x1).ok()?,
        u32::try_from(y1).ok()?,
    ))
}

fn blit_rgb(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
    dst_stride: u32,
    dst_x: u32,
    dst_y: u32,
) {
    let row_len = width as usize * 3;
    for y in 0..height as usize {
        let src_start = y * stride as usize;
        let dst_start = (dst_y as usize + y) * dst_stride as usize + dst_x as usize * 3;
        dst[dst_start..dst_start + row_len].copy_from_slice(&src[src_start..src_start + row_len]);
    }
}

fn blit_rgb_excluding_rect(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
    dst_stride: u32,
    dst_x: u32,
    dst_y: u32,
    exclude: Option<(u32, u32, u32, u32)>,
) {
    for y in 0..height {
        for x in 0..width {
            let out_x = dst_x + x;
            let out_y = dst_y + y;
            if exclude.is_some_and(|(x0, y0, x1, y1)| {
                out_x >= x0 && out_x < x1 && out_y >= y0 && out_y < y1
            }) {
                continue;
            }
            let src_start = y as usize * stride as usize + x as usize * 3;
            let dst_start = out_y as usize * dst_stride as usize + out_x as usize * 3;
            dst[dst_start..dst_start + 3].copy_from_slice(&src[src_start..src_start + 3]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn intersect_rects(
    ax: u32,
    ay: u32,
    aw: u32,
    ah: u32,
    bx: u32,
    by: u32,
    bw: u32,
    bh: u32,
) -> Option<(u32, u32, u32, u32)> {
    let x0 = ax.max(bx);
    let y0 = ay.max(by);
    let x1 = ax.checked_add(aw)?.min(bx.checked_add(bw)?);
    let y1 = ay.checked_add(ah)?.min(by.checked_add(bh)?);
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

pub(super) fn validate_stitched_frame(frame: &StitchedFrame) -> Result<(), Box<dyn Error>> {
    if frame.width == 0 || frame.height == 0 || frame.stride < frame.width.saturating_mul(3) {
        return Err("invalid stitched frame geometry".into());
    }
    let required = (frame.stride as usize)
        .checked_mul(frame.height as usize)
        .ok_or("stitched frame size overflow")?;
    if frame.data.len() < required {
        return Err("stitched frame data too short".into());
    }
    Ok(())
}
