use std::error::Error;

use wayland_client::protocol::wl_shm::Format;

use crate::image::Image;

const DUPLICATE_MAX_AVERAGE_DIFF: f64 = 2.0;
const MATCH_MAX_AVERAGE_DIFF: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendDirection {
    Bottom,
    Top,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Vertical,
    Down,
    Up,
}

impl SearchDirection {
    fn includes_bottom(self) -> bool {
        matches!(self, Self::Vertical | Self::Down)
    }

    fn includes_top(self) -> bool {
        matches!(self, Self::Vertical | Self::Up)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMatch {
    pub direction: AppendDirection,
    pub overlap: u32,
    pub delta_x: i32,
    pub delta_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub struct StitchedFrame {
    pub image: Image,
    pub current_viewport: ViewportRect,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushResult {
    Initialized,
    Duplicate,
    Accepted { match_info: FrameMatch },
    NoMatch,
}

pub struct RawStitcher {
    stitched: Option<StitchedFrame>,
    previous_frame: Option<Image>,
}

impl RawStitcher {
    pub fn new() -> Self {
        Self { stitched: None, previous_frame: None }
    }

    pub fn push_frame(&mut self, frame: Image) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_with_direction(frame, SearchDirection::Vertical)
    }

    pub fn push_frame_with_direction(&mut self, frame: Image, direction: SearchDirection) -> Result<PushResult, Box<dyn Error>> {
        validate_image(&frame)?;

        let Some(stitched) = self.stitched.as_ref() else {
            let current_viewport = ViewportRect { x: 0, y: 0, width: frame.width, height: frame.height };
            self.previous_frame = Some(frame.clone());
            self.stitched = Some(StitchedFrame { image: frame, current_viewport });
            return Ok(PushResult::Initialized);
        };

        if let Some(previous_frame) = self.previous_frame.as_ref() {
            if is_duplicate_frame(previous_frame, &frame)? {
                return Ok(PushResult::Duplicate);
            }
        }

        validate_pair(&stitched.image, &frame)?;
        if let Some(viewport_y) = find_exact_vertical_canvas_placement(&stitched.image, &frame)? {
            let current_y = stitched.current_viewport.y;
            let viewport_y = i32::try_from(viewport_y)?;
            let movement_allowed = if viewport_y > current_y {
                direction.includes_bottom()
            } else if viewport_y < current_y {
                direction.includes_top()
            } else {
                true
            };

            if movement_allowed {
                let direction = if viewport_y < current_y { AppendDirection::Top } else { AppendDirection::Bottom };
                let match_info = FrameMatch { direction, overlap: frame.height, delta_x: 0, delta_y: viewport_y - current_y };
                let current_viewport = ViewportRect { x: 0, y: viewport_y, width: frame.width, height: frame.height };
                let image = stitched.image.clone();
                self.stitched = Some(StitchedFrame { image, current_viewport });
                self.previous_frame = Some(frame);
                return Ok(PushResult::Accepted { match_info });
            }

            return Ok(PushResult::NoMatch);
        }

        let max_overlap = stitched.current_viewport.height.min(frame.height);
        match if direction.includes_bottom() { find_viewport_bottom_overlap(stitched, &frame, 1, max_overlap) } else { Err("bottom search direction disabled".into()) } {
            Ok(overlap) => {
                let viewport_y = stitched.current_viewport.y + i32::try_from(stitched.current_viewport.height)? - i32::try_from(overlap)?;
                let viewport_bottom = viewport_y.checked_add(i32::try_from(frame.height)?).ok_or("viewport bottom overflow")?;
                if viewport_y < 0 {
                    return Ok(PushResult::NoMatch);
                }
                let image_height = i32::try_from(stitched.image.height)?;
                let image = if viewport_bottom <= image_height {
                    if !rows_match_pair(&stitched.image, u32::try_from(viewport_y)?, &frame, 0, frame.height)? {
                        return Ok(PushResult::NoMatch);
                    }
                    stitched.image.clone()
                } else {
                    let existing_rows = stitched.image.height.checked_sub(u32::try_from(viewport_y)?).ok_or("viewport start exceeds stitched image height")?;
                    if !rows_match_pair(&stitched.image, u32::try_from(viewport_y)?, &frame, 0, existing_rows)? {
                        return Ok(PushResult::NoMatch);
                    }
                    append_missing_bottom_rows(&stitched.image, &frame, u32::try_from(viewport_bottom - image_height)?)?
                };
                let match_info = FrameMatch { direction: AppendDirection::Bottom, overlap, delta_x: 0, delta_y: viewport_y - stitched.current_viewport.y };
                let current_viewport = ViewportRect { x: 0, y: viewport_y, width: frame.width, height: frame.height };
                self.stitched = Some(StitchedFrame { image, current_viewport });
                self.previous_frame = Some(frame);
                Ok(PushResult::Accepted { match_info })
            }
            Err(_) => match if direction.includes_top() { find_viewport_top_overlap(stitched, &frame, 1, max_overlap) } else { Err("top search direction disabled".into()) } {
                Ok(overlap) => {
                    let non_overlap_rows = frame.height.checked_sub(overlap).ok_or("vertical overlap exceeds next image height")?;
                    let delta_y = -i32::try_from(non_overlap_rows)?;
                    let viewport_y = stitched.current_viewport.y + delta_y;
                    let image = if viewport_y >= 0 {
                        if !rows_match_pair(&stitched.image, u32::try_from(viewport_y)?, &frame, 0, frame.height)? {
                            return Ok(PushResult::NoMatch);
                        }
                        stitched.image.clone()
                    } else {
                        let missing_rows = u32::try_from(-viewport_y)?;
                        let existing_rows = frame.height.checked_sub(missing_rows).ok_or("top prepend exceeds next image height")?;
                        if !rows_match_pair(&stitched.image, 0, &frame, missing_rows, existing_rows)? {
                            return Ok(PushResult::NoMatch);
                        }
                        prepend_missing_top_rows(&stitched.image, &frame, missing_rows)?
                    };
                    let match_info = FrameMatch { direction: AppendDirection::Top, overlap, delta_x: 0, delta_y };
                    let current_viewport = ViewportRect { x: 0, y: viewport_y.max(0), width: frame.width, height: frame.height };
                    self.stitched = Some(StitchedFrame { image, current_viewport });
                    self.previous_frame = Some(frame);
                    Ok(PushResult::Accepted { match_info })
                }
                Err(_) => Ok(PushResult::NoMatch),
            },
        }
    }

    pub fn finish(self) -> Option<StitchedFrame> {
        self.stitched
    }
}

pub fn find_vertical_overlap(previous: &Image, next: &Image, min_overlap: u32, max_overlap: u32) -> Result<u32, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if min_overlap > max_overlap {
        return Err("minimum overlap exceeds maximum overlap".into());
    }

    let max_possible = max_overlap.min(previous.height).min(next.height);
    if min_overlap == 0 || min_overlap > max_possible {
        return Err("overlap range is outside image bounds".into());
    }

    for overlap in (min_overlap..=max_possible).rev() {
        if rows_overlap(previous, next, overlap)? {
            return Ok(overlap);
        }
    }

    Err("no vertical overlap within RGB threshold found".into())
}

pub fn append_vertical(previous: &Image, next: &Image, overlap: u32) -> Result<Image, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if overlap == 0 || overlap > previous.height || overlap > next.height {
        return Err("invalid vertical overlap".into());
    }

    let appended_height = next.height.checked_sub(overlap).ok_or("vertical overlap exceeds next image height")?;
    let height = previous.height.checked_add(appended_height).ok_or("stitched image height overflow")?;
    let data_len = usize::try_from(previous.stride)?.checked_mul(usize::try_from(height)?).ok_or("stitched image data length overflow")?;
    let mut data = Vec::with_capacity(data_len);

    copy_rows(previous, 0, previous.height, &mut data)?;
    copy_rows(next, overlap, appended_height, &mut data)?;

    Ok(Image { width: previous.width, height, stride: previous.stride, format: previous.format, data })
}

pub fn find_vertical_prepend_overlap(previous: &Image, next: &Image, min_overlap: u32, max_overlap: u32) -> Result<u32, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if min_overlap > max_overlap {
        return Err("minimum overlap exceeds maximum overlap".into());
    }

    let max_possible = max_overlap.min(previous.height).min(next.height);
    if min_overlap == 0 || min_overlap > max_possible {
        return Err("overlap range is outside image bounds".into());
    }

    for overlap in (min_overlap..=max_possible).rev() {
        if rows_prepend_overlap(previous, next, overlap)? {
            return Ok(overlap);
        }
    }

    Err("no vertical prepend overlap within RGB threshold found".into())
}

pub fn prepend_vertical(previous: &Image, next: &Image, overlap: u32) -> Result<Image, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if overlap == 0 || overlap > previous.height || overlap > next.height {
        return Err("invalid vertical overlap".into());
    }

    let prepended_height = next.height.checked_sub(overlap).ok_or("vertical overlap exceeds next image height")?;
    let height = previous.height.checked_add(prepended_height).ok_or("stitched image height overflow")?;
    let data_len = usize::try_from(previous.stride)?.checked_mul(usize::try_from(height)?).ok_or("stitched image data length overflow")?;
    let mut data = Vec::with_capacity(data_len);

    copy_rows(next, 0, prepended_height, &mut data)?;
    copy_rows(previous, 0, previous.height, &mut data)?;

    Ok(Image { width: previous.width, height, stride: previous.stride, format: previous.format, data })
}

fn append_missing_bottom_rows(previous: &Image, next: &Image, missing_rows: u32) -> Result<Image, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if missing_rows == 0 || missing_rows > next.height {
        return Err("invalid bottom append row count".into());
    }

    let height = previous.height.checked_add(missing_rows).ok_or("stitched image height overflow")?;
    let data_len = usize::try_from(previous.stride)?.checked_mul(usize::try_from(height)?).ok_or("stitched image data length overflow")?;
    let mut data = Vec::with_capacity(data_len);

    copy_rows(previous, 0, previous.height, &mut data)?;
    copy_rows(next, next.height - missing_rows, missing_rows, &mut data)?;

    Ok(Image { width: previous.width, height, stride: previous.stride, format: previous.format, data })
}

fn prepend_missing_top_rows(previous: &Image, next: &Image, missing_rows: u32) -> Result<Image, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if missing_rows == 0 || missing_rows > next.height {
        return Err("invalid top prepend row count".into());
    }

    let height = previous.height.checked_add(missing_rows).ok_or("stitched image height overflow")?;
    let data_len = usize::try_from(previous.stride)?.checked_mul(usize::try_from(height)?).ok_or("stitched image data length overflow")?;
    let mut data = Vec::with_capacity(data_len);

    copy_rows(next, 0, missing_rows, &mut data)?;
    copy_rows(previous, 0, previous.height, &mut data)?;

    Ok(Image { width: previous.width, height, stride: previous.stride, format: previous.format, data })
}

pub fn find_exact_vertical_canvas_placement(stitched: &Image, frame: &Image) -> Result<Option<u32>, Box<dyn Error>> {
    validate_pair(stitched, frame)?;
    if frame.height > stitched.height {
        return Ok(None);
    }

    for y in 0..=stitched.height - frame.height {
        if exact_rows_match_at(stitched, frame, y)? {
            return Ok(Some(y));
        }
    }

    Ok(None)
}

fn validate_pair(previous: &Image, next: &Image) -> Result<(), Box<dyn Error>> {
    validate_image(previous)?;
    validate_image(next)?;
    if previous.width != next.width {
        return Err("images have different widths".into());
    }
    if previous.format != next.format {
        return Err("images have different formats".into());
    }
    if previous.stride != next.stride {
        return Err("images have different strides".into());
    }
    Ok(())
}

fn is_duplicate_frame(previous: &Image, next: &Image) -> Result<bool, Box<dyn Error>> {
    if previous.width != next.width || previous.height != next.height || previous.stride != next.stride || previous.format != next.format {
        return Ok(false);
    }

    Ok(average_rgb_difference(previous, next)? <= DUPLICATE_MAX_AVERAGE_DIFF)
}

fn average_rgb_difference(previous: &Image, next: &Image) -> Result<f64, Box<dyn Error>> {
    validate_pair(previous, next)?;
    if previous.height != next.height {
        return Err("images have different heights".into());
    }

    average_rgb_difference_for_ranges(previous, 0, next, 0, previous.height)
}

fn average_rgb_difference_for_ranges(first: &Image, first_start: u32, second: &Image, second_start: u32, row_count: u32) -> Result<f64, Box<dyn Error>> {
    validate_pair(first, second)?;
    if row_count == 0 {
        return Err("stitch row count must be non-zero".into());
    }
    if first_start.checked_add(row_count).ok_or("stitch row range overflow")? > first.height || second_start.checked_add(row_count).ok_or("stitch row range overflow")? > second.height {
        return Err("stitch row range is outside image bounds".into());
    }

    let mut total_difference: u64 = 0;
    for row in 0..row_count {
        let first_row = row_pixels(first, first_start + row)?;
        let second_row = row_pixels(second, second_start + row)?;
        for pixel in 0..usize::try_from(first.width)? {
            let offset = pixel.checked_mul(4).ok_or("stitch pixel offset overflow")?;
            for channel in 0..3 {
                total_difference += first_row[offset + channel].abs_diff(second_row[offset + channel]) as u64;
            }
        }
    }

    let channel_count = u64::from(first.width).checked_mul(u64::from(row_count)).and_then(|pixels| pixels.checked_mul(3)).ok_or("stitch RGB channel count overflow")?;
    Ok(total_difference as f64 / channel_count as f64)
}

fn validate_image(image: &Image) -> Result<(), Box<dyn Error>> {
    if !matches!(image.format, Format::Xrgb8888 | Format::Argb8888) {
        return Err(format!("unsupported stitch image format: {:?}", image.format).into());
    }
    if image.width == 0 || image.height == 0 {
        return Err("stitch image dimensions must be non-zero".into());
    }
    let row_bytes = image.width.checked_mul(4).ok_or("stitch image row size overflow")?;
    if image.stride < row_bytes {
        return Err("stitch image stride is shorter than row width".into());
    }
    let required_len = usize::try_from(image.stride)?.checked_mul(usize::try_from(image.height)?).ok_or("stitch image data length overflow")?;
    if image.data.len() < required_len {
        return Err("stitch image data is shorter than geometry requires".into());
    }
    Ok(())
}

fn rows_overlap(previous: &Image, next: &Image, overlap: u32) -> Result<bool, Box<dyn Error>> {
    Ok(average_rgb_difference_for_ranges(previous, previous.height - overlap, next, 0, overlap)? <= MATCH_MAX_AVERAGE_DIFF)
}

fn rows_prepend_overlap(previous: &Image, next: &Image, overlap: u32) -> Result<bool, Box<dyn Error>> {
    Ok(average_rgb_difference_for_ranges(previous, 0, next, next.height - overlap, overlap)? <= MATCH_MAX_AVERAGE_DIFF)
}

fn exact_rows_match_at(stitched: &Image, frame: &Image, y: u32) -> Result<bool, Box<dyn Error>> {
    validate_pair(stitched, frame)?;
    for row in 0..frame.height {
        if row_pixels(stitched, y + row)? != row_pixels(frame, row)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn find_viewport_bottom_overlap(stitched: &StitchedFrame, next: &Image, min_overlap: u32, max_overlap: u32) -> Result<u32, Box<dyn Error>> {
    validate_pair(&stitched.image, next)?;
    if min_overlap > max_overlap {
        return Err("minimum overlap exceeds maximum overlap".into());
    }

    let max_possible = max_overlap.min(stitched.current_viewport.height).min(next.height);
    if min_overlap == 0 || min_overlap > max_possible {
        return Err("overlap range is outside image bounds".into());
    }

    let viewport_y = u32::try_from(stitched.current_viewport.y)?;
    for overlap in (min_overlap..=max_possible).rev() {
        let stitched_start = viewport_y + stitched.current_viewport.height - overlap;
        if rows_match_pair(&stitched.image, stitched_start, next, 0, overlap)? {
            return Ok(overlap);
        }
    }

    Err("no viewport bottom overlap within RGB threshold found".into())
}

fn find_viewport_top_overlap(stitched: &StitchedFrame, next: &Image, min_overlap: u32, max_overlap: u32) -> Result<u32, Box<dyn Error>> {
    validate_pair(&stitched.image, next)?;
    if min_overlap > max_overlap {
        return Err("minimum overlap exceeds maximum overlap".into());
    }

    let max_possible = max_overlap.min(stitched.current_viewport.height).min(next.height);
    if min_overlap == 0 || min_overlap > max_possible {
        return Err("overlap range is outside image bounds".into());
    }

    let viewport_y = u32::try_from(stitched.current_viewport.y)?;
    for overlap in (min_overlap..=max_possible).rev() {
        if rows_match_pair(&stitched.image, viewport_y, next, next.height - overlap, overlap)? {
            return Ok(overlap);
        }
    }

    Err("no viewport top overlap within RGB threshold found".into())
}

fn rows_match_pair(first: &Image, first_start: u32, second: &Image, second_start: u32, row_count: u32) -> Result<bool, Box<dyn Error>> {
    Ok(average_rgb_difference_for_ranges(first, first_start, second, second_start, row_count)? <= MATCH_MAX_AVERAGE_DIFF)
}

fn copy_rows(image: &Image, start_row: u32, row_count: u32, out: &mut Vec<u8>) -> Result<(), Box<dyn Error>> {
    for row in start_row..start_row.checked_add(row_count).ok_or("stitch row range overflow")? {
        out.extend_from_slice(row_data(image, row)?);
    }
    Ok(())
}

fn row_pixels(image: &Image, row: u32) -> Result<&[u8], Box<dyn Error>> {
    let row_data = row_data(image, row)?;
    let row_bytes = usize::try_from(image.width)?.checked_mul(4).ok_or("stitch image row size overflow")?;
    Ok(&row_data[..row_bytes])
}

fn row_data(image: &Image, row: u32) -> Result<&[u8], Box<dyn Error>> {
    if row >= image.height {
        return Err("stitch row is outside image bounds".into());
    }
    let start = usize::try_from(row)?.checked_mul(usize::try_from(image.stride)?).ok_or("stitch row offset overflow")?;
    let end = start.checked_add(usize::try_from(image.stride)?).ok_or("stitch row end overflow")?;
    Ok(&image.data[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, rows: &[u8]) -> Image {
        let stride = width * 4;
        Image { width, height: rows.len() as u32, stride, format: Format::Xrgb8888, data: rows.iter().flat_map(|value| [*value; 4]).collect() }
    }

    fn pixel_image(width: u32, height: u32, pixels: &[u8]) -> Image {
        Image { width, height, stride: width * 4, format: Format::Xrgb8888, data: pixels.to_vec() }
    }

    fn assert_image_eq(actual: &Image, expected: &Image) {
        assert_eq!(actual.width, expected.width);
        assert_eq!(actual.height, expected.height);
        assert_eq!(actual.stride, expected.stride);
        assert_eq!(actual.format, expected.format);
        assert_eq!(actual.data, expected.data);
    }

    #[test]
    fn finds_exact_vertical_overlap() {
        let previous = image(1, &[1, 20, 30]);
        let next = image(1, &[20, 30, 100]);

        assert_eq!(find_vertical_overlap(&previous, &next, 1, 3).unwrap(), 2);
    }

    #[test]
    fn prefers_largest_exact_overlap() {
        let previous = image(1, &[1, 2, 1, 2]);
        let next = image(1, &[1, 2, 3]);

        assert_eq!(find_vertical_overlap(&previous, &next, 1, 2).unwrap(), 2);
    }

    #[test]
    fn finds_exact_vertical_prepend_overlap() {
        let previous = image(1, &[30, 100]);
        let next = image(1, &[1, 2, 30]);

        assert_eq!(find_vertical_prepend_overlap(&previous, &next, 1, 2).unwrap(), 1);
    }

    #[test]
    fn prepend_overlap_prefers_largest_exact_overlap() {
        let previous = image(1, &[1, 2, 3]);
        let next = image(1, &[4, 1, 2]);

        assert_eq!(find_vertical_prepend_overlap(&previous, &next, 1, 2).unwrap(), 2);
    }

    #[test]
    fn finds_first_exact_vertical_canvas_placement() {
        let stitched = image(1, &[1, 2, 1, 2]);
        let frame = image(1, &[1, 2]);

        assert_eq!(find_exact_vertical_canvas_placement(&stitched, &frame).unwrap(), Some(0));
    }

    #[test]
    fn fails_when_no_exact_overlap_exists() {
        let previous = image(1, &[1, 2, 3]);
        let next = image(1, &[40, 50, 60]);

        assert!(find_vertical_overlap(&previous, &next, 1, 2).is_err());
    }

    #[test]
    fn fuzzy_bottom_overlap_is_accepted() {
        let previous = pixel_image(1, 3, &[10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255]);
        let next = pixel_image(1, 3, &[22, 22, 22, 255, 32, 32, 32, 255, 40, 40, 40, 255]);

        assert_eq!(find_vertical_overlap(&previous, &next, 1, 2).unwrap(), 2);
    }

    #[test]
    fn fuzzy_top_overlap_is_accepted() {
        let previous = pixel_image(1, 2, &[20, 20, 20, 255, 30, 30, 30, 255]);
        let next = pixel_image(1, 3, &[1, 1, 1, 255, 22, 22, 22, 255, 32, 32, 32, 255]);

        assert_eq!(find_vertical_prepend_overlap(&previous, &next, 1, 2).unwrap(), 2);
    }

    #[test]
    fn overlap_above_rgb_threshold_is_rejected() {
        let previous = pixel_image(1, 2, &[10, 10, 10, 255, 20, 20, 20, 255]);
        let next = pixel_image(1, 2, &[24, 24, 24, 255, 30, 30, 30, 255]);

        assert!(find_vertical_overlap(&previous, &next, 1, 1).is_err());
    }

    #[test]
    fn prefers_largest_acceptable_fuzzy_overlap() {
        let previous = pixel_image(1, 3, &[10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255]);
        let next = pixel_image(1, 3, &[21, 21, 21, 255, 32, 32, 32, 255, 80, 80, 80, 255]);

        assert_eq!(find_vertical_overlap(&previous, &next, 1, 2).unwrap(), 2);
    }

    #[test]
    fn alpha_only_overlap_differences_are_ignored() {
        let previous = pixel_image(1, 2, &[10, 20, 30, 0, 40, 50, 60, 0]);
        let next = pixel_image(1, 2, &[40, 50, 60, 255, 70, 80, 90, 255]);

        assert_eq!(find_vertical_overlap(&previous, &next, 1, 1).unwrap(), 1);
    }

    #[test]
    fn rejects_dimension_and_format_mismatch() {
        let previous = image(1, &[1, 2]);
        let mut different_width = image(2, &[1, 2]);
        different_width.height = 1;
        let mut different_format = image(1, &[1, 2]);
        different_format.format = Format::Argb8888;

        assert!(find_vertical_overlap(&previous, &different_width, 1, 1).is_err());
        assert!(append_vertical(&previous, &different_format, 1).is_err());
    }

    #[test]
    fn append_output_preserves_row_bytes_after_overlap() {
        let previous = Image {
            width: 1,
            height: 2,
            stride: 8,
            format: Format::Xrgb8888,
            data: vec![1, 1, 1, 1, 9, 9, 9, 9, 2, 2, 2, 2, 8, 8, 8, 8],
        };
        let next = Image {
            width: 1,
            height: 3,
            stride: 8,
            format: Format::Xrgb8888,
            data: vec![2, 2, 2, 2, 7, 7, 7, 7, 3, 3, 3, 3, 6, 6, 6, 6, 4, 4, 4, 4, 5, 5, 5, 5],
        };

        let stitched = append_vertical(&previous, &next, 1).unwrap();

        assert_eq!(stitched.width, 1);
        assert_eq!(stitched.height, 4);
        assert_eq!(stitched.stride, 8);
        assert_eq!(stitched.format, Format::Xrgb8888);
        assert_eq!(stitched.data, vec![1, 1, 1, 1, 9, 9, 9, 9, 2, 2, 2, 2, 8, 8, 8, 8, 3, 3, 3, 3, 6, 6, 6, 6, 4, 4, 4, 4, 5, 5, 5, 5]);
    }

    #[test]
    fn prepend_output_preserves_existing_rows_in_overlap() {
        let previous = image(1, &[3, 4]);
        let next = image(1, &[1, 2, 3]);

        let stitched = prepend_vertical(&previous, &next, 1).unwrap();

        assert_image_eq(&stitched, &image(1, &[1, 2, 3, 4]));
    }

    #[test]
    fn rejects_invalid_overlap() {
        let previous = image(1, &[1, 2]);
        let next = image(1, &[2, 3]);

        assert!(append_vertical(&previous, &next, 0).is_err());
        assert!(append_vertical(&previous, &next, 3).is_err());
        assert!(prepend_vertical(&previous, &next, 0).is_err());
        assert!(prepend_vertical(&previous, &next, 3).is_err());
        assert!(find_vertical_overlap(&previous, &next, 2, 1).is_err());
        assert!(find_vertical_prepend_overlap(&previous, &next, 2, 1).is_err());
    }

    #[test]
    fn raw_stitcher_initializes_on_first_frame() {
        let mut stitcher = RawStitcher::new();
        let frame = image(1, &[1, 2]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &frame);
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 2 });
    }

    #[test]
    fn raw_stitcher_ignores_exact_duplicate_frame() {
        let mut stitcher = RawStitcher::new();
        let frame = image(1, &[1, 2]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Duplicate);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &frame);
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 2 });
    }

    #[test]
    fn raw_stitcher_ignores_near_duplicate_frame_below_rgb_threshold() {
        let mut stitcher = RawStitcher::new();
        let frame = pixel_image(1, 1, &[10, 20, 30, 255]);
        let near_duplicate = pixel_image(1, 1, &[11, 21, 31, 255]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(near_duplicate).unwrap(), PushResult::Duplicate);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &frame);
    }

    #[test]
    fn raw_stitcher_ignores_near_duplicate_frame_at_rgb_threshold() {
        let mut stitcher = RawStitcher::new();
        let frame = pixel_image(1, 1, &[10, 20, 30, 255]);
        let near_duplicate = pixel_image(1, 1, &[12, 22, 32, 255]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(near_duplicate).unwrap(), PushResult::Duplicate);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &frame);
    }

    #[test]
    fn raw_stitcher_places_larger_rgb_delta_instead_of_duplicate() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(pixel_image(1, 1, &[10, 20, 30, 255])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(pixel_image(1, 1, &[14, 24, 34, 255])).unwrap(), PushResult::NoMatch);
    }

    #[test]
    fn raw_stitcher_ignores_alpha_only_difference_as_duplicate() {
        let mut stitcher = RawStitcher::new();
        let frame = pixel_image(1, 1, &[10, 20, 30, 0]);
        let alpha_changed = pixel_image(1, 1, &[10, 20, 30, 255]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(alpha_changed).unwrap(), PushResult::Duplicate);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &frame);
    }

    #[test]
    fn raw_stitcher_geometry_mismatch_is_not_duplicate() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(pixel_image(1, 1, &[1, 1, 1, 255])).unwrap(), PushResult::Initialized);
        assert!(stitcher.push_frame(pixel_image(2, 1, &[1, 1, 1, 255, 1, 1, 1, 255])).is_err());
    }

    #[test]
    fn raw_stitcher_accepts_exact_vertical_overlap() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(
            stitcher.push_frame(image(1, &[20, 30, 40])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 1, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_accepts_fuzzy_bottom_overlap() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(pixel_image(1, 3, &[10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255])).unwrap(), PushResult::Initialized);
        assert_eq!(
            stitcher.push_frame(pixel_image(1, 3, &[22, 22, 22, 255, 32, 32, 32, 255, 40, 40, 40, 255])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &pixel_image(1, 4, &[10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255]));
    }

    #[test]
    fn raw_stitcher_accepts_exact_vertical_prepend_overlap() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[30, 40])).unwrap(), PushResult::Initialized);
        assert_eq!(
            stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 1, delta_x: 0, delta_y: -2 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_accepts_fuzzy_top_overlap() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(pixel_image(1, 2, &[30, 30, 30, 255, 40, 40, 40, 255])).unwrap(), PushResult::Initialized);
        assert_eq!(
            stitcher.push_frame(pixel_image(1, 3, &[10, 10, 10, 255, 20, 20, 20, 255, 32, 32, 32, 255])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 1, delta_x: 0, delta_y: -2 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &pixel_image(1, 4, &[10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255]));
    }

    #[test]
    fn raw_stitcher_exact_canvas_placement_moves_viewport_without_growing_image() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[20, 30, 40])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } });
        assert_eq!(
            stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 3, delta_x: 0, delta_y: -1 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_down_rejects_top_canvas_placement_without_mutating_state() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[20, 30, 40])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } });
        assert_eq!(stitcher.push_frame_with_direction(image(1, &[10, 20, 30]), SearchDirection::Down).unwrap(), PushResult::NoMatch);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 1, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_same_position_canvas_placement_is_accepted_with_zero_delta() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[20, 30, 40])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } });
        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 3, delta_x: 0, delta_y: -1 } });
        assert_eq!(
            stitcher.push_frame(image(1, &[10, 20])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 0 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 2 });
    }

    #[test]
    fn raw_stitcher_appends_when_frame_is_outside_canvas() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(
            stitcher.push_frame(image(1, &[20, 30, 40])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 1, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_appends_relative_to_viewport_after_moving_up_inside_canvas() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30, 40])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[30, 40, 50, 60])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 2 } });
        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30, 40])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 4, delta_x: 0, delta_y: -2 } });
        assert_eq!(
            stitcher.push_frame(image(1, &[30, 40, 50, 60, 70])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 2 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 40, 50, 60, 70]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 2, width: 1, height: 5 });
    }

    #[test]
    fn raw_stitcher_prepends_relative_to_viewport_after_moving_down_inside_canvas() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[30, 40, 50, 60])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30, 40])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 2, delta_x: 0, delta_y: -2 } });
        assert_eq!(stitcher.push_frame(image(1, &[30, 40, 50, 60])).unwrap(), PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 4, delta_x: 0, delta_y: 2 } });
        assert_eq!(
            stitcher.push_frame(image(1, &[0, 10, 20, 30, 40])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 2, delta_x: 0, delta_y: -3 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[0, 10, 20, 30, 40, 50, 60]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 5 });
    }

    #[test]
    fn raw_stitcher_down_rejects_top_only_match() {
        let mut stitcher = RawStitcher::new();
        let initial = image(1, &[30, 40]);

        assert_eq!(stitcher.push_frame(initial.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame_with_direction(image(1, &[10, 20, 30]), SearchDirection::Down).unwrap(), PushResult::NoMatch);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &initial);
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 2 });
    }

    #[test]
    fn raw_stitcher_up_rejects_bottom_only_match() {
        let mut stitcher = RawStitcher::new();
        let initial = image(1, &[10, 20, 30]);

        assert_eq!(stitcher.push_frame(initial.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame_with_direction(image(1, &[20, 30, 40]), SearchDirection::Up).unwrap(), PushResult::NoMatch);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &initial);
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_vertical_accepts_bottom_and_top_matches() {
        let mut down_stitcher = RawStitcher::new();
        assert_eq!(down_stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(
            down_stitcher.push_frame_with_direction(image(1, &[20, 30, 40]), SearchDirection::Vertical).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } }
        );

        let mut up_stitcher = RawStitcher::new();
        assert_eq!(up_stitcher.push_frame(image(1, &[30, 40])).unwrap(), PushResult::Initialized);
        assert_eq!(
            up_stitcher.push_frame_with_direction(image(1, &[10, 20, 30]), SearchDirection::Vertical).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Top, overlap: 1, delta_x: 0, delta_y: -2 } }
        );
    }

    #[test]
    fn raw_stitcher_duplicate_ignores_search_direction() {
        let mut stitcher = RawStitcher::new();
        let frame = image(1, &[1, 2]);

        assert_eq!(stitcher.push_frame(frame.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame_with_direction(frame.clone(), SearchDirection::Down).unwrap(), PushResult::Duplicate);
        assert_eq!(stitcher.push_frame_with_direction(frame, SearchDirection::Up).unwrap(), PushResult::Duplicate);
    }

    #[test]
    fn raw_stitcher_no_match_keeps_existing_output() {
        let mut stitcher = RawStitcher::new();
        let initial = image(1, &[1, 2, 3]);

        assert_eq!(stitcher.push_frame(initial.clone()).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[40, 50, 60])).unwrap(), PushResult::NoMatch);
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &initial);
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 0, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_no_match_keeps_previous_frame() {
        let mut stitcher = RawStitcher::new();

        assert_eq!(stitcher.push_frame(image(1, &[10, 20, 30])).unwrap(), PushResult::Initialized);
        assert_eq!(stitcher.push_frame(image(1, &[40, 50, 60])).unwrap(), PushResult::NoMatch);
        assert_eq!(
            stitcher.push_frame(image(1, &[20, 30, 70])).unwrap(),
            PushResult::Accepted { match_info: FrameMatch { direction: AppendDirection::Bottom, overlap: 2, delta_x: 0, delta_y: 1 } }
        );
        let stitched = stitcher.finish().unwrap();
        assert_image_eq(&stitched.image, &image(1, &[10, 20, 30, 70]));
        assert_eq!(stitched.current_viewport, ViewportRect { x: 0, y: 1, width: 1, height: 3 });
    }

    #[test]
    fn raw_stitcher_finish_before_init_returns_none() {
        assert!(RawStitcher::new().finish().is_none());
    }
}
