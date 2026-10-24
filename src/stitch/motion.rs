use super::perceptual::{PerceptualFrame, PERCEPTUAL_MOTION_DELTA_RANGE};
use super::{minimum_overlap, FAST_MOTION_MAX_SCORE};
use crate::stitch::{FastMotionCandidate, SearchDirection};

const FAST_MOTION_DELTA_RANGE: i32 = PERCEPTUAL_MOTION_DELTA_RANGE;
const FAST_MOTION_ROW_STEP: u32 = 8;
const FAST_MOTION_COLUMN_STEP: u32 = 16;
const FAST_MOTION_MIN_MARGIN: f64 = 1.0;

#[derive(Debug, Clone)]
pub(super) struct FastMotionScan {
    pub(super) candidate: Option<FastMotionCandidate>,
    #[allow(dead_code)]
    pub(super) ranked: Vec<FastMotionCandidate>,
}

pub(super) fn scan_fast_vertical_motion(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    search_direction: Option<SearchDirection>,
) -> Option<FastMotionScan> {
    let allowed_deltas: &[std::ops::RangeInclusive<i32>] = match search_direction {
        None => &[-FAST_MOTION_DELTA_RANGE..=-1, 1..=FAST_MOTION_DELTA_RANGE],
        Some(SearchDirection::Down) => &[-FAST_MOTION_DELTA_RANGE..=-1],
        Some(SearchDirection::Up) => &[1..=FAST_MOTION_DELTA_RANGE],
        Some(SearchDirection::Right | SearchDirection::Left) => return None,
    };
    let mut ranked = Vec::new();
    for range in allowed_deltas {
        for delta_y in range.clone() {
            let Some(candidate) = score_fast_motion_delta(previous, current, delta_y) else {
                continue;
            };
            ranked.push(candidate);
        }
    }
    ranked.sort_by(compare_fast_motion_candidates);
    let best = ranked.first().copied();
    let second_best_score = ranked
        .iter()
        .skip(1)
        .map(|candidate| candidate.score)
        .fold(f64::INFINITY, f64::min);
    let candidate = best.filter(|best| {
        best.score <= FAST_MOTION_MAX_SCORE
            && (!second_best_score.is_finite()
                || best.score + FAST_MOTION_MIN_MARGIN < second_best_score)
    });

    Some(FastMotionScan { candidate, ranked })
}

pub(super) fn compare_fast_motion_candidates(
    a: &FastMotionCandidate,
    b: &FastMotionCandidate,
) -> std::cmp::Ordering {
    a.score
        .total_cmp(&b.score)
        .then_with(|| b.overlap_rows.cmp(&a.overlap_rows))
        .then_with(|| a.delta_y.abs().cmp(&b.delta_y.abs()))
        .then_with(|| a.delta_y.cmp(&b.delta_y))
}

pub(super) fn score_fast_motion_delta(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    delta_y: i32,
) -> Option<FastMotionCandidate> {
    if delta_y == 0 || previous.width == 0 || current.width == 0 {
        return None;
    }
    let width = previous.width.min(current.width);
    let prev_h = i32::try_from(previous.height).ok()?;
    let curr_h = i32::try_from(current.height).ok()?;
    let previous_start = 0.max(delta_y.checked_neg()?);
    let previous_end = prev_h.min(curr_h.checked_sub(delta_y)?);
    if previous_end <= previous_start {
        return None;
    }
    let overlap_rows = u32::try_from(previous_end - previous_start).ok()?;
    let max_overlap = previous.height.min(current.height).saturating_sub(1);
    if overlap_rows < minimum_overlap(previous.height.min(current.height), max_overlap) {
        return None;
    }

    let mut total = 0.0;
    let mut samples = 0u64;
    let previous_start = u32::try_from(previous_start).ok()?;
    let previous_end = u32::try_from(previous_end).ok()?;
    for previous_y in (previous_start..previous_end).step_by(FAST_MOTION_ROW_STEP as usize) {
        let current_y =
            u32::try_from(i32::try_from(previous_y).ok()?.checked_add(delta_y)?).ok()?;
        for x in (0..width).step_by(FAST_MOTION_COLUMN_STEP as usize) {
            total += f64::from(
                (previous.luminance(x, previous_y) - current.luminance(x, current_y)).abs(),
            );
            samples += 1;
        }
    }
    if samples == 0 {
        return None;
    }
    Some(FastMotionCandidate {
        delta_y,
        score: total / samples as f64,
        overlap_rows,
    })
}
