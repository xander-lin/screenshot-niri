use std::error::Error;

use wayland_client::protocol::wl_shm::Format;

use crate::image::Image;

use super::rgb::{crop_rgb_frame, validate_rgb_frame, RgbSource};
use super::store::CanvasStore;
use super::{
    ComposeCrop, FixedBands, FrameMatch, RgbFrame, RowCoverage, StitchedFrame, ViewportRect,
};

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
            store: first_frame_store(frame),
            current_origin_x: 0,
            current_origin_y: 0,
            anchor_origin_x: 0,
            anchor_origin_y: 0,
            compose_width: frame.width,
            compose_height: frame.height,
            active_crop,
            fixed_bands,
            row_coverage: vec![RowCoverage::Full; frame.height as usize],
        }
    }

    /// Whether the canvas actually captured this pixel.
    ///
    /// The coverage map is `Full` on every row of a rectangular canvas, so this
    /// is the cheap common case. On a ragged one it is what separates content
    /// from the zero bytes of a region that was never captured.
    #[allow(dead_code)] // used by the legacy scan (feature-gated) and by tests
    pub fn captured(&self, x: u32, y: u32) -> bool {
        match self.row_coverage.get(y as usize) {
            Some(row) => row.contains(x),
            // An empty map is a rectangular canvas: every pixel is captured.
            None => self.row_coverage.is_empty(),
        }
    }
}

/// Inserts `[start, end)` into a sorted, disjoint interval list, merging any
/// overlapping *or adjacent* intervals.
///
/// Merging adjacency is what lets "is this interval covered" be answered by a
/// single containment test: a contiguous captured run is always one entry.
pub(super) fn merge_interval(intervals: &mut Vec<(u32, u32)>, start: u32, end: u32) {
    if end <= start {
        return;
    }
    let mut start = start;
    let mut end = end;
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(intervals.len() + 1);
    let mut inserted = false;
    for &(s, e) in intervals.iter() {
        if e < start {
            merged.push((s, e));
        } else if s > end {
            if !inserted {
                merged.push((start, end));
                inserted = true;
            }
            merged.push((s, e));
        } else {
            start = start.min(s);
            end = end.max(e);
        }
    }
    if !inserted {
        merged.push((start, end));
    }
    *intervals = merged;
}

/// Parts of `[start, end)` that `covered` (sorted, disjoint) does not cover.
fn uncovered_spans(start: u32, end: u32, covered: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut spans = Vec::new();
    let mut cursor = start;
    for &(s, e) in covered {
        if e <= cursor {
            continue;
        }
        if s >= end {
            break;
        }
        if s > cursor {
            spans.push((cursor, s.min(end)));
        }
        cursor = cursor.max(e);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        spans.push((cursor, end));
    }
    spans
}

/// The part of a canvas the overlay can actually display.
///
/// A live preview used to be the whole canvas as RGBA: 22 MB at the end of
/// `real-1` and 45 MB on `real-long`, rebuilt on every accepted frame, while the
/// overlay samples at most one screenful of it (everything outside the selection
/// box, mapped through the capture's own density). This is that screenful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasWindow {
    /// Canvas pixel of the window's own pixel (0, 0).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl CanvasWindow {
    /// The whole canvas: what a caller that needs every pixel wants.
    pub fn full(frame: &StitchedFrame) -> Self {
        Self { x: 0, y: 0, width: frame.width, height: frame.height }
    }

    /// Clips `(x, y, width, height)` to the canvas, returning `None` when it
    /// misses it entirely.
    pub fn clipped(frame: &StitchedFrame, x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x.saturating_add(width)).min(frame.width as i32);
        let y1 = (y.saturating_add(height)).min(frame.height as i32);
        (x0 < x1 && y0 < y1).then_some(Self {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }
}

pub fn image_from_stitched_frame(frame: &StitchedFrame) -> Result<Image, Box<dyn Error>> {
    Ok(image_from_stitched_window_reusing(frame, CanvasWindow::full(frame), Vec::new())?.image)
}

/// A window of the canvas as RGBA, plus whether it is fully opaque.
///
/// `opaque` is exact, not a guess: it is true only when every pixel of the
/// window was captured, which is what lets the overlay copy contiguous runs
/// instead of testing each pixel's alpha. It is false for a ragged (T/L/arch)
/// canvas and for a window that includes rows the capture never wrote.
pub struct WindowImage {
    pub image: Image,
    pub opaque: bool,
}

/// One region of the canvas, as RGBA.
///
/// Reuses `buffer`'s allocation when it is large enough.
///
/// The overlay asks for one window per accepted frame and the window is almost
/// always the same size, so a caller that keeps the previous image's buffer
/// turns an 8.3 MB allocation (1080p) into a no-op after the first frame. The
/// fresh pages were also the largest remaining source of page faults in a long
/// capture.
pub fn image_from_stitched_window_reusing(
    frame: &StitchedFrame,
    window: CanvasWindow,
    mut buffer: Vec<u8>,
) -> Result<WindowImage, Box<dyn Error>> {
    if window.width == 0 || window.height == 0 {
        return Err("empty canvas window".into());
    }
    if window.x < 0
        || window.y < 0
        || window.x as u32 + window.width > frame.width
        || window.y as u32 + window.height > frame.height
    {
        return Err("canvas window exceeds the canvas".into());
    }
    validate_stitched_frame(frame)?;
    let stride = window.width.checked_mul(4).ok_or("output stride overflow")?;
    let size = (stride as usize)
        .checked_mul(window.height as usize)
        .ok_or("output image size overflow")?;
    // The caller's buffer is a high-water mark, not an exact fit: the overlay's
    // window grows as the canvas grows and shrinks when a selection-box edit
    // narrows it, and reallocating on every change would defeat the point.
    // Only the first `size` bytes are used; the rest stays allocated for the
    // next frame.
    if buffer.len() < size {
        buffer.resize(size, 0);
    }
    let mut data = buffer;
    // One row at a time: a per-pixel accessor would put a borrow check and a
    // working-set lookup on every one of them (measured: 3× the cost of the
    // whole rest of the frame).
    let mut source = vec![0u8; window.width as usize * 3];
    let mut opaque = true;
    for row in 0..window.height {
        let canvas_y = window.y as u32 + row;
        let dst_row = row as usize * stride as usize;
        let covered = frame.row_coverage.get(canvas_y as usize);
        let have_row = frame
            .store
            .row_into(window.x as u32, canvas_y as i32, &mut source);
        if !have_row {
            // Nothing was ever written here; the row stays transparent. The
            // buffer is recycled, so "stays" has to be written down: without
            // this the row would show the previous frame's pixels, and the
            // overlay paints any pixel with a non-zero alpha.
            data[dst_row..dst_row + stride as usize].fill(0);
            opaque = false;
            continue;
        }
        // Coverage is decided per row, not per pixel. The common canvas is
        // rectangular and every row is `Full`, so this branch settles the whole
        // row at once instead of asking `contains` 1920 times — the per-pixel
        // form was 18% of a 1080p frame's cost.
        let first = window.x as u32;
        let last = first + window.width;
        let whole_row = match covered {
            None | Some(RowCoverage::Full) => true,
            Some(RowCoverage::Empty) => false,
            Some(RowCoverage::Spans(_)) => covered.is_some_and(|row| row.covers(first, last)),
        };
        if whole_row {
            // Whole row visible: one pass, channel-swapped and opaque.
            for (source_pixel, destination) in source
                .as_chunks::<3>()
                .0
                .iter()
                .zip(data[dst_row..dst_row + stride as usize].as_chunks_mut::<4>().0.iter_mut())
            {
                destination[0] = source_pixel[2];
                destination[1] = source_pixel[1];
                destination[2] = source_pixel[0];
                destination[3] = 255;
            }
            continue;
        }
        // Ragged row (T/L/arch shapes): only the captured extents are opaque,
        // so the gaps stay transparent rather than being filled with black.
        // Clearing first is what makes that true for a recycled buffer.
        data[dst_row..dst_row + stride as usize].fill(0);
        opaque = false;
        for column in 0..window.width {
            let filled = covered.is_none_or(|row| row.contains(first + column));
            if !filled {
                continue;
            }
            let dst = dst_row + column as usize * 4;
            let src = column as usize * 3;
            data[dst] = source[src + 2];
            data[dst + 1] = source[src + 1];
            data[dst + 2] = source[src];
            data[dst + 3] = 255;
        }
    }
    Ok(WindowImage {
        image: Image {
            width: window.width,
            height: window.height,
            stride,
            format: Format::Argb8888,
            data,
        },
        opaque,
    })
}

/// `crop_rgb_frame` unless the crop is the whole frame, in which case the
/// caller can keep borrowing the original.
///
/// The shipped fixed-band detector never crops (it is trace-only, so its
/// `top`/`bottom` stay zero), which made the unconditional copy a full-frame
/// duplicate: 6.2 MB of allocation and a memcpy per push on 1080p.
pub(super) fn crop_rgb_frame_if_needed(
    frame: &RgbFrame,
    crop: ComposeCrop,
) -> Result<Option<RgbFrame>, Box<dyn Error>> {
    if crop.x == 0 && crop.y == 0 && crop.width == frame.width && crop.height == frame.height {
        return Ok(None);
    }
    crop_rgb_frame(frame, crop).map(Some)
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

    let stitched_dst_x = (-min_x) as u32;
    let stitched_dst_y = (-min_y) as u32;
    let frame_dst_x = (frame_x - min_x) as u32;
    let frame_dst_y = (frame_y - min_y) as u32;
    // Fast path for the overwhelmingly common frame: a pure vertical scroll,
    // where the canvas keeps its width and x offset and the frame is full
    // width. Coverage then only moves vertically, so it is copied instead of
    // rebuilt row by row (the general path below costs ~0.1 ms per accepted
    // frame on an 8 500-row canvas).
    let vertical_only = !stitched.row_coverage.is_empty()
        && stitched_dst_x == 0
        && output_width == stitched.width
        && frame_dst_x == 0
        && frame.width == output_width;
    let mut row_coverage: Vec<RowCoverage> = if vertical_only {
        let mut coverage = vec![RowCoverage::Empty; output_height as usize];
        let dst = stitched_dst_y as usize;
        let rows = (stitched.height as usize).min(output_height as usize - dst);
        coverage[dst..dst + rows].clone_from_slice(&stitched.row_coverage[..rows]);
        let frame_lo = frame_dst_y as usize;
        let frame_hi = (frame_dst_y as usize + frame.height as usize).min(output_height as usize);
        for row in coverage.iter_mut().take(frame_hi).skip(frame_lo) {
            *row = RowCoverage::Full;
        }
        coverage
    } else {
        vec![RowCoverage::Empty; output_height as usize]
    };
    let mut spans: Vec<(u32, u32)> = Vec::with_capacity(4);
    #[allow(clippy::needless_range_loop)] // `row` is also the canvas row
    for row in 0..output_height as usize {
        if vertical_only {
            break;
        }

        spans.clear();
        // The old row index, not the new one: the canvas may have moved down
        // (prepend) or right (selection moved left) as well as grown.
        let old_row = row.checked_sub(stitched_dst_y as usize);
        match old_row.and_then(|old| stitched.row_coverage.get(old)) {
            Some(existing) => existing.push_into(
                &mut spans,
                stitched_dst_x,
                stitched.width,
                output_width,
            ),
            None if stitched.row_coverage.is_empty() => {
                // Empty map: the old canvas was rectangular, so every row it
                // held was fully covered.
                if old_row.is_some_and(|old| old < stitched.height as usize) {
                    merge_interval(
                        &mut spans,
                        stitched_dst_x.min(output_width),
                        stitched_dst_x.saturating_add(stitched.width).min(output_width),
                    );
                }
            }
            None => {}
        }
        // The frame's own extent on this row.
        if row >= frame_dst_y as usize && row < frame_dst_y as usize + frame.height as usize {
            let frame_end = frame_dst_x.saturating_add(frame.width).min(output_width);
            merge_interval(&mut spans, frame_dst_x, frame_end);
        }
        row_coverage[row] = RowCoverage::from_spans(&spans, output_width);
    }

    // The canvas now grows in place instead of being rebuilt.
    //
    // Growing *downward* appends bands; growing *upward* relabels the whole
    // grid by `stitched_dst_y` rows, which moves band headers rather than
    // pixels. A width change is the one case that cannot be done in place: the
    // row length changes, so every band is re-cut — and that is rare (a
    // horizontal step or a selection-box width edit), not the per-frame cost it
    // used to be.
    if output_width == stitched.width {
        if stitched_dst_y > 0 {
            stitched.store.relabel(stitched_dst_y as i32);
        }
        stitched.store.ensure_top(0);
        stitched.store.ensure_bottom(output_height as i32);
    } else {
        reflow_store(
            stitched,
            output_width,
            output_stride,
            output_height,
            stitched_dst_x,
            stitched_dst_y,
        );
    }

    // The frame's own rows, row by row, written only where the canvas has no
    // pixels yet — or over the proven-overlap rows of a recovery.
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
    let mut covered: Vec<(u32, u32)> = Vec::with_capacity(4);
    let mut targets: Vec<(u32, u32)> = Vec::with_capacity(4);
    for y in 0..frame.height {
        let dst_y = frame_dst_y + y;
        // Two independent reasons to write a span of this row, and a recovery
        // frame can need both at once: the canvas has no pixels there yet, or
        // the frame proved the pixels it has are stale.
        covered.clear();
        let old_row = dst_y.checked_sub(stitched_dst_y);
        match old_row.and_then(|row| stitched.row_coverage.get(row as usize)) {
            Some(existing) => existing.push_into(
                &mut covered,
                stitched_dst_x,
                stitched.width,
                output_width,
            ),
            None if stitched.row_coverage.is_empty() => {
                // Empty map: the old canvas was rectangular, so every row it
                // held was fully covered.
                if old_row.is_some_and(|row| row < stitched.height) {
                    merge_interval(
                        &mut covered,
                        stitched_dst_x.min(output_width),
                        stitched_dst_x
                            .saturating_add(stitched.width)
                            .min(output_width),
                    );
                }
            }
            None => {}
        }
        // Preserve captured pixels, not the entire canvas bounding box.
        // Widening after a narrow scroll must fill previously transparent
        // strips even though they lie inside that bounding box, and the frame
        // is written only where the canvas has no pixels yet — which on a
        // ragged canvas is a *set* of strips per row, not one.
        let frame_end = frame_dst_x + frame.width;
        targets.clear();
        for (start, end) in uncovered_spans(frame_dst_x, frame_end, &covered) {
            if start < end {
                targets.push((start, end));
            }
        }
        if let OverlapWriteMode::Rows { frame_y, rows } = overlap_write_mode {
            // A recovery also rewrites the rows whose content it proved stale,
            // inside the overlap it validated.
            if y >= frame_y && y < frame_y.saturating_add(rows) {
                if let Some((x0, y0, x1, y1)) = overlap_rect {
                    if dst_y >= y0 && dst_y < y1 {
                        let start = frame_dst_x.max(x0);
                        let end = frame_end.min(x1);
                        if start < end {
                            merge_interval(&mut targets, start, end);
                        }
                    }
                }
            }
        }
        if targets.is_empty() {
            continue;
        }
        stitched
            .store
            .write_spans(dst_y as i32, &targets, &frame.data, frame.stride, frame_dst_x, y);
    }

    stitched.width = output_width;
    stitched.height = output_height;
    stitched.stride = output_stride;
    stitched.row_coverage = row_coverage;
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

/// Re-cuts every band for a canvas whose *width* changed.
///
/// The only case the store cannot absorb in place: a tile is a run of rows at a
/// fixed row length, so a new width means new tiles. It happens on a horizontal
/// step and on a selection-box width edit, not on the vertical scrolls that
/// make up almost all of a long capture.
fn reflow_store(
    stitched: &mut StitchedFrame,
    width: u32,
    stride: u32,
    height: u32,
    dst_x: u32,
    dst_y: u32,
) {
    let old_width = stitched.width;
    let old_height = stitched.height;
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(old_height as usize);
    let mut row = vec![0u8; old_width as usize * 3];
    for y in 0..old_height as i32 {
        if stitched.store.row_into(0, y, &mut row) {
            rows.push(row.clone());
        } else {
            rows.push(Vec::new());
        }
    }
    let origin = stitched.store.origin_row() + dst_y as i32;
    let store = &mut stitched.store;
    store.rebuild(width, stride, origin);
    store.ensure_top(0);
    store.ensure_bottom(height as i32);
    for (y, bytes) in rows.iter().enumerate() {
        if bytes.is_empty() {
            continue;
        }
        // `src_x` is the canvas column the copy's own column 0 lands in: the
        // canvas may have grown leftward, so the old rows start at `dst_x`, not
        // at 0.
        store.write_spans(
            y as i32 + dst_y as i32,
            &[(dst_x, dst_x + old_width)],
            bytes,
            old_width * 3,
            dst_x,
            0,
        );
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

/// Whether every pixel of `rect` has already been captured, not merely whether
/// the rect fits inside the canvas bounding box.
///
/// The two are the same only for a rectangular canvas. A ragged one — after a
/// selection resize or move — has never-captured regions *inside* its bounding
/// box: the leg of an L, the space between the legs of an arch. Treating those
/// as captured makes the caller skip the append, so the frame that would fill
/// them is dropped while still being reported as accepted, and every later
/// frame then fails the colour check because the canvas there is still empty.
pub(super) fn viewport_rect_covered_by_stitched(
    stitched: &StitchedFrame,
    rect: ViewportRect,
) -> Result<bool, Box<dyn Error>> {
    if !viewport_rect_within_stitched(stitched, rect)? {
        return Ok(false);
    }
    if stitched.row_coverage.is_empty() {
        // An empty coverage map means every row is fully covered.
        return Ok(true);
    }
    let x = u32::try_from(rect.x)?;
    let right = x + rect.width;
    let first_row = u32::try_from(rect.y)?;
    for row in first_row..first_row + rect.height {
        match stitched.row_coverage.get(row as usize) {
            Some(coverage) if coverage.covers(x, right) => {}
            Some(_) => return Ok(false),
            // No per-row map: rectangular canvas, every row covered.
            None if stitched.row_coverage.is_empty() => {}
            None => return Ok(false),
        }
    }
    Ok(true)
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
    if !stitched
        .store
        .region_into(x, y as i32, rect.width, rect.height, &mut data, stride)
    {
        return Err("stitched canvas is missing a row the viewport covers".into());
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
    if frame.store.width() != frame.width || frame.store.stride() != frame.stride {
        return Err("stitched canvas store does not match the frame geometry".into());
    }
    // The store's grid is cut in whole bands and only ever grows, so it may
    // legitimately extend past the frame's height; it must never fall short of
    // row 0. The rows *below* the height are simply never written, which is what
    // `is_written` reports.
    if frame.store.grid_end_row() < frame.height as i32 {
        return Err("stitched canvas store is shorter than the frame".into());
    }
    Ok(())
}

/// A store holding one frame, which is what a fresh canvas is.
fn first_frame_store(frame: &RgbFrame) -> CanvasStore {
    let frame_bytes = frame.stride as usize * frame.height as usize;
    let mut store = CanvasStore::new(
        frame.width,
        frame.width * 3,
        0,
        CanvasStore::default_budget(frame_bytes),
    );
    store.ensure_bottom(frame.height as i32);
    let spans = [(0u32, frame.width)];
    for y in 0..frame.height {
        store.write_spans(y as i32, &spans, &frame.data, frame.stride, 0, y);
    }
    store
}
