use std::collections::HashMap;

use super::canvas::{canvas_overlap_rect, validate_stitched_frame};
use super::rgb::{validate_rgb_source, RgbSource};
use super::{
    line_average_difference, minimum_overlap, AppendDirection, FrameMatch, SearchDirection,
    StitchedFrame, ViewportRect, MATCH_PREFILTER_MAX_AVERAGE_DIFF,
};

const CANVAS_PLACEMENT_TOP_CANDIDATES: usize = 12;
const CANVAS_PLACEMENT_FAST_SAMPLE_STEP: u32 = 12;
const CANVAS_PLACEMENT_CONFIRM_SAMPLE_STEP: u32 = 2;
const CANVAS_PLACEMENT_LOCAL_RADIUS: i32 = 192;
const CANVAS_PLACEMENT_LOCAL_GOOD_ENOUGH_SCORE: f64 = 3.0;
const CANVAS_PLACEMENT_LOCAL_MIN_MARGIN: f64 = 1.0;
const CANVAS_PLACEMENT_LOCAL_RADII: [i32; 4] = [32, 64, 128, CANVAS_PLACEMENT_LOCAL_RADIUS];
const CANVAS_PLACEMENT_GLOBAL_COARSE_STEP: u32 = 24;
const CANVAS_PLACEMENT_GLOBAL_REFINE_RADIUS: i32 = CANVAS_PLACEMENT_GLOBAL_COARSE_STEP as i32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy)]
struct CanvasPlacementCandidate {
    frame_x: i32,
    frame_y: i32,
    overlap_area: u64,
    score: f64,
}

#[derive(Default)]
struct CanvasScoreCache {
    overlap_rects: HashMap<(i32, i32), Option<(u32, u32, u32, u32)>>,
    scores: HashMap<(i32, i32, u32), Option<f64>>,
}

impl CanvasScoreCache {
    fn overlap_rect(
        &mut self,
        stitched: &StitchedFrame,
        frame: &impl RgbSource,
        frame_x: i32,
        frame_y: i32,
    ) -> Option<(u32, u32, u32, u32)> {
        let key = (frame_x, frame_y);
        if let Some(overlap_rect) = self.overlap_rects.get(&key) {
            return *overlap_rect;
        }
        let overlap_rect = canvas_overlap_rect(stitched, frame, frame_x, frame_y);
        self.overlap_rects.insert(key, overlap_rect);
        overlap_rect
    }

    fn score(
        &mut self,
        stitched: &StitchedFrame,
        frame: &impl RgbSource,
        frame_x: i32,
        frame_y: i32,
        sample_step: u32,
    ) -> Option<f64> {
        let key = (frame_x, frame_y, sample_step);
        if let Some(score) = self.scores.get(&key) {
            return *score;
        }
        let overlap_rect = self.overlap_rect(stitched, frame, frame_x, frame_y);
        let score = score_canvas_overlap_with_rect(
            stitched,
            frame,
            frame_x,
            frame_y,
            sample_step,
            overlap_rect,
        );
        self.scores.insert(key, score);
        score
    }
}

pub(super) fn canvas_search_axis(search_direction: Option<SearchDirection>) -> SearchAxis {
    match search_direction {
        Some(SearchDirection::Right | SearchDirection::Left) => SearchAxis::Horizontal,
        Some(SearchDirection::Down | SearchDirection::Up) | None => SearchAxis::Vertical,
    }
}

pub(super) fn find_canvas_placement_match(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &impl RgbSource,
    axis: SearchAxis,
    search_direction: Option<SearchDirection>,
) -> Option<FrameMatch> {
    if validate_stitched_frame(stitched).is_err()
        || validate_rgb_source(frame).is_err()
        || stitched.width > i32::MAX as u32
        || stitched.height > i32::MAX as u32
        || frame.width() > i32::MAX as u32
        || frame.height() > i32::MAX as u32
    {
        return None;
    }

    let mut candidates = Vec::new();
    let min_overlap = match axis {
        SearchAxis::Vertical => minimum_overlap(
            frame.height().min(stitched.height),
            frame.height().min(stitched.height),
        ),
        SearchAxis::Horizontal => minimum_overlap(
            frame.width().min(stitched.width),
            frame.width().min(stitched.width),
        ),
    };
    if min_overlap == 0 {
        return None;
    }

    let mut score_cache = CanvasScoreCache::default();

    let (global_start, global_end, local_center) = match axis {
        SearchAxis::Vertical => (
            -(frame.height() as i32) + min_overlap as i32,
            stitched.height as i32 - min_overlap as i32,
            previous_rect.y,
        ),
        SearchAxis::Horizontal => (
            -(frame.width() as i32) + min_overlap as i32,
            stitched.width as i32 - min_overlap as i32,
            previous_rect.x,
        ),
    };
    if let Some(best) = find_canvas_placement_match_local(
        stitched,
        frame,
        axis,
        previous_rect,
        search_direction,
        min_overlap,
        global_start,
        global_end,
        local_center,
        &mut score_cache,
        &mut candidates,
    ) {
        return Some(best);
    }

    candidates.clear();
    if collect_canvas_candidates_stepped(
        stitched,
        frame,
        axis,
        previous_rect,
        search_direction,
        min_overlap,
        global_start,
        global_end,
        CANVAS_PLACEMENT_GLOBAL_COARSE_STEP,
        &mut score_cache,
        &mut candidates,
    ) {
        let refined = collect_canvas_candidate_neighborhoods(
            stitched,
            frame,
            axis,
            previous_rect,
            search_direction,
            min_overlap,
            global_start,
            global_end,
            &mut score_cache,
            &candidates,
        );
        if let Some(best) = select_canvas_candidate(
            stitched,
            frame,
            previous_rect,
            axis,
            &mut score_cache,
            &refined,
        ) {
            return Some(best.match_info);
        }
    }

    candidates.clear();
    collect_canvas_candidates(
        stitched,
        frame,
        axis,
        previous_rect,
        search_direction,
        min_overlap,
        global_start,
        global_end,
        &mut score_cache,
        &mut candidates,
    );
    select_canvas_candidate(
        stitched,
        frame,
        previous_rect,
        axis,
        &mut score_cache,
        &candidates,
    )
    .map(|selection| selection.match_info)
}

#[allow(clippy::too_many_arguments)]
fn find_canvas_placement_match_local(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    min_overlap: u32,
    global_start: i32,
    global_end: i32,
    local_center: i32,
    score_cache: &mut CanvasScoreCache,
    candidates: &mut Vec<CanvasPlacementCandidate>,
) -> Option<FrameMatch> {
    let mut previous_radius = 0;
    for radius in CANVAS_PLACEMENT_LOCAL_RADII {
        let local_start = global_start.max(local_center.saturating_sub(radius));
        let local_end = global_end.min(local_center.saturating_add(radius));
        if local_start > local_end {
            continue;
        }

        candidates.clear();
        collect_canvas_candidates_ring(
            stitched,
            frame,
            axis,
            previous_rect,
            search_direction,
            min_overlap,
            local_center,
            previous_radius,
            radius,
            local_start,
            local_end,
            score_cache,
            candidates,
        );
        previous_radius = radius;

        if let Some(best) = select_canvas_candidate(
            stitched,
            frame,
            previous_rect,
            axis,
            score_cache,
            candidates,
        ) {
            if best.match_info.score <= CANVAS_PLACEMENT_LOCAL_GOOD_ENOUGH_SCORE
                && canvas_selection_has_clear_margin(best.match_info.score, best.second_score)
            {
                return Some(best.match_info);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn collect_canvas_candidates(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    min_overlap: u32,
    start: i32,
    end: i32,
    score_cache: &mut CanvasScoreCache,
    candidates: &mut Vec<CanvasPlacementCandidate>,
) {
    collect_canvas_candidates_stepped(
        stitched,
        frame,
        axis,
        previous_rect,
        search_direction,
        min_overlap,
        start,
        end,
        1,
        score_cache,
        candidates,
    );
}

#[allow(clippy::too_many_arguments)]
fn collect_canvas_candidates_stepped(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    min_overlap: u32,
    start: i32,
    end: i32,
    position_step: u32,
    score_cache: &mut CanvasScoreCache,
    candidates: &mut Vec<CanvasPlacementCandidate>,
) -> bool {
    if position_step == 0 || start > end {
        return false;
    }

    let mut any = false;
    match axis {
        SearchAxis::Vertical => {
            let frame_x = previous_rect.x;
            for frame_y in (start..=end).step_by(position_step as usize) {
                push_canvas_candidate(
                    stitched,
                    frame,
                    axis,
                    frame_x,
                    frame_y,
                    min_overlap,
                    CANVAS_PLACEMENT_FAST_SAMPLE_STEP,
                    previous_rect,
                    search_direction,
                    score_cache,
                    candidates,
                );
                any = true;
            }
        }
        SearchAxis::Horizontal => {
            let frame_y = previous_rect.y;
            for frame_x in (start..=end).step_by(position_step as usize) {
                push_canvas_candidate(
                    stitched,
                    frame,
                    axis,
                    frame_x,
                    frame_y,
                    min_overlap,
                    CANVAS_PLACEMENT_FAST_SAMPLE_STEP,
                    previous_rect,
                    search_direction,
                    score_cache,
                    candidates,
                );
                any = true;
            }
        }
    }
    any
}

#[allow(clippy::too_many_arguments)]
fn collect_canvas_candidates_ring(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    min_overlap: u32,
    center: i32,
    previous_radius: i32,
    radius: i32,
    start: i32,
    end: i32,
    score_cache: &mut CanvasScoreCache,
    candidates: &mut Vec<CanvasPlacementCandidate>,
) {
    let left_start = start;
    let left_end = end.min(center.saturating_sub(previous_radius + 1));
    let right_start = start.max(center.saturating_add(previous_radius + 1));
    let right_end = end;

    if previous_radius == 0 && start <= center && center <= end {
        collect_canvas_candidates(
            stitched,
            frame,
            axis,
            previous_rect,
            search_direction,
            min_overlap,
            center,
            center,
            score_cache,
            candidates,
        );
    }

    if left_start <= left_end {
        let left_bound = left_start.max(center.saturating_sub(radius));
        if left_bound <= left_end {
            collect_canvas_candidates(
                stitched,
                frame,
                axis,
                previous_rect,
                search_direction,
                min_overlap,
                left_bound,
                left_end,
                score_cache,
                candidates,
            );
        }
    }

    if right_start <= right_end {
        let right_bound = right_end.min(center.saturating_add(radius));
        if right_start <= right_bound {
            collect_canvas_candidates(
                stitched,
                frame,
                axis,
                previous_rect,
                search_direction,
                min_overlap,
                right_start,
                right_bound,
                score_cache,
                candidates,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_canvas_candidate_neighborhoods(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    min_overlap: u32,
    global_start: i32,
    global_end: i32,
    score_cache: &mut CanvasScoreCache,
    coarse_candidates: &[CanvasPlacementCandidate],
) -> Vec<CanvasPlacementCandidate> {
    let mut coarse = coarse_candidates.to_vec();
    coarse.sort_by(compare_canvas_candidates);
    coarse.truncate(CANVAS_PLACEMENT_TOP_CANDIDATES);

    let mut refined = Vec::new();
    for candidate in coarse {
        let moving_position = match axis {
            SearchAxis::Vertical => candidate.frame_y,
            SearchAxis::Horizontal => candidate.frame_x,
        };
        let refine_start =
            global_start.max(moving_position - CANVAS_PLACEMENT_GLOBAL_REFINE_RADIUS);
        let refine_end = global_end.min(moving_position + CANVAS_PLACEMENT_GLOBAL_REFINE_RADIUS);
        collect_canvas_candidates(
            stitched,
            frame,
            axis,
            previous_rect,
            search_direction,
            min_overlap,
            refine_start,
            refine_end,
            score_cache,
            &mut refined,
        );
    }
    refined
}

fn select_canvas_candidate(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    previous_rect: ViewportRect,
    axis: SearchAxis,
    score_cache: &mut CanvasScoreCache,
    candidates: &[CanvasPlacementCandidate],
) -> Option<CanvasPlacementSelection> {
    let mut candidates = candidates.to_vec();
    candidates.sort_by(compare_canvas_candidates);
    candidates.truncate(CANVAS_PLACEMENT_TOP_CANDIDATES);

    let mut best: Option<CanvasPlacementCandidate> = None;
    let mut second: Option<CanvasPlacementCandidate> = None;
    for candidate in candidates {
        let confirmed = score_cache.score(
            stitched,
            frame,
            candidate.frame_x,
            candidate.frame_y,
            CANVAS_PLACEMENT_CONFIRM_SAMPLE_STEP,
        )?;
        let confirmed = CanvasPlacementCandidate {
            score: confirmed,
            ..candidate
        };
        if best.is_none_or(|best| compare_canvas_candidates(&confirmed, &best).is_lt()) {
            second = best;
            best = Some(confirmed);
        } else if second.is_none_or(|second| compare_canvas_candidates(&confirmed, &second).is_lt())
        {
            second = Some(confirmed);
        }
    }

    let best = best?;
    if best.score > MATCH_PREFILTER_MAX_AVERAGE_DIFF {
        return None;
    }
    Some(CanvasPlacementSelection {
        match_info: canvas_candidate_frame_match(stitched, previous_rect, frame, axis, best)?,
        second_score: second.map(|candidate| candidate.score),
    })
}

#[derive(Debug, Clone, Copy)]
struct CanvasPlacementSelection {
    match_info: FrameMatch,
    second_score: Option<f64>,
}

fn canvas_selection_has_clear_margin(best_score: f64, second_score: Option<f64>) -> bool {
    second_score.is_none_or(|score| best_score + CANVAS_PLACEMENT_LOCAL_MIN_MARGIN < score)
}

#[allow(clippy::too_many_arguments)]
fn push_canvas_candidate(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    axis: SearchAxis,
    frame_x: i32,
    frame_y: i32,
    min_overlap: u32,
    sample_step: u32,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
    score_cache: &mut CanvasScoreCache,
    candidates: &mut Vec<CanvasPlacementCandidate>,
) {
    if !canvas_candidate_matches_direction(frame_x, frame_y, previous_rect, search_direction) {
        return;
    }
    let Some((overlap_x0, overlap_y0, overlap_x1, overlap_y1)) =
        score_cache.overlap_rect(stitched, frame, frame_x, frame_y)
    else {
        return;
    };
    let moving_overlap = match axis {
        SearchAxis::Vertical => overlap_y1 - overlap_y0,
        SearchAxis::Horizontal => overlap_x1 - overlap_x0,
    };
    if moving_overlap < min_overlap {
        return;
    }
    let Some(score) = score_cache.score(stitched, frame, frame_x, frame_y, sample_step) else {
        return;
    };
    candidates.push(CanvasPlacementCandidate {
        frame_x,
        frame_y,
        overlap_area: u64::from(overlap_x1 - overlap_x0) * u64::from(overlap_y1 - overlap_y0),
        score,
    });
}

fn canvas_candidate_matches_direction(
    frame_x: i32,
    frame_y: i32,
    previous_rect: ViewportRect,
    search_direction: Option<SearchDirection>,
) -> bool {
    match search_direction {
        None => true,
        Some(SearchDirection::Down) => frame_y > previous_rect.y,
        Some(SearchDirection::Up) => frame_y < previous_rect.y,
        Some(SearchDirection::Right) => frame_x > previous_rect.x,
        Some(SearchDirection::Left) => frame_x < previous_rect.x,
    }
}

fn canvas_candidate_frame_match(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &impl RgbSource,
    axis: SearchAxis,
    candidate: CanvasPlacementCandidate,
) -> Option<FrameMatch> {
    let (overlap_x0, overlap_y0, overlap_x1, overlap_y1) =
        canvas_overlap_rect(stitched, frame, candidate.frame_x, candidate.frame_y)?;
    let delta_x = candidate.frame_x.checked_sub(previous_rect.x)?;
    let delta_y = candidate.frame_y.checked_sub(previous_rect.y)?;
    let direction = if candidate.frame_y < 0 {
        AppendDirection::Top
    } else if candidate.frame_y.checked_add(frame.height() as i32)? > stitched.height as i32 {
        AppendDirection::Bottom
    } else if candidate.frame_x < 0 {
        AppendDirection::Left
    } else if candidate.frame_x.checked_add(frame.width() as i32)? > stitched.width as i32 {
        AppendDirection::Right
    } else {
        match axis {
            SearchAxis::Vertical if delta_y < 0 => AppendDirection::Top,
            SearchAxis::Vertical => AppendDirection::Bottom,
            SearchAxis::Horizontal if delta_x < 0 => AppendDirection::Left,
            SearchAxis::Horizontal => AppendDirection::Right,
        }
    };
    let overlap = match axis {
        SearchAxis::Vertical => overlap_y1 - overlap_y0,
        SearchAxis::Horizontal => overlap_x1 - overlap_x0,
    };
    Some(FrameMatch {
        direction,
        overlap,
        delta_x,
        delta_y,
        score: candidate.score,
    })
}

fn compare_canvas_candidates(
    a: &CanvasPlacementCandidate,
    b: &CanvasPlacementCandidate,
) -> std::cmp::Ordering {
    a.score
        .total_cmp(&b.score)
        .then_with(|| b.overlap_area.cmp(&a.overlap_area))
        .then_with(|| a.frame_y.abs().cmp(&b.frame_y.abs()))
        .then_with(|| a.frame_x.abs().cmp(&b.frame_x.abs()))
}

fn score_canvas_overlap_with_rect(
    stitched: &StitchedFrame,
    frame: &impl RgbSource,
    frame_x: i32,
    frame_y: i32,
    sample_step: u32,
    overlap_rect: Option<(u32, u32, u32, u32)>,
) -> Option<f64> {
    let (x0, y0, x1, y1) = overlap_rect?;
    let mut total = 0u64;
    let mut samples = 0u32;
    for stitched_y in (y0..y1).step_by(sample_step as usize) {
        for stitched_x in (x0..x1).step_by(sample_step as usize) {
            let frame_px = u32::try_from(stitched_x as i32 - frame_x).ok()?;
            let frame_py = u32::try_from(stitched_y as i32 - frame_y).ok()?;
            total += pixel_difference_stitched_source(
                stitched, stitched_x, stitched_y, frame, frame_px, frame_py,
            ) as u64;
            samples += 1;
        }
    }
    Some(line_average_difference(total, samples))
}

fn pixel_difference_stitched_source(
    stitched: &StitchedFrame,
    stitched_x: u32,
    stitched_y: u32,
    frame: &impl RgbSource,
    frame_x: u32,
    frame_y: u32,
) -> u32 {
    let stitched_offset = stitched_y as usize * stitched.stride as usize + stitched_x as usize * 3;
    let a = [
        stitched.data[stitched_offset],
        stitched.data[stitched_offset + 1],
        stitched.data[stitched_offset + 2],
    ];
    let b = frame.pixel_rgb(frame_x, frame_y);
    (i32::from(a[0]) - i32::from(b[0])).unsigned_abs()
        + (i32::from(a[1]) - i32::from(b[1])).unsigned_abs()
        + (i32::from(a[2]) - i32::from(b[2])).unsigned_abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_canvas_accept_requires_clear_score_margin() {
        assert!(canvas_selection_has_clear_margin(0.0, Some(1.1)));
        assert!(!canvas_selection_has_clear_margin(0.0, Some(1.0)));
        assert!(!canvas_selection_has_clear_margin(0.45, Some(1.2)));
    }

    #[test]
    fn local_canvas_accept_allows_single_candidate() {
        assert!(canvas_selection_has_clear_margin(0.45, None));
    }
}
