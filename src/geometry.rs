use crate::wayland::screencopy::{CaptureOutputRegion, CapturedOutput, Region};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputInfo {
    pub global_name: u32,
    pub logical: LogicalRect,
    pub scale: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedViewport {
    pub rect: LogicalRect,
    pub regions: Vec<SelectedOutputRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedOutputRegion {
    pub output_global_name: u32,
    pub output_logical: LogicalRect,
    pub global_region: LogicalRect,
    pub local_region: LogicalRect,
}

impl LogicalRect {
    pub fn from_points(ax: i32, ay: i32, bx: i32, by: i32) -> Self {
        let x = ax.min(bx);
        let y = ay.min(by);
        Self {
            x,
            y,
            width: ax.max(bx) - x + 1,
            height: ay.max(by) - y + 1,
        }
    }

    pub fn right(self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub fn is_empty(self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());
        let width = x2 - x1;
        let height = y2 - y1;
        (width > 0 && height > 0).then_some(Self { x: x1, y: y1, width, height })
    }

    pub fn area(self) -> i64 {
        i64::from(self.width.max(0)) * i64::from(self.height.max(0))
    }
}

/// Whether a selection rectangle overlaps more than one output. Long capture
/// works on a single output, so a spanning selection must be clamped first.
pub fn rect_spans_outputs(rect: LogicalRect, outputs: &[OutputInfo]) -> bool {
    outputs.iter().filter(|output| rect.intersection(output.logical).is_some()).count() > 1
}

/// Largest single-output piece of a possibly spanning selection: the
/// intersection with the output that covers the most of it.
pub fn clamp_rect_to_largest_output(rect: LogicalRect, outputs: &[OutputInfo]) -> LogicalRect {
    outputs
        .iter()
        .filter_map(|output| rect.intersection(output.logical))
        .max_by_key(|intersection| intersection.area())
        .unwrap_or(rect)
}

/// Clamps a spanning selection to the output that contains the point where
/// the drag started: the box belongs to the screen you clicked on, no matter
/// how much of it ends up on another screen. Falls back to the largest
/// piece when the point lies on no output.
pub fn clamp_rect_to_output_at_point(
    rect: LogicalRect,
    outputs: &[OutputInfo],
    x: i32,
    y: i32,
) -> LogicalRect {
    let anchor_output = outputs.iter().find(|output| {
        x >= output.logical.x && x < output.logical.right() && y >= output.logical.y && y < output.logical.bottom()
    });
    match anchor_output {
        Some(output) => rect.intersection(output.logical).unwrap_or(rect),
        None => clamp_rect_to_largest_output(rect, outputs),
    }
}

impl SelectedViewport {
    pub fn from_outputs(rect: LogicalRect, outputs: &[OutputInfo]) -> Result<Self, String> {
        if rect.is_empty() {
            return Err("selected viewport is empty".into());
        }
        let mut regions = Vec::new();
        for output in outputs {
            let Some(global_region) = rect.intersection(output.logical) else {
                continue;
            };
            regions.push(SelectedOutputRegion {
                output_global_name: output.global_name,
                output_logical: output.logical,
                global_region,
                local_region: LogicalRect {
                    x: global_region.x - output.logical.x,
                    y: global_region.y - output.logical.y,
                    width: global_region.width,
                    height: global_region.height,
                },
            });
        }
        if regions.is_empty() {
            return Err("selected viewport does not intersect any output".into());
        }
        Ok(Self { rect, regions })
    }

    /// Pixel rectangles to crop out of the frozen captures.
    ///
    /// The logical-to-pixel factor comes from the capture itself
    /// (`image.width / output logical width`), never from `wl_output.scale`:
    /// that one is an integer, while a 1.5-scaled output delivers buffers at
    /// 1.5x its logical size. Scaling by the integer reported by the
    /// compositor asks the crop for pixels the capture does not have (which is
    /// why a whole-output capture on the scaled screen produced no image at
    /// all) or silently crops the wrong rectangle.
    pub fn capture_regions(&self, captures: &[CapturedOutput]) -> Vec<CaptureOutputRegion> {
        self.regions
            .iter()
            .map(|region| {
                let (scale_x, inv_x) = capture_pixel_scale(region, captures, true);
                let (scale_y, inv_y) = capture_pixel_scale(region, captures, false);
                let right = region.local_region.right();
                let bottom = region.local_region.bottom();
                CaptureOutputRegion {
                    output_name: region.output_global_name,
                    region: Region {
                        x: scale_logical(region.local_region.x, scale_x, inv_x),
                        y: scale_logical(region.local_region.y, scale_y, inv_y),
                        // Edges, not widths: two adjacent pieces then share one
                        // boundary even when the factor is fractional.
                        width: scale_logical(right, scale_x, inv_x)
                            - scale_logical(region.local_region.x, scale_x, inv_x),
                        height: scale_logical(bottom, scale_y, inv_y)
                            - scale_logical(region.local_region.y, scale_y, inv_y),
                    },
                    dst_x: self.scaled_axis_offset(region.global_region.x, true, captures),
                    dst_y: self.scaled_axis_offset(region.global_region.y, false, captures),
                }
            })
            .collect()
    }

    pub fn capture_size(&self, captures: &[CapturedOutput]) -> Result<(u32, u32), String> {
        let mut width = 0u32;
        let mut height = 0u32;
        for region in self.capture_regions(captures) {
            let region_width = u32::try_from(region.region.width).map_err(|_| "capture region has negative width")?;
            let region_height = u32::try_from(region.region.height).map_err(|_| "capture region has negative height")?;
            width = width.max(region.dst_x.checked_add(region_width).ok_or("capture width overflow")?);
            height = height.max(region.dst_y.checked_add(region_height).ok_or("capture height overflow")?);
        }
        if width == 0 || height == 0 {
            Err("selected viewport has no capture size".into())
        } else {
            Ok((width, height))
        }
    }

    fn scaled_axis_offset(&self, target: i32, horizontal: bool, captures: &[CapturedOutput]) -> u32 {
        let viewport_start = if horizontal { self.rect.x } else { self.rect.y };
        let mut intervals: Vec<_> = self
            .regions
            .iter()
            .map(|region| {
                let rect = region.global_region;
                let start = if horizontal { rect.x } else { rect.y };
                let end = if horizontal { rect.right() } else { rect.bottom() };
                // The ratio is defined on this output's own local frame, so the
                // span must be anchored at the output's origin: mapping the
                // global coordinate directly adds the phase of
                // `origin * ratio`, which can round one pixel away from the crop
                // width the same piece was written with.
                let origin = if horizontal { region.output_logical.x } else { region.output_logical.y };
                let (scale, inv) = capture_pixel_scale(region, captures, horizontal);
                (start, end, origin, scale, inv)
            })
            .collect();
        intervals.sort_by_key(|item| (item.0, item.1));

        let mut offset = 0u32;
        let mut covered_until = viewport_start;
        for (start, end, origin, scale, inv) in intervals {
            if end <= covered_until || start >= target {
                continue;
            }
            let segment_start = start.max(covered_until).max(viewport_start);
            let segment_end = end.min(target);
            if segment_end > segment_start {
                // Map both edges in the output's local frame and subtract: the
                // same rule as the source rectangles, so every piece's
                // destination offset equals the widths already written.
                let span = scale_logical(segment_end - origin, scale, inv)
                    - scale_logical(segment_start - origin, scale, inv);
                offset = offset.saturating_add(u32::try_from(span).unwrap_or(0));
                covered_until = segment_end;
            }
        }
        offset
    }
}

/// Pixels per logical unit on one output, as `(numerator, denominator)` taken
/// from the frozen capture for that output.
fn capture_pixel_scale(region: &SelectedOutputRegion, captures: &[CapturedOutput], horizontal: bool) -> (i32, i32) {
    let Some(capture) = captures.iter().find(|capture| capture.output_name == region.output_global_name) else {
        return (1, 1);
    };
    let (pixels, logical) = if horizontal {
        (capture.image.width, region.output_logical.width)
    } else {
        (capture.image.height, region.output_logical.height)
    };
    if pixels == 0 || logical <= 0 {
        return (1, 1);
    }
    (i32::try_from(pixels).unwrap_or(1), logical)
}

fn scale_logical(value: i32, numerator: i32, denominator: i32) -> i32 {
    if numerator <= 0 || denominator <= 0 {
        return value;
    }
    (i64::from(value) * i64::from(numerator) / i64::from(denominator)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_selection_across_outputs() {
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 2 },
        ];
        let captures = [captured(1, 100, 80), captured(2, 200, 160)];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 90, y: 10, width: 30, height: 20 }, &outputs).unwrap();
        let regions = viewport.capture_regions(&captures);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].region.width, 10);
        assert_eq!(regions[1].region.width, 40);
        assert_eq!(regions[1].dst_x, 10);
        assert_eq!(viewport.capture_size(&captures).unwrap(), (50, 40));
    }

    #[test]
    fn capture_regions_follow_the_capture_pixel_ratio_on_fractional_scale() {
        // A 100x80 logical output at scale 1.5. Some compositors report
        // wl_output.scale = 2 for it while delivering 150x120 pixel buffers;
        // cropping by the reported integer asks for 200x160 and fails, which is
        // why a whole-output capture on the scaled screen produced no file.
        let outputs = [OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 2 }];
        let captures = [captured(1, 150, 120)];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 0, y: 0, width: 100, height: 80 }, &outputs).unwrap();
        let regions = viewport.capture_regions(&captures);
        assert_eq!((regions[0].region.x, regions[0].region.y), (0, 0));
        assert_eq!((regions[0].region.width, regions[0].region.height), (150, 120));
        assert_eq!(viewport.capture_size(&captures).unwrap(), (150, 120));

        // Half the width in logical units is half the width in pixels.
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 50, y: 0, width: 50, height: 80 }, &outputs).unwrap();
        let regions = viewport.capture_regions(&captures);
        assert_eq!((regions[0].region.x, regions[0].region.width), (75, 75));
    }

    #[test]
    fn mixed_scale_selection_places_pieces_contiguously() {
        // The selection starts inside a fractionally scaled output whose left
        // edge is *not* at pixel zero (1 logical of unscaled output before it),
        // so `origin * ratio` has a fractional part: 1 * 151/101 = 1.495.
        // Each piece's destination must equal the accumulated crop widths of
        // the pieces before it, otherwise the seam gains or loses a column.
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 1, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 1, y: 0, width: 101, height: 80 }, scale: 2 },
            OutputInfo { global_name: 3, logical: LogicalRect { x: 102, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let captures = [captured(1, 1, 80), captured(2, 151, 80), captured(3, 100, 80)];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 7, y: 0, width: 143, height: 80 }, &outputs).unwrap();
        let regions = viewport.capture_regions(&captures);
        assert_eq!(regions.len(), 2);
        let mut placed = 0u32;
        for region in &regions {
            assert_eq!(region.dst_x, placed, "output {} starts at the wrong column", region.output_name);
            placed += u32::try_from(region.region.width).unwrap();
        }
        assert_eq!(viewport.capture_size(&captures).unwrap(), (placed, 80));
    }

    fn captured(output_name: u32, width: u32, height: u32) -> CapturedOutput {
        CapturedOutput {
            output_name,
            image: crate::image::Image {
                width,
                height,
                stride: width * 4,
                format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
                data: vec![0; (width * height * 4) as usize],
            },
        }
    }

    #[test]
    fn rejects_empty_or_non_intersecting_selection() {
        let output = [OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 }];
        assert!(SelectedViewport::from_outputs(LogicalRect { x: 0, y: 0, width: 0, height: 10 }, &output).is_err());
        assert!(SelectedViewport::from_outputs(LogicalRect { x: 200, y: 0, width: 10, height: 10 }, &output).is_err());
    }

    #[test]
    fn spans_outputs_detects_multi_output_selections() {
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let single = LogicalRect { x: 10, y: 10, width: 30, height: 30 };
        let spanning = LogicalRect { x: 80, y: 10, width: 40, height: 30 };
        let outside = LogicalRect { x: 500, y: 500, width: 10, height: 10 };
        assert!(!rect_spans_outputs(single, &outputs));
        assert!(rect_spans_outputs(spanning, &outputs));
        assert!(!rect_spans_outputs(outside, &outputs));
    }

    /// A single screen is the layout the two-screen rig never exercised, and
    /// every rule that counts intersections must still hold there: one region,
    /// no span, and a long-capture region available.
    #[test]
    fn a_single_output_is_one_region_and_never_spans() {
        let outputs = [OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 1920, height: 1080 }, scale: 1 }];
        let inside = LogicalRect { x: 100, y: 100, width: 200, height: 200 };
        let viewport = SelectedViewport::from_outputs(inside, &outputs).unwrap();
        assert_eq!(viewport.regions.len(), 1);
        assert!(!rect_spans_outputs(inside, &outputs));
        assert!(prepare_capture_region(inside, &outputs).is_some());
    }

    /// Three screens in a row: the offsets are accumulated across *every*
    /// covered output, not just the first two. Nothing may be written twice or
    /// skipped when the selection crosses two seams.
    #[test]
    fn a_selection_across_three_outputs_stays_contiguous() {
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 3, logical: LogicalRect { x: 200, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let captures = [captured(1, 100, 80), captured(2, 100, 80), captured(3, 100, 80)];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 90, y: 10, width: 120, height: 20 }, &outputs).unwrap();
        assert_eq!(viewport.regions.len(), 3);
        let regions = viewport.capture_regions(&captures);
        let mut placed = 0u32;
        for region in &regions {
            assert_eq!(region.dst_x, placed, "output {} starts at the wrong column", region.output_name);
            placed += u32::try_from(region.region.width).unwrap();
        }
        assert_eq!(viewport.capture_size(&captures).unwrap(), (120, 20));
    }

    /// Screens stacked vertically, and a screen placed at a negative origin
    /// (to the left of / above the primary). Both are ordinary desktop layouts
    /// that a side-by-side rig never reaches.
    #[test]
    fn stacked_and_negative_origin_layouts_map_into_local_coordinates() {
        let stacked = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 0, y: 80, width: 100, height: 80 }, scale: 1 },
        ];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 10, y: 70, width: 20, height: 20 }, &stacked).unwrap();
        assert_eq!(viewport.regions.len(), 2);
        assert_eq!((viewport.regions[0].local_region.y, viewport.regions[0].local_region.height), (70, 10));
        assert_eq!((viewport.regions[1].local_region.y, viewport.regions[1].local_region.height), (0, 10));

        let negative = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: -100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: -50, y: 10, width: 20, height: 20 }, &negative).unwrap();
        assert_eq!(viewport.regions.len(), 1);
        assert_eq!(viewport.regions[0].local_region.x, 50);
    }

    #[test]
    fn clamp_to_largest_output_keeps_dominant_piece() {
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        // 70 px on the left output, 30 px on the right one.
        let spanning = LogicalRect { x: 30, y: 10, width: 70, height: 30 };
        assert_eq!(
            clamp_rect_to_largest_output(spanning, &outputs),
            LogicalRect { x: 30, y: 10, width: 70, height: 30 }
        );
        // Mostly on the right output: keeps only that piece.
        let mostly_right = LogicalRect { x: 80, y: 10, width: 40, height: 30 };
        assert_eq!(
            clamp_rect_to_largest_output(mostly_right, &outputs),
            LogicalRect { x: 100, y: 10, width: 20, height: 30 }
        );
        // No overlap: returned unchanged (callers treat it as an error case).
        let outside = LogicalRect { x: 500, y: 500, width: 10, height: 10 };
        assert_eq!(clamp_rect_to_largest_output(outside, &outputs), outside);
    }

    #[test]
    fn clamp_to_output_at_point_follows_the_click_anchor() {
        let outputs = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        // Mostly on the right output, but the drag started on the left one:
        // the box belongs to the screen the user clicked on.
        let mostly_right = LogicalRect { x: 80, y: 10, width: 40, height: 30 };
        assert_eq!(
            clamp_rect_to_output_at_point(mostly_right, &outputs, 85, 50),
            LogicalRect { x: 80, y: 10, width: 20, height: 30 }
        );
        // Started on the right screen: keep the right piece instead.
        assert_eq!(
            clamp_rect_to_output_at_point(mostly_right, &outputs, 130, 50),
            LogicalRect { x: 100, y: 10, width: 20, height: 30 }
        );
        // Anchor on no output falls back to the largest piece.
        assert_eq!(
            clamp_rect_to_output_at_point(mostly_right, &outputs, 500, 500),
            clamp_rect_to_largest_output(mostly_right, &outputs)
        );
    }
}

/// Shrinks a capture region inward without moving tiny selections outside
/// their bounds. Long screencopy requests use logical coordinates.
pub fn inset_capture_region(
    region: crate::wayland::screencopy::CaptureOutputRegion,
    scale: i32,
) -> crate::wayland::screencopy::CaptureOutputRegion {
    let inset = scale.max(1);
    let inset_x = inset.min((region.region.width - 1).max(0));
    let inset_y = inset.min((region.region.height - 1).max(0));
    crate::wayland::screencopy::CaptureOutputRegion {
        output_name: region.output_name,
        region: crate::wayland::screencopy::Region {
            x: region.region.x + inset_x,
            y: region.region.y + inset_y,
            width: (region.region.width - 2 * inset_x).max(1),
            height: (region.region.height - 2 * inset_y).max(1),
        },
        dst_x: region.dst_x,
        dst_y: region.dst_y,
    }
}

/// Capture region for a settled selection rectangle on a single output, inset
/// one logical pixel per side.
///
/// Returns `None` for selections that do not map to exactly one output: long
/// capture cannot follow them.
pub fn prepare_capture_region(
    rect: LogicalRect,
    outputs: &[OutputInfo],
) -> Option<crate::wayland::screencopy::CaptureOutputRegion> {
    let viewport = SelectedViewport::from_outputs(rect, outputs).ok()?;
    if viewport.regions.len() != 1 {
        return None;
    }
    // wlr-screencopy capture_output_region takes output-local logical units,
    // unlike the pixel rectangles used to crop frozen normal screenshots.
    let selected = viewport.regions[0];
    let local = selected.local_region;
    Some(inset_capture_region(
        crate::wayland::screencopy::CaptureOutputRegion {
            output_name: selected.output_global_name,
            region: crate::wayland::screencopy::Region {
                x: local.x,
                y: local.y,
                width: local.width,
                height: local.height,
            },
            dst_x: 0,
            dst_y: 0,
        },
        1,
    ))
}

/// Logical screen rectangle a capture region actually covers.
///
/// The preview must map stitched content through this rectangle: using the
/// un-inset selection box would offset the preview by the inset.
pub fn capture_region_logical_rect(
    region: crate::wayland::screencopy::CaptureOutputRegion,
    outputs: &[OutputInfo],
) -> Option<LogicalRect> {
    let output = outputs.iter().find(|o| o.global_name == region.output_name)?;
    Some(LogicalRect {
        x: output.logical.x + region.region.x,
        y: output.logical.y + region.region.y,
        width: region.region.width,
        height: region.region.height,
    })
}

/// Where the captured frame sits in output-local buffer pixels.
///
/// `region` is in logical units while `image` is in buffer pixels, so the
/// offset has to be scaled by the ratio the compositor actually produced.
pub fn frame_screen_offset(
    region: crate::wayland::screencopy::CaptureOutputRegion,
    image: &crate::image::Image,
) -> crate::stitch::FrameScreenOffset {
    crate::stitch::FrameScreenOffset {
        x: (i64::from(region.region.x) * i64::from(image.width)
            / i64::from(region.region.width.max(1))) as i32,
        y: (i64::from(region.region.y) * i64::from(image.height)
            / i64::from(region.region.height.max(1))) as i32,
    }
}
