use super::perceptual::PerceptualMotionEstimate;
use super::rgb::{average_difference_same, pixel_difference_source, validate_rgb_frame};
use super::{FixedBands, RgbFrame};

const FIXED_ROW_WINDOW_HEIGHT: u32 = 4;
const FIXED_ROW_STEP: u32 = 2;
const FIXED_SAME_MAX_AVERAGE_DIFF: f64 = 3.0;
const FIXED_MOTION_MIN_SEPARATION: f64 = 8.0;
const FIXED_MAX_BAND_FRACTION_NUM: u32 = 2;
const FIXED_MAX_BAND_FRACTION_DEN: u32 = 5;
const FIXED_STABLE_OBSERVATIONS: u8 = 2;
const FIXED_HEIGHT_TOLERANCE: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FixedBandObservation {
    pub(super) bands: FixedBands,
    pub(super) count: u8,
}

#[derive(Debug, Clone)]
pub(super) struct FixedBandDetector {
    pub(super) stable: FixedBands,
    pub(super) pending: Option<FixedBandObservation>,
    observations_required: u8,
    pub(super) frozen: bool,
}

impl Default for FixedBandDetector {
    fn default() -> Self {
        Self {
            stable: FixedBands::default(),
            pending: None,
            observations_required: FIXED_STABLE_OBSERVATIONS,
            frozen: false,
        }
    }
}

impl FixedBandDetector {
    pub(super) fn observe(
        &mut self,
        previous: &RgbFrame,
        current: &RgbFrame,
        estimate: Option<PerceptualMotionEstimate>,
    ) -> FixedBandObservation {
        if self.frozen {
            return FixedBandObservation {
                bands: self.stable,
                count: self.observations_required,
            };
        }

        let bands = detect_fixed_bands(previous, current, estimate).unwrap_or_default();
        let count = match self.pending {
            Some(pending)
                if pending.bands.is_empty() == bands.is_empty()
                    && fixed_bands_within_tolerance(pending.bands, bands) =>
            {
                pending.count.saturating_add(1)
            }
            _ => 1,
        };
        let observation = FixedBandObservation { bands, count };
        self.pending = Some(observation);

        if !bands.is_empty() && count >= self.observations_required {
            self.stable = bands;
            self.frozen = true;
        }

        observation
    }

    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
    pub(super) fn pending_summary(&self) -> String {
        match self.pending {
            Some(pending) => format!(
                "{}:{}/{}",
                fixed_bands_summary(pending.bands),
                pending.count,
                self.observations_required
            ),
            None => "none".to_string(),
        }
    }

    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
    pub(super) fn stable_summary(&self) -> String {
        fixed_bands_summary(self.stable)
    }
}

pub(super) fn detect_fixed_bands(
    previous: &RgbFrame,
    current: &RgbFrame,
    estimate: Option<PerceptualMotionEstimate>,
) -> Option<FixedBands> {
    let delta_y = estimate?.delta_y;
    if delta_y == 0
        || previous.width != current.width
        || previous.height != current.height
        || previous.height < FIXED_ROW_WINDOW_HEIGHT
        || validate_rgb_frame(previous).is_err()
        || validate_rgb_frame(current).is_err()
        || average_difference_same(previous, current) <= FIXED_SAME_MAX_AVERAGE_DIFF
    {
        return None;
    }

    let max_band =
        previous.height.saturating_mul(FIXED_MAX_BAND_FRACTION_NUM) / FIXED_MAX_BAND_FRACTION_DEN;
    let top = scan_fixed_top_band(previous, current, delta_y, max_band);
    let bottom = scan_fixed_bottom_band(previous, current, delta_y, max_band);
    let mut bands = FixedBands { top, bottom };
    if bands.top.saturating_add(bands.bottom) >= previous.height {
        bands = FixedBands::default();
    }
    bands.active_crop(previous.width, previous.height)?;
    Some(bands)
}

pub(super) fn scan_fixed_top_band(
    previous: &RgbFrame,
    current: &RgbFrame,
    delta_y: i32,
    max_band: u32,
) -> u32 {
    let mut band = 0;
    let scan_limit = max_band
        .min(previous.height)
        .saturating_sub(FIXED_ROW_WINDOW_HEIGHT);
    let mut y = 0;
    while y <= scan_limit {
        if !fixed_window_at(previous, current, y, delta_y) {
            break;
        }
        band = (y + FIXED_ROW_WINDOW_HEIGHT).min(max_band);
        y = y.saturating_add(FIXED_ROW_STEP);
    }
    band
}

pub(super) fn scan_fixed_bottom_band(
    previous: &RgbFrame,
    current: &RgbFrame,
    delta_y: i32,
    max_band: u32,
) -> u32 {
    let mut band = 0;
    let scan_limit = max_band
        .min(previous.height)
        .saturating_sub(FIXED_ROW_WINDOW_HEIGHT);
    let mut offset = 0;
    while offset <= scan_limit {
        let y = previous.height - FIXED_ROW_WINDOW_HEIGHT - offset;
        if !fixed_window_at(previous, current, y, delta_y) {
            break;
        }
        band = (offset + FIXED_ROW_WINDOW_HEIGHT).min(max_band);
        offset = offset.saturating_add(FIXED_ROW_STEP);
    }
    band
}

pub(super) fn fixed_window_at(
    previous: &RgbFrame,
    current: &RgbFrame,
    y: u32,
    delta_y: i32,
) -> bool {
    if y.saturating_add(FIXED_ROW_WINDOW_HEIGHT) > current.height {
        return false;
    }

    let same_score = window_average_difference(previous, y, current, y, FIXED_ROW_WINDOW_HEIGHT);
    let motion_score = match i64::from(y).checked_sub(i64::from(delta_y)) {
        Some(value)
            if value >= 0
                && (value as u32).saturating_add(FIXED_ROW_WINDOW_HEIGHT) <= previous.height =>
        {
            window_average_difference(previous, value as u32, current, y, FIXED_ROW_WINDOW_HEIGHT)
        }
        _ => f64::from(u8::MAX),
    };
    same_score <= FIXED_SAME_MAX_AVERAGE_DIFF
        && motion_score >= same_score + FIXED_MOTION_MIN_SEPARATION
}

pub(super) fn window_average_difference(
    previous: &RgbFrame,
    previous_y: u32,
    current: &RgbFrame,
    current_y: u32,
    height: u32,
) -> f64 {
    let width = previous.width.min(current.width);
    let mut total = 0u64;
    let mut samples = 0u32;
    for row in 0..height {
        let previous_row = previous_y + row;
        let current_row = current_y + row;
        for x in (0..width).step_by(FIXED_ROW_STEP as usize) {
            total +=
                pixel_difference_source(previous, x, previous_row, current, x, current_row) as u64;
            samples += 1;
        }
    }
    fixed_line_average_difference(total, samples)
}

pub(super) fn fixed_bands_within_tolerance(a: FixedBands, b: FixedBands) -> bool {
    a.top.abs_diff(b.top) <= FIXED_HEIGHT_TOLERANCE
        && a.bottom.abs_diff(b.bottom) <= FIXED_HEIGHT_TOLERANCE
}

#[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
pub(super) fn fixed_bands_summary(bands: FixedBands) -> String {
    format!("{}/{}", bands.top, bands.bottom)
}

fn fixed_line_average_difference(total: u64, pixels: u32) -> f64 {
    if pixels == 0 {
        f64::MAX
    } else {
        total as f64 / pixels as f64 / 3.0
    }
}
