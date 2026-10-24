use super::rgb::RgbSource;
use super::{
    minimum_overlap, AppendDirection, FastMotionCandidate, FastMotionVerifyPass, FrameMatch,
    RgbFrame, SearchDirection, MATCH_PREFILTER_MAX_AVERAGE_DIFF,
};

pub(super) const PERCEPTUAL_MOTION_DELTA_RANGE: i32 = 150;
pub(super) const PERCEPTUAL_MOTION_BAND_HEIGHT: u32 = 8;
const PERCEPTUAL_MOTION_BAND_STEP: u32 = 4;
const PERCEPTUAL_MOTION_BINS: usize = 64;
pub(super) const PERCEPTUAL_MOTION_MARGIN: f64 = 0.5;
pub(super) const PERCEPTUAL_MOTION_ADJACENT_DELTA: i32 = 2;
const PERCEPTUAL_MOTION_MIN_OVERLAP_RATIO: u32 = 2;
#[allow(dead_code)]
pub(super) const FAST_MOTION_PERCEPTUAL_FIRST_PASS: usize = 20;
#[allow(dead_code)]
pub(super) const FAST_MOTION_PERCEPTUAL_SECOND_PASS: usize = 50;

#[derive(Debug, Clone)]
pub(super) struct PerceptualFrame {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) luminance: Vec<f32>,
}

impl PerceptualFrame {
    pub(super) fn from_source(source: &impl RgbSource) -> Self {
        let mut luminance = Vec::with_capacity(source.width() as usize * source.height() as usize);
        for y in 0..source.height() {
            for x in 0..source.width() {
                let [r, g, b] = source.pixel_rgb(x, y);
                luminance.push(
                    ((77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8) as f32,
                );
            }
        }
        Self {
            width: source.width(),
            height: source.height(),
            luminance,
        }
    }

    pub(super) fn luminance(&self, x: u32, y: u32) -> f32 {
        self.luminance[y as usize * self.width as usize + x as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PerceptualMotionEstimate {
    pub(super) delta_y: i32,
    pub(super) median: f64,
    pub(super) p75: f64,
    pub(super) p90: f64,
    pub(super) mean: f64,
    pub(super) second_best_delta_y: Option<i32>,
    pub(super) second_best_median: Option<f64>,
    pub(super) non_adjacent_second_best_delta_y: Option<i32>,
    pub(super) non_adjacent_second_best_median: Option<f64>,
    pub(super) no_motion_median: Option<f64>,
    pub(super) separation: Option<f64>,
    pub(super) overlap_rows: u32,
    pub(super) band_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PerceptualMotionConfig {
    pub(super) delta_range: i32,
    pub(super) band_height: u32,
    pub(super) band_step: u32,
    pub(super) bins: usize,
}

#[derive(Debug, Clone)]
pub(super) struct BandSignatures {
    pub(super) bins: usize,
    rows: usize,
    data: Vec<f32>,
}

impl BandSignatures {
    fn get(&self, y: usize) -> Option<&[f32]> {
        if y >= self.rows {
            return None;
        }
        let start = y.checked_mul(self.bins)?;
        Some(&self.data[start..start + self.bins])
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn estimate_vertical_perceptual_motion(
    previous: &RgbFrame,
    current: &RgbFrame,
) -> Option<PerceptualMotionEstimate> {
    let previous = PerceptualFrame::from_source(previous);
    let current = PerceptualFrame::from_source(current);
    estimate_vertical_perceptual_motion_from_frame(&previous, &current)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn estimate_vertical_perceptual_motion_with_config(
    previous: &RgbFrame,
    current: &RgbFrame,
    config: PerceptualMotionConfig,
) -> Option<PerceptualMotionEstimate> {
    let previous = PerceptualFrame::from_source(previous);
    let current = PerceptualFrame::from_source(current);
    estimate_vertical_perceptual_motion_from_frame_with_config(&previous, &current, config)
}

pub(super) fn estimate_vertical_perceptual_motion_from_frame(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
) -> Option<PerceptualMotionEstimate> {
    let config = PerceptualMotionConfig {
        delta_range: PERCEPTUAL_MOTION_DELTA_RANGE,
        band_height: PERCEPTUAL_MOTION_BAND_HEIGHT,
        band_step: PERCEPTUAL_MOTION_BAND_STEP,
        bins: PERCEPTUAL_MOTION_BINS,
    };
    estimate_vertical_perceptual_motion_from_frame_with_config(previous, current, config)
}

#[allow(dead_code)]
pub(super) fn estimate_vertical_perceptual_motion_from_ranked_deltas(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    frame: &impl RgbSource,
    search_direction: Option<SearchDirection>,
    ranked: &[FastMotionCandidate],
) -> (
    Option<PerceptualMotionEstimate>,
    Option<FastMotionVerifyPass>,
) {
    let config = PerceptualMotionConfig {
        delta_range: PERCEPTUAL_MOTION_DELTA_RANGE,
        band_height: PERCEPTUAL_MOTION_BAND_HEIGHT,
        band_step: PERCEPTUAL_MOTION_BAND_STEP,
        bins: PERCEPTUAL_MOTION_BINS,
    };
    for limit in [
        FAST_MOTION_PERCEPTUAL_FIRST_PASS,
        FAST_MOTION_PERCEPTUAL_SECOND_PASS,
    ] {
        let estimate = estimate_vertical_perceptual_motion_from_frame_with_config_and_deltas(
            previous,
            current,
            config,
            ranked_perceptual_delta_pass(ranked, limit),
            false,
        );
        if partial_perceptual_estimate_is_safe(estimate, frame, search_direction) {
            let verify_pass = if limit == FAST_MOTION_PERCEPTUAL_FIRST_PASS {
                FastMotionVerifyPass::Top20
            } else {
                FastMotionVerifyPass::Top50
            };
            return (estimate, Some(verify_pass));
        }
    }
    (None, None)
}

#[allow(dead_code)]
pub(super) fn partial_perceptual_estimate_is_safe(
    estimate: Option<PerceptualMotionEstimate>,
    frame: &impl RgbSource,
    search_direction: Option<SearchDirection>,
) -> bool {
    let Some(estimate) = estimate else {
        return false;
    };
    estimate.non_adjacent_second_best_median.is_some()
        && perceptual_frame_match(Some(estimate), frame, search_direction).is_ok()
}

#[allow(dead_code)]
pub(super) fn ranked_perceptual_delta_pass(
    ranked: &[FastMotionCandidate],
    limit: usize,
) -> impl Iterator<Item = i32> + '_ {
    std::iter::once(0).chain(
        ranked
            .iter()
            .take(limit)
            .map(|candidate| candidate.delta_y)
            .filter(|delta_y| *delta_y != 0),
    )
}

pub(super) fn perceptual_frame_match(
    estimate: Option<PerceptualMotionEstimate>,
    frame: &impl RgbSource,
    search_direction: Option<SearchDirection>,
) -> Result<FrameMatch, &'static str> {
    let estimate = estimate.ok_or("no-estimate")?;
    if estimate.delta_y == 0 {
        return Err("zero-delta");
    }

    let shift = estimate.delta_y.unsigned_abs();
    if shift >= frame.height() {
        return Err("delta-out-of-range");
    }

    let overlap = frame.height() - shift;
    let max_overlap = frame.height().saturating_sub(1);
    if overlap < minimum_overlap(frame.height(), max_overlap) {
        return Err("insufficient-overlap");
    }
    if estimate.overlap_rows < minimum_overlap(frame.height(), max_overlap) {
        return Err("insufficient-estimate-overlap");
    }
    if estimate.overlap_rows < frame.height().div_ceil(PERCEPTUAL_MOTION_MIN_OVERLAP_RATIO) {
        return Err("insufficient-estimate-overlap-ratio");
    }
    if estimate.band_count == 0 {
        return Err("no-bands");
    }
    if estimate.median > MATCH_PREFILTER_MAX_AVERAGE_DIFF
        || estimate.p75 > MATCH_PREFILTER_MAX_AVERAGE_DIFF
        || estimate.p90 > MATCH_PREFILTER_MAX_AVERAGE_DIFF
        || estimate.mean > MATCH_PREFILTER_MAX_AVERAGE_DIFF
    {
        return Err("weak-score");
    }
    if estimate
        .no_motion_median
        .is_some_and(|median| estimate.median + PERCEPTUAL_MOTION_MARGIN >= median)
    {
        return Err("weak-zero-margin");
    }
    // Adjacent deltas around the true offset often score similarly because the
    // band signatures overlap heavily. Only require a margin against the best
    // ranked competitor outside that small neighborhood.
    if estimate
        .non_adjacent_second_best_median
        .is_some_and(|median| estimate.median + PERCEPTUAL_MOTION_MARGIN >= median)
    {
        return Err("weak-second-margin");
    }

    let direction = if estimate.delta_y < 0 {
        AppendDirection::Bottom
    } else {
        AppendDirection::Top
    };
    match (search_direction, direction) {
        (Some(SearchDirection::Down), AppendDirection::Bottom)
        | (Some(SearchDirection::Up), AppendDirection::Top)
        | (None, _) => {}
        (Some(SearchDirection::Left | SearchDirection::Right), _) => {
            return Err("horizontal-direction")
        }
        _ => return Err("direction-mismatch"),
    }

    Ok(FrameMatch {
        direction,
        overlap,
        delta_x: 0,
        delta_y: -estimate.delta_y,
        score: estimate.median,
    })
}

pub(super) fn estimate_vertical_perceptual_motion_from_frame_with_config(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    config: PerceptualMotionConfig,
) -> Option<PerceptualMotionEstimate> {
    estimate_vertical_perceptual_motion_from_frame_with_config_and_deltas(
        previous,
        current,
        config,
        -config.delta_range..=config.delta_range,
        true,
    )
}

pub(super) fn estimate_vertical_perceptual_motion_from_frame_with_config_and_deltas(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    config: PerceptualMotionConfig,
    deltas: impl IntoIterator<Item = i32>,
    scan_missing_non_adjacent: bool,
) -> Option<PerceptualMotionEstimate> {
    if config.delta_range < 0
        || config.band_height == 0
        || config.band_step == 0
        || config.bins == 0
        || previous.width == 0
        || previous.height == 0
        || current.width == 0
        || current.height == 0
        || previous.luminance.len() < previous.width as usize * previous.height as usize
        || current.luminance.len() < current.width as usize * current.height as usize
    {
        return None;
    }

    let width = previous.width.min(current.width);
    if width == 0 || previous.height < config.band_height || current.height < config.band_height {
        return None;
    }

    let previous_signatures = precompute_perceptual_band_signatures(previous, width, config);
    let current_signatures = precompute_perceptual_band_signatures(current, width, config);
    let max_distance_count = previous.height.min(current.height) as usize;
    let mut distances = Vec::with_capacity(max_distance_count);
    let mut candidates = Vec::new();
    let mut no_motion_median: Option<f64> = None;

    for delta_y in deltas {
        if !(-config.delta_range..=config.delta_range).contains(&delta_y) {
            continue;
        }
        if let Some(candidate) = score_perceptual_delta(
            previous,
            current,
            &previous_signatures,
            &current_signatures,
            delta_y,
            config,
            &mut distances,
        ) {
            if delta_y == 0 {
                no_motion_median = Some(candidate.median);
            }
            candidates.push(candidate);
        }
    }

    candidates.sort_by(compare_perceptual_estimates);
    let best = *candidates.first()?;
    let second_best = candidates
        .iter()
        .copied()
        .find(|candidate| candidate.delta_y != best.delta_y);
    let non_adjacent_second_best = candidates
        .iter()
        .copied()
        .find(|candidate| {
            (candidate.delta_y - best.delta_y).abs() > PERCEPTUAL_MOTION_ADJACENT_DELTA
        })
        .or_else(|| {
            scan_missing_non_adjacent
                .then(|| {
                    find_best_non_adjacent_perceptual_candidate(
                        previous,
                        current,
                        &previous_signatures,
                        &current_signatures,
                        config,
                        best.delta_y,
                        &mut distances,
                    )
                })
                .flatten()
        });

    Some(PerceptualMotionEstimate {
        second_best_delta_y: second_best.map(|candidate| candidate.delta_y),
        second_best_median: second_best.map(|candidate| candidate.median),
        non_adjacent_second_best_delta_y: non_adjacent_second_best
            .map(|candidate| candidate.delta_y),
        non_adjacent_second_best_median: non_adjacent_second_best.map(|candidate| candidate.median),
        no_motion_median,
        separation: non_adjacent_second_best.map(|candidate| candidate.median - best.median),
        ..best
    })
}

pub(super) fn compare_perceptual_estimates(
    a: &PerceptualMotionEstimate,
    b: &PerceptualMotionEstimate,
) -> std::cmp::Ordering {
    a.median
        .total_cmp(&b.median)
        .then_with(|| a.p75.total_cmp(&b.p75))
        .then_with(|| b.overlap_rows.cmp(&a.overlap_rows))
        .then_with(|| a.delta_y.abs().cmp(&b.delta_y.abs()))
        .then_with(|| a.delta_y.cmp(&b.delta_y))
}

pub(super) fn score_perceptual_delta(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    previous_signatures: &BandSignatures,
    current_signatures: &BandSignatures,
    delta_y: i32,
    config: PerceptualMotionConfig,
    distances: &mut Vec<f32>,
) -> Option<PerceptualMotionEstimate> {
    let prev_h = previous.height as i32;
    let curr_h = current.height as i32;
    let previous_start = 0.max(delta_y.checked_neg()?);
    let previous_end = prev_h.min(curr_h.checked_sub(delta_y)?);
    if previous_end <= previous_start {
        return None;
    }
    let overlap_rows = u32::try_from(previous_end - previous_start).ok()?;
    if overlap_rows < config.band_height {
        return None;
    }

    distances.clear();
    let max_band_start = overlap_rows - config.band_height;
    for band_offset in (0..=max_band_start).step_by(config.band_step as usize) {
        let previous_y = u32::try_from(previous_start).ok()? + band_offset;
        let current_y = u32::try_from(previous_start.checked_add(delta_y)?).ok()? + band_offset;
        let previous_sig = previous_signatures.get(previous_y as usize)?;
        let current_sig = current_signatures.get(current_y as usize)?;
        distances.push(perceptual_signature_distance(previous_sig, current_sig));
    }
    if distances.is_empty() {
        return None;
    }

    distances.sort_by(f32::total_cmp);
    let mean = distances.iter().map(|v| f64::from(*v)).sum::<f64>() / distances.len() as f64;
    Some(PerceptualMotionEstimate {
        delta_y,
        median: percentile_sorted_f32(distances, 0.50),
        p75: percentile_sorted_f32(distances, 0.75),
        p90: percentile_sorted_f32(distances, 0.90),
        mean,
        second_best_delta_y: None,
        second_best_median: None,
        non_adjacent_second_best_delta_y: None,
        non_adjacent_second_best_median: None,
        no_motion_median: None,
        separation: None,
        overlap_rows,
        band_count: distances.len(),
    })
}

pub(super) fn find_best_non_adjacent_perceptual_candidate(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    previous_signatures: &BandSignatures,
    current_signatures: &BandSignatures,
    config: PerceptualMotionConfig,
    best_delta_y: i32,
    distances: &mut Vec<f32>,
) -> Option<PerceptualMotionEstimate> {
    let mut best_non_adjacent = None;
    for delta_y in -config.delta_range..=config.delta_range {
        if (delta_y - best_delta_y).abs() <= PERCEPTUAL_MOTION_ADJACENT_DELTA {
            continue;
        }
        if let Some(candidate) = score_perceptual_delta(
            previous,
            current,
            previous_signatures,
            current_signatures,
            delta_y,
            config,
            distances,
        ) {
            if best_non_adjacent
                .as_ref()
                .is_none_or(|best| compare_perceptual_estimates(&candidate, best).is_lt())
            {
                best_non_adjacent = Some(candidate);
            }
        }
    }
    best_non_adjacent
}

pub(super) fn precompute_perceptual_band_signatures(
    frame: &PerceptualFrame,
    width: u32,
    config: PerceptualMotionConfig,
) -> BandSignatures {
    if width == 0 || frame.height < config.band_height {
        return BandSignatures {
            bins: config.bins,
            rows: 0,
            data: Vec::new(),
        };
    }

    let bins = config.bins;
    let height = frame.height as usize;
    let band_height = config.band_height as usize;
    let row_count = height - band_height + 1;
    let mut row_sums = vec![0.0f32; height * bins];
    let mut bin_counts = vec![0u32; bins];

    for row in 0..frame.height {
        for x in 0..width {
            let bin = (x as usize * bins) / width as usize;
            row_sums[row as usize * bins + bin] += perceptual_frame_luminance(frame, x, row);
            if row == 0 {
                bin_counts[bin] += 1;
            }
        }
    }

    let mut window_sums = vec![0.0f32; bins];
    for row in 0..band_height {
        let row_start = row * bins;
        for bin in 0..bins {
            window_sums[bin] += row_sums[row_start + bin];
        }
    }

    let mut data = vec![0.0f32; row_count * bins];
    for y in 0..row_count {
        let output_start = y * bins;
        for bin in 0..bins {
            let count = bin_counts[bin] as f32 * config.band_height as f32;
            if count > 0.0 {
                data[output_start + bin] = window_sums[bin] / count;
            }
        }

        let next_row = y + band_height;
        if next_row < height {
            let old_start = y * bins;
            let next_start = next_row * bins;
            for bin in 0..bins {
                window_sums[bin] += row_sums[next_start + bin] - row_sums[old_start + bin];
            }
        }
    }

    BandSignatures {
        bins,
        rows: row_count,
        data,
    }
}

pub(super) fn perceptual_frame_luminance(frame: &PerceptualFrame, x: u32, y: u32) -> f32 {
    frame.luminance(x, y)
}

pub(super) fn perceptual_signature_distance(a: &[f32], b: &[f32]) -> f32 {
    let bins = a.len().min(b.len());
    if bins == 0 {
        return f32::MAX;
    }
    a.iter().zip(b).map(|(a, b)| (a - b).abs()).sum::<f32>() / bins as f32
}

pub(super) fn percentile_sorted_f32(values: &[f32], percentile: f64) -> f64 {
    if values.is_empty() {
        return f64::MAX;
    }
    let rank = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    f64::from(values[rank])
}
