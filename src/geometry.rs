use crate::wayland::screencopy::{CaptureOutputRegion, Region};

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
    pub output_scale: i32,
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
                output_scale: output.scale.max(1),
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

    pub fn capture_regions(&self) -> Vec<CaptureOutputRegion> {
        self.regions
            .iter()
            .map(|region| CaptureOutputRegion {
                output_name: region.output_global_name,
                region: Region {
                    x: region.local_region.x * region.output_scale,
                    y: region.local_region.y * region.output_scale,
                    width: region.local_region.width * region.output_scale,
                    height: region.local_region.height * region.output_scale,
                },
                dst_x: self.scaled_axis_offset(region.global_region.x, true),
                dst_y: self.scaled_axis_offset(region.global_region.y, false),
            })
            .collect()
    }

    pub fn capture_size(&self) -> Result<(u32, u32), String> {
        let mut width = 0u32;
        let mut height = 0u32;
        for region in self.capture_regions() {
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

    fn scaled_axis_offset(&self, target: i32, horizontal: bool) -> u32 {
        let viewport_start = if horizontal { self.rect.x } else { self.rect.y };
        let mut intervals: Vec<_> = self
            .regions
            .iter()
            .map(|region| {
                let rect = region.global_region;
                let start = if horizontal { rect.x } else { rect.y };
                let end = if horizontal { rect.right() } else { rect.bottom() };
                (start, end, region.output_scale)
            })
            .collect();
        intervals.sort_by_key(|item| *item);

        let mut offset = 0u32;
        let mut covered_until = viewport_start;
        for (start, end, scale) in intervals {
            if end <= covered_until || start >= target {
                continue;
            }
            let segment_start = start.max(covered_until).max(viewport_start);
            let segment_end = end.min(target);
            if segment_end > segment_start {
                let span = u32::try_from(segment_end - segment_start).unwrap_or(0);
                let scale = u32::try_from(scale).unwrap_or(1);
                offset = offset.saturating_add(span.saturating_mul(scale));
                covered_until = segment_end;
            }
        }
        offset
    }
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
        let viewport = SelectedViewport::from_outputs(LogicalRect { x: 90, y: 10, width: 30, height: 20 }, &outputs).unwrap();
        let regions = viewport.capture_regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].region.width, 10);
        assert_eq!(regions[1].region.width, 40);
        assert_eq!(regions[1].dst_x, 10);
        assert_eq!(viewport.capture_size().unwrap(), (50, 40));
    }

    #[test]
    fn rejects_empty_or_non_intersecting_selection() {
        let output = [OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 }];
        assert!(SelectedViewport::from_outputs(LogicalRect { x: 0, y: 0, width: 0, height: 10 }, &output).is_err());
        assert!(SelectedViewport::from_outputs(LogicalRect { x: 200, y: 0, width: 10, height: 10 }, &output).is_err());
    }
}
