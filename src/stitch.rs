use std::error::Error;
use std::time::{Duration, Instant};

#[cfg(test)]
use wayland_client::protocol::wl_shm::Format;

#[cfg(test)]
use crate::image::Image;
use crate::wayland::selection::LongDirection;

mod canvas;
mod fixed_bands;
mod hash;
mod motion;
mod perceptual;
mod placement;
mod rgb;
mod types;

use canvas::{
    append_frame_at_position, append_frame_at_position_overwriting_overlap_rows,
    extract_stitched_region, translate_viewport_rect, viewport_rect_within_stitched,
};
#[cfg(test)]
use fixed_bands::detect_fixed_bands;
use fixed_bands::{FixedBandDetector, FixedBandObservation};
use hash::estimate_vertical_perceptual_hash_motion_from_frame;
use motion::scan_fast_vertical_motion;
#[cfg(test)]
use motion::{compare_fast_motion_candidates, score_fast_motion_delta};
#[cfg(test)]
use perceptual::estimate_vertical_perceptual_motion_from_frame;
#[cfg(test)]
use perceptual::{
    estimate_vertical_perceptual_motion, estimate_vertical_perceptual_motion_from_ranked_deltas,
    estimate_vertical_perceptual_motion_with_config, precompute_perceptual_band_signatures,
    score_perceptual_delta, PerceptualMotionConfig, FAST_MOTION_PERCEPTUAL_FIRST_PASS,
    FAST_MOTION_PERCEPTUAL_SECOND_PASS, PERCEPTUAL_MOTION_BAND_HEIGHT, PERCEPTUAL_MOTION_MARGIN,
};
use perceptual::{
    perceptual_frame_match, PerceptualFrame, PerceptualMotionEstimate,
    PERCEPTUAL_MOTION_ADJACENT_DELTA,
};
#[cfg(test)]
use placement::SearchAxis;
use placement::{canvas_search_axis, find_canvas_placement_match};
use rgb::{
    average_difference_same_source, average_luminance_difference_source, crop_rgb_frame,
    pixel_difference_source, pixel_offset, rgb_frame_from_source, validate_rgb_frame,
    validate_rgb_source, CroppedRgbSource, RgbSource,
};

pub use canvas::image_from_stitched_frame;
#[allow(unused_imports)]
pub use hash::{
    estimate_vertical_fuzzy_hash_motion, estimate_vertical_lazy_fuzzy_hash_motion,
    estimate_vertical_lazy_multi_level_fuzzy_hash_motion,
    estimate_vertical_multi_level_fuzzy_hash_motion,
    estimate_vertical_multi_level_fuzzy_hash_motion_with_stats, FuzzyHashMotionStats,
};
#[allow(unused_imports)]
pub use rgb::{average_difference_same, rgb_frame_from_image, ImageRgbView};
pub use types::{
    AppendDirection, ComposeCrop, DuplicateAnalysisMotion, FastMotionAgreement,
    FastMotionCandidate, FastMotionTrace, FastMotionVerifyPass, FixedBands, FrameMatch,
    MotionEstimate, PushResult, PushStreakKind, RgbFrame, SearchDirection, StitchDecisionPath,
    StitchProfileBreakdown, StitchedFrame, ViewportRect,
};

const MATCH_LINE_MAX_AVERAGE_DIFF: f64 = 3.0;
const MATCH_LINE_SAMPLE_STEP: u32 = 4;
const MATCH_PREFILTER_MAX_AVERAGE_DIFF: f64 = 12.0;
const MATCH_PREFILTER_SAMPLE_STEP: u32 = 2;
const MATCH_MIN_OVERLAP_PIXELS: u32 = 16;
const FAST_MOTION_MAX_SCORE: f64 = 8.0;
const DUPLICATE_ANALYSIS_ROW_DIFF_THRESHOLD: f64 = 3.0;
const NOMATCH_RECOVERY_MIN_STABLE_ROWS: u32 = 8;
const NOMATCH_RECOVERY_MIN_DIRTY_ROWS: u32 = 4;
const NOMATCH_RECOVERY_SMALL_DIRTY_ROWS: u32 = 64;
const NOMATCH_RECOVERY_MAX_DIRTY_ROWS: u32 = 192;
const NOMATCH_RECOVERY_MAX_DIRTY_RATIO: f64 = 0.35;
const NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF: f64 = 8.0;
const NOMATCH_RECOVERY_RAW_STABLE_MAX_PIXEL_DIFF: u32 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementSource {
    Canvas,
    Perceptual,
    PreviousFrameFallback,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FramePlacement {
    match_info: FrameMatch,
    source: PlacementSource,
    fast_motion_trace: Option<FastMotionTrace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PlacementProfile {
    path: Option<StitchDecisionPath>,
    canvas_match: Duration,
    perceptual_prepare: Duration,
    perceptual_match: Duration,
    fallback_match: Duration,
}

#[derive(Debug, Clone)]
struct PlacementOutcome {
    candidates: Vec<FramePlacement>,
    profile: PlacementProfile,
    current_perceptual_frame: Option<PerceptualFrame>,
    perceptual_estimate: Option<PerceptualMotionEstimate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct AcceptedDirectionMemory {
    direction: Option<AppendDirection>,
    streak: u32,
}

impl AcceptedDirectionMemory {
    fn record(&mut self, direction: AppendDirection) {
        if self.direction == Some(direction) {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.direction = Some(direction);
            self.streak = 1;
        }
    }

    fn vertical_direction(self) -> Option<AppendDirection> {
        match (self.direction, self.streak) {
            (Some(AppendDirection::Bottom | AppendDirection::Top), streak) if streak > 0 => {
                self.direction
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NoMatchRecovery {
    match_info: FrameMatch,
    overwrite_frame_y: u32,
    overwrite_rows: u32,
}

struct PlacementInput<'a, C, A>
where
    C: RgbSource,
    A: RgbSource,
{
    stitched: &'a StitchedFrame,
    previous_rect: ViewportRect,
    previous_region: &'a RgbFrame,
    active_compose: &'a C,
    analysis_frame: &'a A,
    previous_perceptual_frame: Option<&'a PerceptualFrame>,
    search_direction: Option<SearchDirection>,
    profile_enabled: bool,
}

#[derive(Debug, Default, Clone)]
struct DefaultPlacementPipeline;

impl DefaultPlacementPipeline {
    fn place<C, A>(&mut self, input: PlacementInput<'_, C, A>) -> PlacementOutcome
    where
        C: RgbSource,
        A: RgbSource,
    {
        let fast_motion_verify_pass = None;
        let mut profile = PlacementProfile::default();
        let mut candidates = Vec::new();
        let canvas_started_at = input.profile_enabled.then(Instant::now);
        if let Some(match_info) = find_canvas_placement_match(
            input.stitched,
            input.previous_rect,
            input.active_compose,
            canvas_search_axis(input.search_direction),
            input.search_direction,
        ) {
            if let Some(started_at) = canvas_started_at {
                profile.canvas_match += started_at.elapsed();
            }
            profile.path = Some(StitchDecisionPath::CanvasAccepted);
            return PlacementOutcome {
                candidates: vec![FramePlacement {
                    match_info,
                    source: PlacementSource::Canvas,
                    fast_motion_trace: placement_fast_motion_trace(
                        None,
                        Some(match_info),
                        false,
                        fast_motion_verify_pass,
                    ),
                }],
                profile,
                current_perceptual_frame: None,
                perceptual_estimate: None,
            };
        } else if let Some(started_at) = canvas_started_at {
            profile.canvas_match += started_at.elapsed();
        }

        let perceptual_started_at = input.profile_enabled.then(Instant::now);
        let current_perceptual_frame = PerceptualFrame::from_source(input.analysis_frame);
        let fast_motion_enabled =
            crate::trace::trace_fast_motion_enabled() || crate::trace::fast_motion_accept_enabled();
        let fast_motion_scan = if fast_motion_enabled {
            input.previous_perceptual_frame.and_then(|previous| {
                scan_fast_vertical_motion(
                    previous,
                    &current_perceptual_frame,
                    input.search_direction,
                )
            })
        } else {
            None
        };
        let fast_motion_candidate = fast_motion_scan.as_ref().and_then(|scan| scan.candidate);
        let perceptual_estimate = input.previous_perceptual_frame.and_then(|previous| {
            estimate_vertical_perceptual_hash_motion_from_frame(previous, &current_perceptual_frame)
        });
        if let Some(started_at) = perceptual_started_at {
            profile.perceptual_prepare += started_at.elapsed();
        }

        let perceptual_match_started_at = input.profile_enabled.then(Instant::now);
        match perceptual_frame_match(
            perceptual_estimate,
            input.active_compose,
            input.search_direction,
        ) {
            Ok(match_info) => {
                if let Some(started_at) = perceptual_match_started_at {
                    profile.perceptual_match += started_at.elapsed();
                }
                profile.path = Some(StitchDecisionPath::PerceptualAccepted);
                candidates.push(FramePlacement {
                    match_info,
                    source: PlacementSource::Perceptual,
                    fast_motion_trace: placement_fast_motion_trace(
                        fast_motion_candidate,
                        Some(match_info),
                        false,
                        fast_motion_verify_pass,
                    ),
                });
            }
            Err(reason) => {
                #[cfg(not(feature = "trace-logs"))]
                let _ = reason;
                if crate::trace::trace_verbose_enabled() {
                    crate::trace_log!("stitch: perceptual rejected reason={}", reason);
                }
            }
        }
        if profile.perceptual_match == Duration::default() {
            if let Some(started_at) = perceptual_match_started_at {
                profile.perceptual_match += started_at.elapsed();
            }
        }

        let fallback_started_at = input.profile_enabled.then(Instant::now);
        let match_info = find_frame_shift_match_source(
            input.previous_region,
            input.active_compose,
            input.search_direction,
        );
        if let Some(started_at) = fallback_started_at {
            profile.fallback_match += started_at.elapsed();
        }
        if match_info.overlap == 0 {
            if candidates.is_empty() {
                profile.path = Some(StitchDecisionPath::NoMatch);
            }
            return PlacementOutcome {
                candidates,
                profile,
                current_perceptual_frame: Some(current_perceptual_frame),
                perceptual_estimate,
            };
        }

        if candidates.is_empty() {
            profile.path = Some(StitchDecisionPath::FallbackAccepted);
        }
        candidates.push(FramePlacement {
            match_info,
            source: PlacementSource::PreviousFrameFallback,
            fast_motion_trace: placement_fast_motion_trace(
                fast_motion_candidate,
                Some(match_info),
                true,
                fast_motion_verify_pass,
            ),
        });
        PlacementOutcome {
            candidates,
            profile,
            current_perceptual_frame: Some(current_perceptual_frame),
            perceptual_estimate,
        }
    }
}

fn placement_fast_motion_trace(
    candidate: Option<FastMotionCandidate>,
    match_info: Option<FrameMatch>,
    fallback: bool,
    verify_pass: Option<FastMotionVerifyPass>,
) -> Option<FastMotionTrace> {
    (crate::trace::trace_fast_motion_enabled() || crate::trace::fast_motion_accept_enabled())
        .then(|| compare_fast_motion_candidate(candidate, match_info, fallback, verify_pass))
}

#[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
fn placement_source_label(source: PlacementSource) -> &'static str {
    match source {
        PlacementSource::Canvas => "canvas",
        PlacementSource::Perceptual => "perceptual",
        PlacementSource::PreviousFrameFallback => "fallback",
    }
}

fn placement_path_for_source(source: PlacementSource) -> Option<StitchDecisionPath> {
    Some(match source {
        PlacementSource::Canvas => StitchDecisionPath::CanvasAccepted,
        PlacementSource::Perceptual => StitchDecisionPath::PerceptualAccepted,
        PlacementSource::PreviousFrameFallback => StitchDecisionPath::FallbackAccepted,
    })
}

impl PartialEq for FrameMatch {
    fn eq(&self, other: &Self) -> bool {
        self.direction == other.direction
            && self.overlap == other.overlap
            && self.delta_x == other.delta_x
            && self.delta_y == other.delta_y
            && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for FrameMatch {}

pub struct RawStitcher {
    stitched: Option<StitchedFrame>,
    viewport_rect: Option<ViewportRect>,
    active_crop: Option<ComposeCrop>,
    previous_compose_frame: Option<RgbFrame>,
    previous_perceptual_frame: Option<PerceptualFrame>,
    previous_perceptual_delta: Option<Option<i32>>,
    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
    previous_fixed_bands_trace: Option<(FixedBands, FixedBands, bool)>,
    fixed_detector: FixedBandDetector,
    duplicate_threshold: f64,
    last_duplicate_difference: Option<f64>,
    last_duplicate_analysis_difference: Option<f64>,
    last_duplicate_analysis_motion: Option<DuplicateAnalysisMotion>,
    last_fast_motion_trace: Option<FastMotionTrace>,
    last_profile_breakdown: StitchProfileBreakdown,
    accepted_direction: AcceptedDirectionMemory,
}

impl RawStitcher {
    pub fn new() -> Self {
        Self {
            stitched: None,
            viewport_rect: None,
            active_crop: None,
            previous_compose_frame: None,
            previous_perceptual_frame: None,
            previous_perceptual_delta: None,
            previous_fixed_bands_trace: None,
            fixed_detector: FixedBandDetector::default(),
            duplicate_threshold: 2.0,
            last_duplicate_difference: None,
            last_duplicate_analysis_difference: None,
            last_duplicate_analysis_motion: None,
            last_fast_motion_trace: None,
            last_profile_breakdown: StitchProfileBreakdown::default(),
            accepted_direction: AcceptedDirectionMemory::default(),
        }
    }

    pub fn push_frame(
        &mut self,
        frame: RgbFrame,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_with_analysis(frame.clone(), frame, direction)
    }

    pub fn push_frame_with_analysis(
        &mut self,
        compose_frame: RgbFrame,
        analysis_frame: RgbFrame,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        validate_rgb_frame(&compose_frame)?;
        validate_rgb_frame(&analysis_frame)?;
        self.push_frame_sources(&compose_frame, &analysis_frame, direction)
    }

    pub fn push_frame_views(
        &mut self,
        compose_frame: ImageRgbView<'_>,
        analysis_frame: ImageRgbView<'_>,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_sources(&compose_frame, &analysis_frame, direction)
    }

    fn push_frame_sources<C, A>(
        &mut self,
        compose_frame: &C,
        analysis_frame: &A,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>>
    where
        C: RgbSource,
        A: RgbSource,
    {
        self.last_duplicate_difference = None;
        self.last_duplicate_analysis_difference = None;
        self.last_duplicate_analysis_motion = None;
        self.last_fast_motion_trace = None;
        self.last_profile_breakdown = StitchProfileBreakdown::default();
        let profile_enabled = cfg!(test) || crate::trace::trace_profile_enabled();
        validate_rgb_source(compose_frame)?;
        validate_rgb_source(analysis_frame)?;
        if self.stitched.is_none() {
            let fixed_bands = FixedBands::default();
            let active_crop = fixed_bands
                .active_crop(compose_frame.width(), compose_frame.height())
                .ok_or("invalid active crop")?;
            let compose_frame = rgb_frame_from_source(compose_frame)?;
            self.active_crop = Some(active_crop);
            self.stitched = Some(StitchedFrame::from_first_frame_with_layout(
                &compose_frame,
                active_crop,
                fixed_bands,
            ));
            self.viewport_rect = Some(ViewportRect {
                x: 0,
                y: 0,
                width: active_crop.width,
                height: active_crop.height,
            });
            self.previous_compose_frame = Some(compose_frame);
            self.previous_perceptual_frame = Some(PerceptualFrame::from_source(analysis_frame));
            self.last_profile_breakdown.path = Some(StitchDecisionPath::Initialized);
            return Ok(PushResult::Initialized);
        }

        let active_crop = self.active_crop.ok_or("missing active crop")?;
        let active_compose = CroppedRgbSource::new(compose_frame, active_crop)?;

        let previous_rect = self.viewport_rect.ok_or("missing viewport rect")?;
        let previous = extract_stitched_region(
            self.stitched.as_ref().ok_or("missing stitched frame")?,
            previous_rect,
        )?;
        let duplicate_started_at = profile_enabled.then(Instant::now);
        let duplicate_difference = average_difference_same_source(&previous, &active_compose);
        if let Some(started_at) = duplicate_started_at {
            self.last_profile_breakdown.duplicate_check += started_at.elapsed();
        }
        if duplicate_difference <= self.duplicate_threshold {
            self.last_duplicate_difference = Some(duplicate_difference);
            if crate::trace::trace_deep_profile_enabled() {
                if let Some(previous) = self.previous_perceptual_frame.as_ref() {
                    self.last_duplicate_analysis_difference = Some(
                        average_luminance_difference_source(previous, analysis_frame),
                    );
                    self.last_duplicate_analysis_motion =
                        duplicate_analysis_motion(previous, analysis_frame);
                }
            }
            self.last_profile_breakdown.path = Some(StitchDecisionPath::Duplicate);
            return Ok(PushResult::Duplicate);
        }

        let search_direction = direction.map(|direction| match direction {
            LongDirection::Down => SearchDirection::Down,
            LongDirection::Up => SearchDirection::Up,
            LongDirection::Right => SearchDirection::Right,
            LongDirection::Left => SearchDirection::Left,
        });

        let compose_frame = rgb_frame_from_source(compose_frame)?;
        let active_compose_frame = crop_rgb_frame(&compose_frame, active_crop)?;
        let mut placement_pipeline = DefaultPlacementPipeline;
        let placement = placement_pipeline.place(PlacementInput {
            stitched: self.stitched.as_ref().ok_or("missing stitched frame")?,
            previous_rect,
            previous_region: &previous,
            active_compose: &active_compose,
            analysis_frame,
            previous_perceptual_frame: self.previous_perceptual_frame.as_ref(),
            search_direction,
            profile_enabled,
        });

        self.last_profile_breakdown.canvas_match += placement.profile.canvas_match;
        self.last_profile_breakdown.perceptual_prepare += placement.profile.perceptual_prepare;
        self.last_profile_breakdown.perceptual_match += placement.profile.perceptual_match;
        self.last_profile_breakdown.fallback_match += placement.profile.fallback_match;
        if let Some(estimate) = placement.perceptual_estimate {
            self.trace_perceptual_motion_if_changed(Some(estimate));
        } else if placement.current_perceptual_frame.is_some() {
            self.trace_perceptual_motion_if_changed(None);
        }

        if let Some(previous_compose_frame) = self.previous_compose_frame.as_ref() {
            if placement
                .perceptual_estimate
                .is_some_and(|estimate| estimate.delta_y != 0)
            {
                let fixed_bands_started_at = profile_enabled.then(Instant::now);
                let observation = self.fixed_detector.observe(
                    previous_compose_frame,
                    &compose_frame,
                    placement.perceptual_estimate,
                );
                if let Some(started_at) = fixed_bands_started_at {
                    self.last_profile_breakdown.fixed_bands += started_at.elapsed();
                }
                self.trace_fixed_bands_if_changed(observation);
                #[cfg(not(feature = "trace-logs"))]
                let _ = observation;
            }
        }

        for frame_placement in &placement.candidates {
            let match_info = frame_placement.match_info;
            if let Some(recovery) =
                self.find_dirty_overlap_recovery(previous_rect, &active_compose_frame, match_info)
            {
                let append_started_at = profile_enabled.then(Instant::now);
                let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
                let new_rect = append_frame_at_position_overwriting_overlap_rows(
                    stitched,
                    previous_rect,
                    &active_compose_frame,
                    recovery.match_info,
                    recovery.overwrite_frame_y,
                    recovery.overwrite_rows,
                )?;
                if let Some(started_at) = append_started_at {
                    self.last_profile_breakdown.append_frame += started_at.elapsed();
                }
                self.viewport_rect = Some(new_rect);
                if let Some(current_perceptual_frame) = placement.current_perceptual_frame {
                    self.previous_perceptual_frame = Some(current_perceptual_frame);
                }
                self.previous_compose_frame = Some(compose_frame);
                self.last_profile_breakdown.path =
                    placement_path_for_source(frame_placement.source);
                self.last_fast_motion_trace = frame_placement.fast_motion_trace;
                self.accepted_direction
                    .record(recovery.match_info.direction);
                self.trace_placement_accept(*frame_placement);
                return Ok(PushResult::Accepted {
                    match_info: recovery.match_info,
                });
            }
            if self.try_apply_match(
                previous_rect,
                &active_compose_frame,
                match_info,
                profile_enabled,
            )? {
                if let Some(current_perceptual_frame) = placement.current_perceptual_frame {
                    self.previous_perceptual_frame = Some(current_perceptual_frame);
                }
                self.previous_compose_frame = Some(compose_frame);
                self.last_profile_breakdown.path =
                    placement_path_for_source(frame_placement.source);
                self.last_fast_motion_trace = frame_placement.fast_motion_trace;
                self.accepted_direction.record(match_info.direction);
                self.trace_placement_accept(*frame_placement);
                return Ok(PushResult::Accepted { match_info });
            }
            if crate::trace::trace_verbose_enabled() {
                crate::trace_log!(
                    "stitch: {} rejected reason=apply-failed",
                    placement_source_label(frame_placement.source)
                );
            }
        }

        if let Some(recovery) = self.find_nomatch_recovery(previous_rect, &active_compose_frame) {
            let append_started_at = profile_enabled.then(Instant::now);
            let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
            let new_rect = append_frame_at_position_overwriting_overlap_rows(
                stitched,
                previous_rect,
                &active_compose_frame,
                recovery.match_info,
                recovery.overwrite_frame_y,
                recovery.overwrite_rows,
            )?;
            if let Some(started_at) = append_started_at {
                self.last_profile_breakdown.append_frame += started_at.elapsed();
            }
            self.viewport_rect = Some(new_rect);
            if let Some(current_perceptual_frame) = placement.current_perceptual_frame {
                self.previous_perceptual_frame = Some(current_perceptual_frame);
            }
            self.previous_compose_frame = Some(compose_frame);
            self.accepted_direction
                .record(recovery.match_info.direction);
            self.last_profile_breakdown.path = Some(StitchDecisionPath::CanvasAccepted);
            return Ok(PushResult::Accepted {
                match_info: recovery.match_info,
            });
        }

        if let Some(current_perceptual_frame) = placement.current_perceptual_frame {
            self.previous_perceptual_frame = Some(current_perceptual_frame);
        }
        self.previous_compose_frame = Some(compose_frame);
        self.last_profile_breakdown.path = Some(StitchDecisionPath::NoMatch);
        Ok(PushResult::NoMatch)
    }

    fn find_nomatch_recovery(
        &self,
        previous_rect: ViewportRect,
        frame: &RgbFrame,
    ) -> Option<NoMatchRecovery> {
        let direction = self.accepted_direction.vertical_direction()?;
        let stitched = self.stitched.as_ref()?;
        if previous_rect.x != 0
            || frame.width != stitched.width
            || frame.width != previous_rect.width
        {
            return None;
        }
        match direction {
            AppendDirection::Bottom => recover_stale_bottom_edge(stitched, previous_rect, frame),
            AppendDirection::Top => recover_stale_top_edge(stitched, previous_rect, frame),
            AppendDirection::Right | AppendDirection::Left => None,
        }
    }

    fn find_dirty_overlap_recovery(
        &self,
        previous_rect: ViewportRect,
        frame: &RgbFrame,
        match_info: FrameMatch,
    ) -> Option<NoMatchRecovery> {
        let stitched = self.stitched.as_ref()?;
        if previous_rect.x != 0
            || frame.width != stitched.width
            || frame.width != previous_rect.width
        {
            return None;
        }
        match match_info.direction {
            AppendDirection::Bottom => {
                if let Some(previous_compose_frame) = self.previous_compose_frame.as_ref() {
                    if let Some(recovery) = direct_bottom_raw_overlap_recovery(
                        previous_compose_frame,
                        frame,
                        match_info,
                    ) {
                        return Some(recovery);
                    }
                }
                let frame_y = previous_rect.y.checked_add(match_info.delta_y)?;
                let recovery = bottom_recovery_candidate(stitched, previous_rect, frame, frame_y)?;
                (recovery.overlap == match_info.overlap).then_some(NoMatchRecovery {
                    match_info: FrameMatch {
                        direction: AppendDirection::Bottom,
                        overlap: recovery.overlap,
                        delta_x: 0,
                        delta_y: frame_y.checked_sub(previous_rect.y)?,
                        score: recovery.score + f64::from(recovery.overwrite_rows),
                    },
                    overwrite_frame_y: recovery.stable_rows,
                    overwrite_rows: recovery.overwrite_rows,
                })
            }
            AppendDirection::Top => direct_top_recovery(stitched, previous_rect, frame, match_info),
            AppendDirection::Right | AppendDirection::Left => None,
        }
    }

    fn trace_placement_accept(&self, placement: FramePlacement) {
        #[cfg(not(feature = "trace-logs"))]
        let _ = placement;
        #[cfg(feature = "trace-logs")]
        {
            if !crate::trace::trace_verbose_enabled() {
                return;
            }
            crate::trace_log!(
                "stitch: {} accepted direction={:?} overlap={} delta={}x{} score={:.2}",
                placement_source_label(placement.source),
                placement.match_info.direction,
                placement.match_info.overlap,
                placement.match_info.delta_x,
                placement.match_info.delta_y,
                placement.match_info.score
            );
        }
    }

    fn try_apply_match(
        &mut self,
        previous_rect: ViewportRect,
        frame: &RgbFrame,
        match_info: FrameMatch,
        profile_enabled: bool,
    ) -> Result<bool, Box<dyn Error>> {
        let apply_started_at = profile_enabled.then(Instant::now);
        let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
        let candidate_rect = match translate_viewport_rect(previous_rect, match_info)? {
            Some(rect) => rect,
            None => {
                if let Some(started_at) = apply_started_at {
                    self.last_profile_breakdown.apply_match += started_at.elapsed();
                }
                return Ok(false);
            }
        };
        if viewport_rect_within_stitched(stitched, candidate_rect)? {
            self.viewport_rect = Some(candidate_rect);
            stitched.current_origin_x = candidate_rect.x;
            stitched.current_origin_y = candidate_rect.y;
            if let Some(started_at) = apply_started_at {
                self.last_profile_breakdown.apply_match += started_at.elapsed();
            }
            return Ok(true);
        }
        let append_started_at = profile_enabled.then(Instant::now);
        let new_rect = append_frame_at_position(stitched, previous_rect, frame, match_info)?;
        if let Some(started_at) = append_started_at {
            self.last_profile_breakdown.append_frame += started_at.elapsed();
        }
        self.viewport_rect = Some(new_rect);
        if let Some(started_at) = apply_started_at {
            self.last_profile_breakdown.apply_match += started_at.elapsed();
        }
        Ok(true)
    }

    fn trace_perceptual_motion_if_changed(&mut self, estimate: Option<PerceptualMotionEstimate>) {
        #[cfg(feature = "trace-logs")]
        {
            let current_delta = estimate.map(|e| e.delta_y);
            let should_log = self
                .previous_perceptual_delta
                .map_or(true, |prev| prev != current_delta)
                && crate::trace::trace_verbose_enabled();
            self.previous_perceptual_delta = Some(current_delta);
            if should_log {
                match estimate {
                    Some(e) => {
                        crate::trace_log!(
                            "perceptual-motion: delta_y={} median={:.2} p75={:.2} p90={:.2} mean={:.2} second={:?}/{:.2?} non_adjacent={:?}/{:.2?} zero={:.2?} separation={:.2?} overlap={} bands={}",
                            e.delta_y,
                            e.median,
                            e.p75,
                            e.p90,
                            e.mean,
                            e.second_best_delta_y,
                            e.second_best_median,
                            e.non_adjacent_second_best_delta_y,
                            e.non_adjacent_second_best_median,
                            e.no_motion_median,
                            e.separation,
                            e.overlap_rows,
                            e.band_count
                        );
                    }
                    None => {
                        crate::trace_log!("perceptual-motion: no estimate");
                    }
                }
            }
        }
        #[cfg(not(feature = "trace-logs"))]
        {
            self.previous_perceptual_delta = Some(estimate.map(|e| e.delta_y));
        }
    }

    fn trace_fixed_bands_if_changed(&mut self, observation: FixedBandObservation) {
        #[cfg(feature = "trace-logs")]
        {
            let current = (
                observation.bands,
                self.fixed_detector.stable,
                self.fixed_detector.frozen,
            );
            let should_log = crate::trace::trace_verbose_enabled()
                || self
                    .previous_fixed_bands_trace
                    .map_or(true, |previous| previous != current);
            self.previous_fixed_bands_trace = Some(current);
            if should_log {
                crate::trace_log!(
                    "fixed-bands: observed top={} bottom={} pending={} stable={} frozen={}",
                    observation.bands.top,
                    observation.bands.bottom,
                    self.fixed_detector.pending_summary(),
                    self.fixed_detector.stable_summary(),
                    self.fixed_detector.frozen
                );
            }
        }
        #[cfg(not(feature = "trace-logs"))]
        {
            let _ = observation;
        }
    }

    pub fn finish(self) -> Option<StitchedFrame> {
        self.stitched
    }

    pub fn stitched(&self) -> Option<&StitchedFrame> {
        self.stitched.as_ref()
    }

    pub fn last_duplicate_difference(&self) -> Option<f64> {
        self.last_duplicate_difference
    }

    pub fn last_duplicate_analysis_difference(&self) -> Option<f64> {
        self.last_duplicate_analysis_difference
    }

    pub fn last_duplicate_analysis_motion(&self) -> Option<DuplicateAnalysisMotion> {
        self.last_duplicate_analysis_motion
    }

    pub fn last_fast_motion_trace(&self) -> Option<FastMotionTrace> {
        self.last_fast_motion_trace
    }

    pub fn last_profile_breakdown(&self) -> StitchProfileBreakdown {
        self.last_profile_breakdown
    }
}

impl FixedBands {
    pub fn is_empty(&self) -> bool {
        self.top == 0 && self.bottom == 0
    }

    pub fn active_crop(&self, compose_width: u32, compose_height: u32) -> Option<ComposeCrop> {
        let active_height = compose_height.checked_sub(self.top.checked_add(self.bottom)?)?;
        if compose_width == 0 || active_height == 0 {
            return None;
        }
        Some(ComposeCrop {
            x: 0,
            y: self.top,
            width: compose_width,
            height: active_height,
        })
    }
}

fn duplicate_analysis_motion(
    previous: &PerceptualFrame,
    current: &impl RgbSource,
) -> Option<DuplicateAnalysisMotion> {
    let width = previous.width.min(current.width());
    let height = previous.height.min(current.height());
    if width == 0 || height == 0 {
        return None;
    }

    let mut changed_top = None;
    let mut changed_bottom = 0;
    let mut strongest_y = 0;
    let mut strongest_diff = 0.0;
    for y in (0..height).step_by(4) {
        let mut total = 0.0;
        let mut samples = 0u64;
        for x in (0..width).step_by(4) {
            let [r, g, b] = current.pixel_rgb(x, y);
            let current_luminance =
                ((77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8) as f64;
            total += (f64::from(previous.luminance(x, y)) - current_luminance).abs();
            samples += 1;
        }
        if samples == 0 {
            continue;
        }
        let diff = total / samples as f64;
        if diff > strongest_diff {
            strongest_diff = diff;
            strongest_y = y;
        }
        if diff >= DUPLICATE_ANALYSIS_ROW_DIFF_THRESHOLD {
            changed_top.get_or_insert(y);
            changed_bottom = (y + 4).min(height);
        }
    }

    changed_top.map(|changed_top| DuplicateAnalysisMotion {
        changed_top,
        changed_bottom,
        strongest_y,
        strongest_diff,
    })
}

pub fn find_frame_shift_match(
    previous: &RgbFrame,
    current: &RgbFrame,
    search_direction: Option<SearchDirection>,
) -> FrameMatch {
    find_frame_shift_match_source(previous, current, search_direction)
}

fn find_frame_shift_match_source(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    search_direction: Option<SearchDirection>,
) -> FrameMatch {
    let max_vertical_shift = previous.height().min(current.height()).saturating_sub(1);
    let max_horizontal_shift = previous.width().min(current.width()).saturating_sub(1);
    let allowed_directions: &[SearchDirection] = match search_direction {
        None => &[SearchDirection::Down, SearchDirection::Up],
        Some(SearchDirection::Down) => &[SearchDirection::Down],
        Some(SearchDirection::Up) => &[SearchDirection::Up],
        Some(SearchDirection::Right) => &[SearchDirection::Right],
        Some(SearchDirection::Left) => &[SearchDirection::Left],
    };
    let mut best: Option<FrameMatch> = None;
    for direction in allowed_directions {
        let max_shift = match direction {
            SearchDirection::Down | SearchDirection::Up => max_vertical_shift,
            SearchDirection::Right | SearchDirection::Left => max_horizontal_shift,
        };
        for shift in 1..=max_shift {
            let candidate = match direction {
                SearchDirection::Down => try_vertical_shift(previous, current, shift, true),
                SearchDirection::Up => try_vertical_shift(previous, current, shift, false),
                SearchDirection::Right => try_horizontal_shift(previous, current, shift, true),
                SearchDirection::Left => try_horizontal_shift(previous, current, shift, false),
            };
            if let Some(candidate) = candidate {
                if best.is_none_or(|best| candidate.score < best.score) {
                    best = Some(candidate);
                }
            }
        }
    }
    best.unwrap_or_else(no_match)
}

#[cfg(test)]
fn fast_motion_candidate_frame_match(
    candidate: FastMotionCandidate,
    frame: &impl RgbSource,
    search_direction: Option<SearchDirection>,
) -> Result<FrameMatch, &'static str> {
    if candidate.delta_y == 0 {
        return Err("zero-delta");
    }
    let shift = candidate.delta_y.unsigned_abs();
    if shift >= frame.height() {
        return Err("delta-out-of-range");
    }
    let overlap = frame.height() - shift;
    let max_overlap = frame.height().saturating_sub(1);
    if overlap < minimum_overlap(frame.height(), max_overlap) {
        return Err("insufficient-overlap");
    }
    if candidate.overlap_rows < minimum_overlap(frame.height(), max_overlap) {
        return Err("insufficient-estimate-overlap");
    }
    if candidate.score > FAST_MOTION_MAX_SCORE {
        return Err("weak-score");
    }

    let direction = if candidate.delta_y < 0 {
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
        delta_y: -candidate.delta_y,
        score: candidate.score,
    })
}

#[cfg(test)]
fn scan_fast_vertical_motion_candidate(
    previous: &PerceptualFrame,
    current: &PerceptualFrame,
    search_direction: Option<SearchDirection>,
) -> Option<FastMotionCandidate> {
    scan_fast_vertical_motion(previous, current, search_direction)?.candidate
}

fn compare_fast_motion_candidate(
    candidate: Option<FastMotionCandidate>,
    match_info: Option<FrameMatch>,
    fallback: bool,
    verify_pass: Option<FastMotionVerifyPass>,
) -> FastMotionTrace {
    let reference_delta_y = match_info.and_then(fast_motion_reference_delta_y);
    let agreement = match (candidate, reference_delta_y, fallback) {
        (None, Some(_), false) => FastMotionAgreement::MissedHeavyAccept,
        (None, Some(_), true) => FastMotionAgreement::MissedFallbackAccept,
        (None, None, _) => FastMotionAgreement::NoCandidate,
        (Some(_), None, _) => FastMotionAgreement::CandidateOnly,
        (Some(candidate), Some(reference), false) => {
            compare_fast_motion_delta(candidate.delta_y, reference, false)
        }
        (Some(candidate), Some(reference), true) => {
            compare_fast_motion_delta(candidate.delta_y, reference, true)
        }
    };
    FastMotionTrace {
        candidate,
        reference_delta_y,
        agreement,
        verify_pass,
    }
}

fn compare_fast_motion_delta(
    candidate_delta_y: i32,
    reference_delta_y: i32,
    fallback: bool,
) -> FastMotionAgreement {
    if candidate_delta_y == reference_delta_y {
        return if fallback {
            FastMotionAgreement::FallbackExactDelta
        } else {
            FastMotionAgreement::HeavyExactDelta
        };
    }
    if candidate_delta_y.signum() == reference_delta_y.signum() {
        return if fallback {
            FastMotionAgreement::FallbackSameDirection
        } else if (candidate_delta_y - reference_delta_y).abs() <= PERCEPTUAL_MOTION_ADJACENT_DELTA
        {
            FastMotionAgreement::HeavySameDirection
        } else {
            FastMotionAgreement::HeavyDifferentDelta
        };
    }
    if fallback {
        FastMotionAgreement::FallbackOppositeDirection
    } else {
        FastMotionAgreement::HeavyOppositeDirection
    }
}

fn fast_motion_reference_delta_y(match_info: FrameMatch) -> Option<i32> {
    match match_info.direction {
        AppendDirection::Bottom | AppendDirection::Top => Some(-match_info.delta_y),
        AppendDirection::Right | AppendDirection::Left => None,
    }
}

fn no_match() -> FrameMatch {
    FrameMatch {
        direction: AppendDirection::Bottom,
        overlap: 0,
        delta_x: 0,
        delta_y: 0,
        score: f64::MAX,
    }
}

fn recover_stale_bottom_edge(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
) -> Option<NoMatchRecovery> {
    // Dirty-canvas recovery is a last-resort path after normal placement fails.
    // Search locally first to avoid global false positives, then fall back to a
    // full prefix search for cases where stale stitched content displaced the
    // previous viewport relation.
    let local_radius = i32::try_from(frame.height).ok()?.saturating_mul(2);
    let local_start = previous_rect.y.saturating_sub(local_radius).max(0);
    let local_end = previous_rect
        .y
        .saturating_add(local_radius)
        .min(i32::try_from(stitched.height).ok()?.saturating_sub(1));
    find_bottom_recovery_in_range(stitched, previous_rect, frame, local_start, local_end).or_else(
        || {
            find_bottom_recovery_in_range(
                stitched,
                previous_rect,
                frame,
                0,
                i32::try_from(stitched.height).ok()?.saturating_sub(1),
            )
        },
    )
}

fn find_bottom_recovery_in_range(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    start_y: i32,
    end_y: i32,
) -> Option<NoMatchRecovery> {
    let mut best: Option<BottomRecoveryCandidate> = None;
    for frame_y in start_y..=end_y {
        let Some(candidate) = bottom_recovery_candidate(stitched, previous_rect, frame, frame_y)
        else {
            continue;
        };
        if best.as_ref().is_none_or(|best| {
            candidate.stable_rows > best.stable_rows
                || (candidate.stable_rows == best.stable_rows && candidate.score < best.score)
        }) {
            best = Some(candidate);
        }
    }
    let candidate = best?;
    Some(NoMatchRecovery {
        match_info: FrameMatch {
            direction: AppendDirection::Bottom,
            overlap: candidate.overlap,
            delta_x: 0,
            delta_y: candidate.frame_y.checked_sub(previous_rect.y)?,
            score: candidate.score + f64::from(candidate.overwrite_rows),
        },
        overwrite_frame_y: candidate.stable_rows,
        overwrite_rows: candidate.overwrite_rows,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BottomRecoveryCandidate {
    frame_y: i32,
    overlap: u32,
    stable_rows: u32,
    overwrite_rows: u32,
    score: f64,
}

fn bottom_recovery_candidate(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    frame_y: i32,
) -> Option<BottomRecoveryCandidate> {
    if frame_y < 0
        || frame_y
            < previous_rect
                .y
                .saturating_sub(i32::try_from(frame.height).ok()?)
    {
        return None;
    }
    let frame_y_u32 = u32::try_from(frame_y).ok()?;
    if frame_y_u32 >= stitched.height {
        return None;
    }
    let overlap = frame.height.min(stitched.height - frame_y_u32);
    let stable_rows = matching_top_prefix_rows(stitched, frame, frame_y_u32, overlap);
    let overwrite_rows =
        matching_top_dirty_rows(stitched, frame, frame_y_u32, stable_rows, overlap);
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, stable_rows + overwrite_rows) {
        return None;
    }
    let dirty_score =
        average_top_dirty_score(stitched, frame, frame_y_u32, stable_rows, overwrite_rows)?;
    if dirty_score < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF {
        return None;
    }
    let score = average_top_row_score(stitched, frame, frame_y_u32, stable_rows)?;
    Some(BottomRecoveryCandidate {
        frame_y,
        overlap,
        stable_rows,
        overwrite_rows,
        score,
    })
}

fn recover_stale_top_edge(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
) -> Option<NoMatchRecovery> {
    let min_frame_y = -(frame.height as i32) + 1;
    let max_frame_y = previous_rect.y.saturating_sub(1).min(-1);
    let mut best: Option<(i32, u32, u32, f64)> = None;
    for frame_y in min_frame_y..=max_frame_y {
        let existing_overlap = u32::try_from(frame.height as i32 + frame_y).ok()?;
        if existing_overlap == 0 || existing_overlap > stitched.height {
            continue;
        }
        let stable_rows = matching_bottom_suffix_rows(stitched, frame, existing_overlap);
        if !dirty_canvas_recovery_shape_for_rows(stable_rows, existing_overlap)
            || bottom_dirty_average_difference(stitched, frame, existing_overlap, stable_rows)
                < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF
        {
            continue;
        }
        let score = average_bottom_row_score(stitched, frame, existing_overlap, stable_rows)?;
        if best.is_none_or(|(_, best_stable, _, best_score)| {
            stable_rows > best_stable || (stable_rows == best_stable && score < best_score)
        }) {
            best = Some((frame_y, stable_rows, existing_overlap, score));
        }
    }
    let (frame_y, stable_rows, existing_overlap, score) = best?;
    let overwrite_rows = existing_overlap - stable_rows;
    Some(NoMatchRecovery {
        match_info: FrameMatch {
            direction: AppendDirection::Top,
            overlap: existing_overlap,
            delta_x: 0,
            delta_y: frame_y.checked_sub(previous_rect.y)?,
            score: score + f64::from(existing_overlap - stable_rows),
        },
        overwrite_frame_y: frame.height - existing_overlap,
        overwrite_rows,
    })
}

fn direct_top_recovery(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &RgbFrame,
    match_info: FrameMatch,
) -> Option<NoMatchRecovery> {
    let frame_y = previous_rect.y.checked_add(match_info.delta_y)?;
    let existing_overlap = u32::try_from(frame.height as i32 + frame_y).ok()?;
    if frame_y >= 0 || existing_overlap == 0 || existing_overlap != match_info.overlap {
        return None;
    }
    let stable_rows = matching_bottom_suffix_rows(stitched, frame, existing_overlap);
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, existing_overlap)
        || bottom_dirty_average_difference(stitched, frame, existing_overlap, stable_rows)
            < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF
    {
        return None;
    }
    let score = average_bottom_row_score(stitched, frame, existing_overlap, stable_rows)?;
    Some(NoMatchRecovery {
        match_info: FrameMatch {
            direction: AppendDirection::Top,
            overlap: existing_overlap,
            delta_x: 0,
            delta_y: frame_y.checked_sub(previous_rect.y)?,
            score: score + f64::from(existing_overlap - stable_rows),
        },
        overwrite_frame_y: frame.height - existing_overlap,
        overwrite_rows: existing_overlap - stable_rows,
    })
}

fn direct_bottom_raw_overlap_recovery(
    previous_compose_frame: &RgbFrame,
    frame: &RgbFrame,
    match_info: FrameMatch,
) -> Option<NoMatchRecovery> {
    if match_info.delta_y < 0 || match_info.delta_x != 0 {
        return None;
    }
    let frame_y = u32::try_from(match_info.delta_y).ok()?;
    if frame_y >= frame.height {
        return None;
    }
    let overlap = frame.height.saturating_sub(frame_y);
    if overlap != match_info.overlap {
        return None;
    }
    let stable_rows = matching_raw_top_prefix_rows(previous_compose_frame, frame, frame_y, overlap);
    let overwrite_rows =
        matching_raw_top_dirty_rows(previous_compose_frame, frame, frame_y, stable_rows, overlap);
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, stable_rows + overwrite_rows) {
        return None;
    }
    let dirty_score = average_raw_top_dirty_score(
        previous_compose_frame,
        frame,
        frame_y,
        stable_rows,
        overwrite_rows,
    )?;
    if dirty_score < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF {
        return None;
    }
    let score = average_raw_top_row_score(previous_compose_frame, frame, frame_y, stable_rows)?;
    Some(NoMatchRecovery {
        match_info: FrameMatch {
            direction: AppendDirection::Bottom,
            overlap,
            delta_x: 0,
            delta_y: match_info.delta_y,
            score: score + f64::from(overwrite_rows),
        },
        overwrite_frame_y: stable_rows,
        overwrite_rows,
    })
}

fn matching_raw_top_prefix_rows(
    previous: &RgbFrame,
    frame: &RgbFrame,
    previous_y: u32,
    max_rows: u32,
) -> u32 {
    if max_rows == 0 {
        return 0;
    }
    let mut pass = 0;
    let mut probe = 1;
    while probe <= max_rows && raw_top_prefix_rows_are_stable(previous, frame, previous_y, probe) {
        pass = probe;
        probe = probe.saturating_mul(2);
        if probe == 0 {
            break;
        }
    }
    let mut low = pass;
    let mut high = probe.min(max_rows.saturating_add(1));
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if raw_top_prefix_rows_are_stable(previous, frame, previous_y, mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn matching_raw_top_dirty_rows(
    previous: &RgbFrame,
    frame: &RgbFrame,
    previous_y: u32,
    stable_rows: u32,
    overlap: u32,
) -> u32 {
    let mut rows = 0;
    for row in stable_rows..overlap {
        if raw_row_is_stable(previous, previous_y + row, frame, row) {
            break;
        }
        rows += 1;
    }
    rows
}

fn raw_top_prefix_rows_are_stable(
    previous: &RgbFrame,
    frame: &RgbFrame,
    previous_y: u32,
    rows: u32,
) -> bool {
    if rows == 0 {
        return false;
    }
    (0..rows).all(|row| raw_row_is_stable(previous, previous_y + row, frame, row))
}

fn raw_row_is_stable(previous: &RgbFrame, previous_y: u32, frame: &RgbFrame, frame_y: u32) -> bool {
    rgb_frame_row_average_difference(previous, previous_y, frame, frame_y)
        <= MATCH_LINE_MAX_AVERAGE_DIFF
        && rgb_frame_row_max_difference(previous, previous_y, frame, frame_y)
            <= NOMATCH_RECOVERY_RAW_STABLE_MAX_PIXEL_DIFF
}

fn average_raw_top_dirty_score(
    previous: &RgbFrame,
    frame: &RgbFrame,
    previous_y: u32,
    stable_rows: u32,
    dirty_rows: u32,
) -> Option<f64> {
    let mut total = 0.0;
    for row in stable_rows..stable_rows + dirty_rows {
        total += rgb_frame_row_average_difference(previous, previous_y + row, frame, row);
    }
    (dirty_rows > 0).then_some(total / f64::from(dirty_rows))
}

fn average_raw_top_row_score(
    previous: &RgbFrame,
    frame: &RgbFrame,
    previous_y: u32,
    rows: u32,
) -> Option<f64> {
    let mut total = 0.0;
    for row in 0..rows {
        total += rgb_frame_row_average_difference(previous, previous_y + row, frame, row);
    }
    (rows > 0).then_some(total / f64::from(rows))
}

fn rgb_frame_row_average_difference(
    previous: &RgbFrame,
    previous_y: u32,
    frame: &RgbFrame,
    frame_y: u32,
) -> f64 {
    let width = previous.width.min(frame.width);
    if width == 0 || previous_y >= previous.height || frame_y >= frame.height {
        return f64::MAX;
    }
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in (0..width).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        let previous_offset = pixel_offset(previous, x, previous_y);
        let frame_offset = pixel_offset(frame, x, frame_y);
        total += (i32::from(previous.data[previous_offset]) - i32::from(frame.data[frame_offset]))
            .unsigned_abs() as u64;
        total += (i32::from(previous.data[previous_offset + 1])
            - i32::from(frame.data[frame_offset + 1]))
        .unsigned_abs() as u64;
        total += (i32::from(previous.data[previous_offset + 2])
            - i32::from(frame.data[frame_offset + 2]))
        .unsigned_abs() as u64;
        samples += 1;
    }
    line_average_difference(total, samples)
}

fn rgb_frame_row_max_difference(
    previous: &RgbFrame,
    previous_y: u32,
    frame: &RgbFrame,
    frame_y: u32,
) -> u32 {
    let width = previous.width.min(frame.width);
    if width == 0 || previous_y >= previous.height || frame_y >= frame.height {
        return u32::MAX;
    }
    let mut max_diff = 0;
    for x in (0..width).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        let previous_offset = pixel_offset(previous, x, previous_y);
        let frame_offset = pixel_offset(frame, x, frame_y);
        let diff = (i32::from(previous.data[previous_offset])
            - i32::from(frame.data[frame_offset]))
        .unsigned_abs()
            + (i32::from(previous.data[previous_offset + 1])
                - i32::from(frame.data[frame_offset + 1]))
            .unsigned_abs()
            + (i32::from(previous.data[previous_offset + 2])
                - i32::from(frame.data[frame_offset + 2]))
            .unsigned_abs();
        max_diff = max_diff.max(diff);
    }
    max_diff
}

fn dirty_canvas_recovery_shape_for_rows(stable_rows: u32, proven_rows: u32) -> bool {
    if stable_rows < NOMATCH_RECOVERY_MIN_STABLE_ROWS || stable_rows >= proven_rows {
        return false;
    }
    let dirty_rows = proven_rows - stable_rows;
    let ratio_limit = (f64::from(proven_rows) * NOMATCH_RECOVERY_MAX_DIRTY_RATIO).ceil() as u32;
    dirty_rows >= NOMATCH_RECOVERY_MIN_DIRTY_ROWS
        && dirty_rows <= NOMATCH_RECOVERY_MAX_DIRTY_ROWS
        && dirty_rows <= NOMATCH_RECOVERY_SMALL_DIRTY_ROWS.max(ratio_limit)
}

fn matching_top_dirty_rows(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    stitched_y: u32,
    stable_rows: u32,
    overlap: u32,
) -> u32 {
    let mut rows = 0;
    for row in stable_rows..overlap {
        if rgb_row_average_difference(stitched, stitched_y + row, frame, row)
            < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF
        {
            break;
        }
        rows += 1;
    }
    rows
}

fn matching_top_prefix_rows(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    stitched_y: u32,
    max_rows: u32,
) -> u32 {
    if max_rows == 0 {
        return 0;
    }
    let mut pass = 0;
    let mut probe = 1;
    while probe <= max_rows
        && top_prefix_average_difference(stitched, frame, stitched_y, probe)
            <= MATCH_LINE_MAX_AVERAGE_DIFF
    {
        pass = probe;
        probe = probe.saturating_mul(2);
        if probe == 0 {
            break;
        }
    }
    let mut low = pass;
    let mut high = probe.min(max_rows.saturating_add(1));
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if top_prefix_average_difference(stitched, frame, stitched_y, mid)
            <= MATCH_LINE_MAX_AVERAGE_DIFF
        {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn matching_bottom_suffix_rows(stitched: &StitchedFrame, frame: &RgbFrame, max_rows: u32) -> u32 {
    if max_rows == 0 {
        return 0;
    }
    let mut pass = 0;
    let mut probe = 1;
    while probe <= max_rows
        && bottom_suffix_average_difference(stitched, frame, max_rows, probe)
            <= MATCH_LINE_MAX_AVERAGE_DIFF
    {
        pass = probe;
        probe = probe.saturating_mul(2);
        if probe == 0 {
            break;
        }
    }
    let mut low = pass;
    let mut high = probe.min(max_rows.saturating_add(1));
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if bottom_suffix_average_difference(stitched, frame, max_rows, mid)
            <= MATCH_LINE_MAX_AVERAGE_DIFF
        {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn top_prefix_average_difference(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    stitched_y: u32,
    rows: u32,
) -> f64 {
    average_top_row_score(stitched, frame, stitched_y, rows).unwrap_or(f64::MAX)
}

fn bottom_suffix_average_difference(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    overlap: u32,
    rows: u32,
) -> f64 {
    average_bottom_row_score(stitched, frame, overlap, rows).unwrap_or(f64::MAX)
}

fn average_top_dirty_score(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    stitched_y: u32,
    stable_rows: u32,
    dirty_rows: u32,
) -> Option<f64> {
    let mut total = 0.0;
    for row in stable_rows..stable_rows + dirty_rows {
        total += rgb_row_average_difference(stitched, stitched_y + row, frame, row);
    }
    (dirty_rows > 0).then_some(total / f64::from(dirty_rows))
}

fn bottom_dirty_average_difference(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    overlap: u32,
    stable_rows: u32,
) -> f64 {
    let dirty_rows = overlap.saturating_sub(stable_rows);
    if dirty_rows == 0 {
        return 0.0;
    }
    let mut total = 0.0;
    for row in 0..dirty_rows {
        total += rgb_row_average_difference(stitched, row, frame, frame.height - overlap + row);
    }
    total / f64::from(dirty_rows)
}

fn average_top_row_score(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    stitched_y: u32,
    rows: u32,
) -> Option<f64> {
    let mut total = 0.0;
    for row in 0..rows {
        total += rgb_row_average_difference(stitched, stitched_y + row, frame, row);
    }
    (rows > 0).then_some(total / f64::from(rows))
}

fn average_bottom_row_score(
    stitched: &StitchedFrame,
    frame: &RgbFrame,
    overlap: u32,
    rows: u32,
) -> Option<f64> {
    let mut total = 0.0;
    for row in 0..rows {
        total +=
            rgb_row_average_difference(stitched, overlap - 1 - row, frame, frame.height - 1 - row);
    }
    (rows > 0).then_some(total / f64::from(rows))
}

fn rgb_row_average_difference(
    stitched: &StitchedFrame,
    stitched_y: u32,
    frame: &RgbFrame,
    frame_y: u32,
) -> f64 {
    let width = stitched.width.min(frame.width);
    if width == 0 || stitched_y >= stitched.height || frame_y >= frame.height {
        return f64::MAX;
    }
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in (0..width).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        let stitched_offset = stitched_y as usize * stitched.stride as usize + x as usize * 3;
        let frame_offset = pixel_offset(frame, x, frame_y);
        total += (i32::from(stitched.data[stitched_offset]) - i32::from(frame.data[frame_offset]))
            .unsigned_abs() as u64;
        total += (i32::from(stitched.data[stitched_offset + 1])
            - i32::from(frame.data[frame_offset + 1]))
        .unsigned_abs() as u64;
        total += (i32::from(stitched.data[stitched_offset + 2])
            - i32::from(frame.data[frame_offset + 2]))
        .unsigned_abs() as u64;
        samples += 1;
    }
    line_average_difference(total, samples)
}

fn try_vertical_shift(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_bottom: bool,
) -> Option<FrameMatch> {
    let overlap = previous
        .height()
        .min(current.height())
        .saturating_sub(shift);
    let max_overlap = previous.height().min(current.height()).saturating_sub(1);
    if overlap < minimum_overlap(previous.height().min(current.height()), max_overlap)
        || !vertical_column_has_overlap(previous, current, shift, append_bottom)
    {
        return None;
    }
    let score = confirm_vertical_shift(previous, current, shift, append_bottom)?;
    Some(FrameMatch {
        direction: if append_bottom {
            AppendDirection::Bottom
        } else {
            AppendDirection::Top
        },
        overlap,
        delta_x: 0,
        delta_y: if append_bottom {
            shift as i32
        } else {
            -(shift as i32)
        },
        score,
    })
}

fn try_horizontal_shift(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_right: bool,
) -> Option<FrameMatch> {
    let overlap = previous.width().min(current.width()).saturating_sub(shift);
    let max_overlap = previous.width().min(current.width()).saturating_sub(1);
    if overlap < minimum_overlap(previous.width().min(current.width()), max_overlap)
        || !horizontal_row_has_overlap(previous, current, shift, append_right)
    {
        return None;
    }
    let score = confirm_horizontal_shift(previous, current, shift, append_right)?;
    Some(FrameMatch {
        direction: if append_right {
            AppendDirection::Right
        } else {
            AppendDirection::Left
        },
        overlap,
        delta_x: if append_right {
            shift as i32
        } else {
            -(shift as i32)
        },
        delta_y: 0,
        score,
    })
}

fn minimum_overlap(viewport_dimension: u32, max_possible_overlap: u32) -> u32 {
    MATCH_MIN_OVERLAP_PIXELS
        .max(viewport_dimension.div_ceil(10))
        .min(max_possible_overlap)
}

fn vertical_column_has_overlap(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_bottom: bool,
) -> bool {
    let overlap = previous
        .height()
        .min(current.height())
        .saturating_sub(shift);
    if overlap == 0 {
        return false;
    }
    let x = previous.width().min(current.width()) / 2;
    let mut total = 0u64;
    let mut samples = 0u32;
    for y in (0..overlap).step_by(MATCH_PREFILTER_SAMPLE_STEP as usize) {
        let previous_y = if append_bottom { shift + y } else { y };
        let current_y = if append_bottom { y } else { shift + y };
        total += pixel_difference_source(previous, x, previous_y, current, x, current_y) as u64;
        samples += 1;
    }
    line_average_difference(total, samples) <= MATCH_PREFILTER_MAX_AVERAGE_DIFF
}

fn horizontal_row_has_overlap(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_right: bool,
) -> bool {
    let overlap = previous.width().min(current.width()).saturating_sub(shift);
    if overlap == 0 {
        return false;
    }
    let y = previous.height().min(current.height()) / 2;
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in (0..overlap).step_by(MATCH_PREFILTER_SAMPLE_STEP as usize) {
        let previous_x = if append_right { shift + x } else { x };
        let current_x = if append_right { x } else { shift + x };
        total += pixel_difference_source(previous, previous_x, y, current, current_x, y) as u64;
        samples += 1;
    }
    line_average_difference(total, samples) <= MATCH_PREFILTER_MAX_AVERAGE_DIFF
}

fn confirm_vertical_shift(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_bottom: bool,
) -> Option<f64> {
    let overlap = previous
        .height()
        .min(current.height())
        .saturating_sub(shift);
    let width = previous.width().min(current.width());
    if overlap == 0 || width == 0 {
        return None;
    }
    let mut total = 0u64;
    let mut samples = 0u32;
    for y in 0..overlap {
        let previous_y = if append_bottom { shift + y } else { y };
        let current_y = if append_bottom { y } else { shift + y };
        let line_score = row_average_difference(previous, previous_y, current, current_y, width);
        if line_score.score > MATCH_LINE_MAX_AVERAGE_DIFF {
            return None;
        }
        total += line_score.total;
        samples += line_score.samples;
    }
    let score = line_average_difference(total, samples);
    Some(score)
}

fn confirm_horizontal_shift(
    previous: &impl RgbSource,
    current: &impl RgbSource,
    shift: u32,
    append_right: bool,
) -> Option<f64> {
    let overlap = previous.width().min(current.width()).saturating_sub(shift);
    let height = previous.height().min(current.height());
    if overlap == 0 || height == 0 {
        return None;
    }
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in 0..overlap {
        let previous_x = if append_right { shift + x } else { x };
        let current_x = if append_right { x } else { shift + x };
        let line_score =
            column_average_difference(previous, previous_x, current, current_x, height);
        if line_score.score > MATCH_LINE_MAX_AVERAGE_DIFF {
            return None;
        }
        total += line_score.total;
        samples += line_score.samples;
    }
    let score = line_average_difference(total, samples);
    Some(score)
}

struct LineScore {
    total: u64,
    samples: u32,
    score: f64,
}

fn row_average_difference(
    a: &impl RgbSource,
    ay: u32,
    b: &impl RgbSource,
    by: u32,
    width: u32,
) -> LineScore {
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in (0..width).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        total += pixel_difference_source(a, x, ay, b, x, by) as u64;
        samples += 1;
    }
    LineScore {
        total,
        samples,
        score: line_average_difference(total, samples),
    }
}

fn column_average_difference(
    a: &impl RgbSource,
    ax: u32,
    b: &impl RgbSource,
    bx: u32,
    height: u32,
) -> LineScore {
    let mut total = 0u64;
    let mut samples = 0u32;
    for y in (0..height).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        total += pixel_difference_source(a, ax, y, b, bx, y) as u64;
        samples += 1;
    }
    LineScore {
        total,
        samples,
        score: line_average_difference(total, samples),
    }
}

fn line_average_difference(total: u64, pixels: u32) -> f64 {
    if pixels == 0 {
        f64::MAX
    } else {
        total as f64 / pixels as f64 / 3.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, seed: u8) -> RgbFrame {
        let stride = width * 3;
        let mut data = vec![0; stride as usize * height as usize];
        for y in 0..height {
            for x in 0..width {
                let offset = (y * stride + x * 3) as usize;
                data[offset] = seed.wrapping_add(x as u8);
                data[offset + 1] = seed.wrapping_add((y * 3) as u8);
                data[offset + 2] = seed.wrapping_add((x + y) as u8);
            }
        }
        RgbFrame {
            width,
            height,
            stride,
            data,
        }
    }

    fn textured_frame(width: u32, height: u32) -> RgbFrame {
        let stride = width * 3;
        let mut data = vec![0; stride as usize * height as usize];
        for y in 0..height {
            for x in 0..width {
                let offset = (y * stride + x * 3) as usize;
                data[offset] = ((x * 13 + y * 17 + (x * y) % 19) % 251) as u8;
                data[offset + 1] = ((x * 7 + y * 29 + (x + y * 3) % 23) % 251) as u8;
                data[offset + 2] = ((x * 3 + y * 11 + (x * 5 + y) % 31) % 251) as u8;
            }
        }
        RgbFrame {
            width,
            height,
            stride,
            data,
        }
    }

    fn periodic_textured_frame(width: u32, height: u32, period: u32) -> RgbFrame {
        let stride = width * 3;
        let mut data = vec![0; stride as usize * height as usize];
        for row in 0..height {
            let pattern_y = row % period;
            for x in 0..width {
                let offset = (row * stride + x * 3) as usize;
                data[offset] = ((x * 13 + pattern_y * 17 + (x * pattern_y) % 19) % 251) as u8;
                data[offset + 1] =
                    ((x * 7 + pattern_y * 29 + (x + pattern_y * 3) % 23) % 251) as u8;
                data[offset + 2] =
                    ((x * 3 + pattern_y * 11 + (x * 5 + pattern_y) % 31) % 251) as u8;
            }
        }
        RgbFrame {
            width,
            height,
            stride,
            data,
        }
    }

    fn shifted_top_with_tiny_difference(previous: &RgbFrame, shift: u32) -> RgbFrame {
        let mut current = textured_frame(previous.width, previous.height);
        for y in 0..previous.height - shift {
            for x in 0..previous.width {
                let prev_offset = pixel_offset(previous, x, y);
                let cur_offset = pixel_offset(&current, x, y + shift);
                current.data[cur_offset] = previous.data[prev_offset].saturating_add(1);
                current.data[cur_offset + 1] = previous.data[prev_offset + 1].saturating_add(1);
                current.data[cur_offset + 2] = previous.data[prev_offset + 2].saturating_add(1);
            }
        }
        current
    }

    fn shifted_bottom_with_difference(previous: &RgbFrame, shift: u32, difference: u8) -> RgbFrame {
        let mut current = textured_frame(previous.width, previous.height);
        for y in 0..previous.height - shift {
            for x in 0..previous.width {
                let prev_offset = pixel_offset(previous, x, y + shift);
                let cur_offset = pixel_offset(&current, x, y);
                current.data[cur_offset] = previous.data[prev_offset].saturating_add(difference);
                current.data[cur_offset + 1] =
                    previous.data[prev_offset + 1].saturating_add(difference);
                current.data[cur_offset + 2] =
                    previous.data[prev_offset + 2].saturating_add(difference);
            }
        }
        current
    }

    fn shifted_bottom(previous: &RgbFrame, shift: u32) -> RgbFrame {
        let mut current = frame(previous.width, previous.height, 200);
        for y in 0..previous.height - shift {
            for x in 0..previous.width {
                let prev_offset = pixel_offset(previous, x, y + shift);
                let cur_offset = pixel_offset(&current, x, y);
                current.data[cur_offset..cur_offset + 3]
                    .copy_from_slice(&previous.data[prev_offset..prev_offset + 3]);
            }
        }
        current
    }

    fn shifted_top(previous: &RgbFrame, shift: u32) -> RgbFrame {
        let mut current = frame(previous.width, previous.height, 180);
        for y in 0..previous.height - shift {
            for x in 0..previous.width {
                let prev_offset = pixel_offset(previous, x, y);
                let cur_offset = pixel_offset(&current, x, y + shift);
                current.data[cur_offset..cur_offset + 3]
                    .copy_from_slice(&previous.data[prev_offset..prev_offset + 3]);
            }
        }
        current
    }

    fn shifted_right(previous: &RgbFrame, shift: u32) -> RgbFrame {
        let mut current = frame(previous.width, previous.height, 160);
        for y in 0..previous.height {
            for x in 0..previous.width - shift {
                let prev_offset = pixel_offset(previous, x + shift, y);
                let cur_offset = pixel_offset(&current, x, y);
                current.data[cur_offset..cur_offset + 3]
                    .copy_from_slice(&previous.data[prev_offset..prev_offset + 3]);
            }
        }
        current
    }

    fn shifted_left(previous: &RgbFrame, shift: u32) -> RgbFrame {
        let mut current = frame(previous.width, previous.height, 140);
        for y in 0..previous.height {
            for x in 0..previous.width - shift {
                let prev_offset = pixel_offset(previous, x, y);
                let cur_offset = pixel_offset(&current, x + shift, y);
                current.data[cur_offset..cur_offset + 3]
                    .copy_from_slice(&previous.data[prev_offset..prev_offset + 3]);
            }
        }
        current
    }

    fn crop_frame(source: &RgbFrame, x: u32, y: u32, width: u32, height: u32) -> RgbFrame {
        crop_rgb_frame(
            source,
            ComposeCrop {
                x,
                y,
                width,
                height,
            },
        )
        .unwrap()
    }

    fn row_texture(width: u32, row: u32) -> Vec<u8> {
        let mut data = vec![0; width as usize * 3];
        for x in 0..width {
            let offset = x as usize * 3;
            data[offset] = ((x * 37 + row * 17 + (x * row) % 29) % 251) as u8;
            data[offset + 1] = ((x * 19 + row * 31 + (x + row * 3) % 23) % 251) as u8;
            data[offset + 2] = ((x * 11 + row * 7 + (x * 5 + row) % 41) % 251) as u8;
        }
        data
    }

    fn frame_from_world_rows(width: u32, rows: &[u32]) -> RgbFrame {
        let stride = width * 3;
        let mut data = Vec::with_capacity(stride as usize * rows.len());
        for row in rows {
            data.extend_from_slice(&row_texture(width, *row));
        }
        RgbFrame {
            width,
            height: rows.len() as u32,
            stride,
            data,
        }
    }

    fn paint_full_row(target: &mut RgbFrame, y: u32, seed: u8) {
        for x in 0..target.width {
            let offset = pixel_offset(target, x, y);
            target.data[offset] = seed.wrapping_add((x * 13 + y * 3) as u8);
            target.data[offset + 1] = seed.wrapping_add((x * 7 + y * 11) as u8);
            target.data[offset + 2] = seed.wrapping_add((x * 5 + y * 17) as u8);
        }
    }

    fn stitched_pixel_rgb(frame: &StitchedFrame, x: u32, y: u32) -> [u8; 3] {
        let offset = y as usize * frame.stride as usize + x as usize * 3;
        [
            frame.data[offset],
            frame.data[offset + 1],
            frame.data[offset + 2],
        ]
    }

    fn fixed_band_estimate(delta_y: i32) -> PerceptualMotionEstimate {
        PerceptualMotionEstimate {
            delta_y,
            median: 1.0,
            p75: 1.0,
            p90: 1.0,
            mean: 1.0,
            second_best_delta_y: None,
            second_best_median: None,
            non_adjacent_second_best_delta_y: None,
            non_adjacent_second_best_median: None,
            no_motion_median: Some(20.0),
            separation: Some(19.0),
            overlap_rows: 40,
            band_count: 8,
        }
    }

    fn copy_same_rows(source: &RgbFrame, target: &mut RgbFrame, start_y: u32, height: u32) {
        for y in start_y..start_y + height {
            for x in 0..source.width {
                let src = pixel_offset(source, x, y);
                let dst = pixel_offset(target, x, y);
                target.data[dst..dst + 3].copy_from_slice(&source.data[src..src + 3]);
            }
        }
    }

    fn paint_rows(target: &mut RgbFrame, start_y: u32, height: u32, seed: u8) {
        for y in start_y..start_y + height {
            for x in 0..target.width {
                let dst = pixel_offset(target, x, y);
                target.data[dst] = seed.wrapping_add((x * 5 + y * 11) as u8);
                target.data[dst + 1] = seed.wrapping_add((x * 3 + y * 7) as u8);
                target.data[dst + 2] = seed.wrapping_add((x * 13 + y * 2) as u8);
            }
        }
    }

    #[test]
    fn default_fixed_bands_active_crop_returns_full_frame() {
        let crop = FixedBands::default().active_crop(120, 80).unwrap();

        assert_eq!(
            crop,
            ComposeCrop {
                x: 0,
                y: 0,
                width: 120,
                height: 80,
            }
        );
    }

    #[test]
    fn non_empty_fixed_bands_active_crop_skips_bands() {
        let crop = FixedBands { top: 12, bottom: 8 }
            .active_crop(120, 80)
            .unwrap();

        assert_eq!(
            crop,
            ComposeCrop {
                x: 0,
                y: 12,
                width: 120,
                height: 60,
            }
        );
    }

    #[test]
    fn invalid_fixed_bands_active_crop_returns_none() {
        assert!(FixedBands {
            top: 30,
            bottom: 50
        }
        .active_crop(120, 80)
        .is_none());
        assert!(FixedBands {
            top: 40,
            bottom: 40
        }
        .active_crop(120, 80)
        .is_none());
    }

    #[test]
    fn from_first_frame_fills_full_frame_layout_metadata() {
        let frame = frame(32, 24, 10);
        let stitched = StitchedFrame::from_first_frame(&frame);

        assert_eq!(stitched.width, frame.width);
        assert_eq!(stitched.height, frame.height);
        assert_eq!(stitched.stride, frame.stride);
        assert_eq!(stitched.data, frame.data);
        assert_eq!(stitched.current_origin_x, 0);
        assert_eq!(stitched.current_origin_y, 0);
        assert_eq!(stitched.compose_width, frame.width);
        assert_eq!(stitched.compose_height, frame.height);
        assert_eq!(
            stitched.active_crop,
            ComposeCrop {
                x: 0,
                y: 0,
                width: frame.width,
                height: frame.height,
            }
        );
        assert_eq!(stitched.fixed_bands, FixedBands::default());
        assert!(stitched.fixed_bands.is_empty());
    }

    #[test]
    fn crop_rgb_frame_copies_expected_pixels_and_layout() {
        let frame = frame(5, 4, 10);
        let crop = ComposeCrop {
            x: 1,
            y: 1,
            width: 3,
            height: 2,
        };

        let cropped = crop_rgb_frame(&frame, crop).unwrap();

        assert_eq!(cropped.width, 3);
        assert_eq!(cropped.height, 2);
        assert_eq!(cropped.stride, 9);
        assert_eq!(cropped.data.len(), 18);
        for y in 0..crop.height {
            for x in 0..crop.width {
                let src = pixel_offset(&frame, crop.x + x, crop.y + y);
                let dst = pixel_offset(&cropped, x, y);
                assert_eq!(cropped.data[dst..dst + 3], frame.data[src..src + 3]);
            }
        }
    }

    #[test]
    fn crop_rgb_frame_rejects_out_of_bounds_crop() {
        let frame = frame(5, 4, 10);

        assert!(crop_rgb_frame(
            &frame,
            ComposeCrop {
                x: 3,
                y: 1,
                width: 3,
                height: 2,
            }
        )
        .is_err());
    }

    #[test]
    fn raw_stitcher_sets_default_active_crop_to_full_frame_after_first_push() {
        let frame = frame(32, 24, 10);
        let mut stitcher = RawStitcher::new();

        assert_eq!(
            stitcher.push_frame(frame.clone(), None).unwrap(),
            PushResult::Initialized
        );

        assert_eq!(
            stitcher.active_crop,
            Some(ComposeCrop {
                x: 0,
                y: 0,
                width: frame.width,
                height: frame.height,
            })
        );
        assert_eq!(stitcher.viewport_rect.unwrap().width, frame.width);
        assert_eq!(stitcher.viewport_rect.unwrap().height, frame.height);
    }

    #[test]
    fn fixed_band_detector_detects_fixed_top_band() {
        let previous = textured_frame(48, 60);
        let mut current = shifted_bottom_with_difference(&previous, 8, 12);
        paint_rows(&mut current, 52, 8, 90);
        copy_same_rows(&previous, &mut current, 0, 8);

        let bands = detect_fixed_bands(&previous, &current, Some(fixed_band_estimate(-8))).unwrap();

        assert_eq!(bands.top, 8);
        assert_eq!(bands.bottom, 0);
    }

    #[test]
    fn fixed_band_detector_detects_fixed_bottom_band() {
        let previous = textured_frame(48, 60);
        let mut current = shifted_top_with_tiny_difference(&previous, 8);
        paint_rows(&mut current, 0, 8, 90);
        copy_same_rows(&previous, &mut current, 52, 8);

        let bands = detect_fixed_bands(&previous, &current, Some(fixed_band_estimate(8))).unwrap();

        assert_eq!(bands.top, 0);
        assert_eq!(bands.bottom, 8);
    }

    #[test]
    fn fixed_band_detector_detects_top_and_bottom_bands() {
        let previous = textured_frame(48, 64);
        let mut current = shifted_bottom_with_difference(&previous, 8, 12);
        paint_rows(&mut current, 56, 8, 90);
        copy_same_rows(&previous, &mut current, 0, 8);
        copy_same_rows(&previous, &mut current, 56, 8);

        let bands = detect_fixed_bands(&previous, &current, Some(fixed_band_estimate(-8))).unwrap();

        assert_eq!(bands.top, 8);
        assert_eq!(bands.bottom, 8);
    }

    #[test]
    fn fixed_band_detector_ignores_fixed_region_in_middle() {
        let previous = textured_frame(48, 64);
        let mut current = shifted_bottom_with_difference(&previous, 8, 12);
        paint_rows(&mut current, 56, 8, 90);
        copy_same_rows(&previous, &mut current, 28, 8);

        let bands = detect_fixed_bands(&previous, &current, Some(fixed_band_estimate(-8))).unwrap();

        assert_eq!(bands, FixedBands::default());
    }

    #[test]
    fn fixed_band_detector_ignores_duplicate_no_motion_pair() {
        let previous = textured_frame(48, 60);

        let bands = detect_fixed_bands(&previous, &previous, Some(fixed_band_estimate(0)));

        assert_eq!(bands, None);
    }

    #[test]
    fn fixed_band_detector_ignores_duplicate_compose_pair_with_analysis_motion() {
        let previous = textured_frame(48, 60);

        let bands = detect_fixed_bands(&previous, &previous, Some(fixed_band_estimate(-8)));

        assert_eq!(bands, None);
    }

    #[test]
    fn fixed_band_detector_requires_multiple_stable_observations() {
        let previous = textured_frame(48, 60);
        let mut current = shifted_bottom_with_difference(&previous, 8, 12);
        paint_rows(&mut current, 52, 8, 90);
        copy_same_rows(&previous, &mut current, 0, 8);
        let mut detector = FixedBandDetector::default();

        let first = detector.observe(&previous, &current, Some(fixed_band_estimate(-8)));
        assert_eq!(first.count, 1);
        assert_eq!(detector.stable, FixedBands::default());
        assert!(!detector.frozen);

        let second = detector.observe(&previous, &current, Some(fixed_band_estimate(-8)));
        assert_eq!(second.count, 2);
        assert_eq!(detector.stable, FixedBands { top: 8, bottom: 0 });
        assert!(detector.frozen);
    }

    #[test]
    fn fixed_band_detector_does_not_promote_small_band_after_empty_observation() {
        let empty_previous = textured_frame(48, 60);
        let empty_current = empty_previous.clone();
        let fixed_previous = textured_frame(48, 60);
        let mut fixed_current = shifted_bottom_with_difference(&fixed_previous, 8, 12);
        copy_same_rows(&fixed_previous, &mut fixed_current, 0, 4);
        paint_rows(&mut fixed_current, 52, 8, 90);
        let mut detector = FixedBandDetector::default();

        let empty = detector.observe(
            &empty_previous,
            &empty_current,
            Some(fixed_band_estimate(-8)),
        );
        let fixed = detector.observe(
            &fixed_previous,
            &fixed_current,
            Some(fixed_band_estimate(-8)),
        );

        assert_eq!(empty.bands, FixedBands::default());
        assert_eq!(fixed.bands, FixedBands { top: 4, bottom: 0 });
        assert_eq!(fixed.count, 1);
        assert_eq!(detector.stable, FixedBands::default());
        assert!(!detector.frozen);
    }

    #[test]
    fn detects_append_bottom_shift() {
        let previous = frame(32, 24, 10);
        let current = shifted_bottom(&previous, 2);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Down));
        assert_eq!(result.direction, AppendDirection::Bottom);
        assert_eq!(result.delta_y, 2);
        assert_eq!(result.overlap, 22);
    }

    #[test]
    fn detects_append_top_shift() {
        let previous = frame(32, 24, 10);
        let current = shifted_top(&previous, 2);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Up));
        assert_eq!(result.direction, AppendDirection::Top);
        assert_eq!(result.delta_y, -2);
        assert_eq!(result.overlap, 22);
    }

    #[test]
    fn detects_append_right_shift() {
        let previous = frame(32, 24, 10);
        let current = shifted_right(&previous, 3);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Right));
        assert_eq!(result.direction, AppendDirection::Right);
        assert_eq!(result.delta_x, 3);
        assert_eq!(result.overlap, 29);
    }

    #[test]
    fn detects_append_left_shift() {
        let previous = frame(32, 24, 10);
        let current = shifted_left(&previous, 3);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Left));
        assert_eq!(result.direction, AppendDirection::Left);
        assert_eq!(result.delta_x, -3);
        assert_eq!(result.overlap, 29);
    }

    #[test]
    fn rejects_tiny_overlap() {
        let previous = frame(32, 40, 10);
        let current = shifted_bottom(&previous, 30);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Down));
        assert_eq!(result.overlap, 0);
    }

    #[test]
    fn best_match_beats_smaller_wrong_valid_shift() {
        let previous = frame(32, 24, 10);
        let current = shifted_bottom(&previous, 3);
        let result = find_frame_shift_match(&previous, &current, Some(SearchDirection::Down));
        assert_eq!(result.direction, AppendDirection::Bottom);
        assert_eq!(result.delta_y, 3);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn automatic_mode_detects_vertical_shifts() {
        let previous = frame(32, 24, 10);
        let current_down = shifted_bottom(&previous, 2);
        let down = find_frame_shift_match(&previous, &current_down, None);
        assert_eq!(down.direction, AppendDirection::Bottom);

        let current_up = shifted_top(&previous, 2);
        let up = find_frame_shift_match(&previous, &current_up, None);
        assert_eq!(up.direction, AppendDirection::Top);
    }

    #[test]
    fn automatic_mode_ignores_horizontal_shifts() {
        let previous = frame(32, 24, 10);
        let mut current_right = shifted_right(&previous, 3);
        for y in 0..current_right.height {
            let offset = pixel_offset(&current_right, 0, y);
            current_right.data[offset..offset + 3].copy_from_slice(&[255, 0, 255]);
        }
        let right = find_frame_shift_match(&previous, &current_right, None);
        assert_eq!(right.overlap, 0);

        let mut current_left = shifted_left(&previous, 3);
        for y in 0..current_left.height {
            let offset = pixel_offset(&current_left, current_left.width - 1, y);
            current_left.data[offset..offset + 3].copy_from_slice(&[0, 255, 255]);
        }
        let left = find_frame_shift_match(&previous, &current_left, None);
        assert_eq!(left.overlap, 0);
    }

    #[test]
    fn duplicate_frame_is_rejected() {
        let frame = frame(32, 24, 10);
        let mut stitcher = RawStitcher::new();
        assert_eq!(
            stitcher
                .push_frame(frame.clone(), Some(LongDirection::Down))
                .unwrap(),
            PushResult::Initialized
        );
        assert_eq!(
            stitcher
                .push_frame(frame, Some(LongDirection::Down))
                .unwrap(),
            PushResult::Duplicate
        );
    }

    #[test]
    fn duplicate_frame_is_rejected_before_perceptual_matching() {
        let frame = textured_frame(160, 48);
        let previous_analysis = textured_frame(160, 120);
        let current_analysis = shifted_bottom(&previous_analysis, 12);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame_with_analysis(frame.clone(), previous_analysis, None)
            .unwrap();

        assert_eq!(
            stitcher
                .push_frame_with_analysis(frame, current_analysis, None)
                .unwrap(),
            PushResult::Duplicate
        );
        assert!(stitcher.fixed_detector.pending.is_none());
    }

    #[test]
    fn duplicate_frame_does_not_advance_perceptual_baseline() {
        let frame = textured_frame(160, 48);
        let previous_analysis = textured_frame(160, 120);
        let duplicate_analysis = shifted_bottom_with_difference(&previous_analysis, 12, 6);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame_with_analysis(frame.clone(), previous_analysis, None)
            .unwrap();
        let baseline = stitcher.previous_perceptual_frame.clone().unwrap();

        assert_eq!(
            stitcher
                .push_frame_with_analysis(frame, duplicate_analysis, None)
                .unwrap(),
            PushResult::Duplicate
        );

        let current = stitcher.previous_perceptual_frame.unwrap();
        assert_eq!(current.width, baseline.width);
        assert_eq!(current.height, baseline.height);
        assert_eq!(current.luminance, baseline.luminance);
    }

    #[test]
    fn duplicate_frame_does_not_record_fast_motion_trace() {
        let frame = textured_frame(160, 48);
        let previous_analysis = textured_frame(160, 120);
        let duplicate_analysis = shifted_bottom_with_difference(&previous_analysis, 12, 6);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame_with_analysis(frame.clone(), previous_analysis, None)
            .unwrap();

        assert_eq!(
            stitcher
                .push_frame_with_analysis(frame, duplicate_analysis, None)
                .unwrap(),
            PushResult::Duplicate
        );
        assert!(stitcher.last_fast_motion_trace().is_none());
    }

    #[test]
    fn push_frame_with_analysis_rejects_short_rgb_frame_without_panic() {
        let mut bad = frame(8, 8, 10);
        bad.data.truncate(3);
        let mut stitcher = RawStitcher::new();

        assert!(stitcher
            .push_frame_with_analysis(bad.clone(), bad, None)
            .is_err());
    }

    #[test]
    fn push_frame_uses_perceptual_bottom_when_old_match_is_brittle() {
        let previous = textured_frame(160, 48);
        let current = shifted_bottom_with_difference(&previous, 12, 6);
        assert_eq!(
            find_frame_shift_match(&previous, &current, Some(SearchDirection::Down)).overlap,
            0
        );
        let estimate = estimate_vertical_perceptual_motion(&previous, &current).unwrap();
        assert_eq!(estimate.delta_y, -12);
        assert!(estimate.no_motion_median.unwrap() > estimate.median + PERCEPTUAL_MOTION_MARGIN);

        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Down))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Down))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected perceptual acceptance, got {result:?}");
        };
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(match_info.delta_y, 12);
        assert_eq!(match_info.overlap, 36);
        assert!(match_info.score > MATCH_LINE_MAX_AVERAGE_DIFF);
    }

    #[test]
    fn push_frame_with_analysis_rejects_unsafe_hash_analysis_when_compose_frames_do_not_match() {
        let previous_compose = frame(160, 48, 10);
        let current_compose = frame(160, 48, 90);
        assert_eq!(
            find_frame_shift_match(
                &previous_compose,
                &current_compose,
                Some(SearchDirection::Down)
            )
            .overlap,
            0
        );

        let previous_analysis = textured_frame(160, 120);
        let current_analysis = shifted_bottom_with_difference(&previous_analysis, 12, 1);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame_with_analysis(
                previous_compose,
                previous_analysis,
                Some(LongDirection::Down),
            )
            .unwrap();
        let result = stitcher
            .push_frame_with_analysis(current_compose, current_analysis, Some(LongDirection::Down))
            .unwrap();

        assert_eq!(result, PushResult::NoMatch);
    }

    #[test]
    fn nomatch_recovery_replaces_stale_bottom_edge_after_known_down_scroll() {
        let width = 64;
        let first = frame_from_world_rows(width, &(0..48).collect::<Vec<_>>());
        let normal_next = frame_from_world_rows(width, &(12..60).collect::<Vec<_>>());
        let mut stale_edge = frame_from_world_rows(width, &(24..72).collect::<Vec<_>>());
        for y in 36..48 {
            paint_full_row(&mut stale_edge, y, 170 + y as u8);
        }
        let refreshed = frame_from_world_rows(width, &(48..96).collect::<Vec<_>>());
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        let normal_result = stitcher
            .push_frame(normal_next, Some(LongDirection::Down))
            .unwrap();
        assert!(matches!(normal_result, PushResult::Accepted { .. }));
        let stale_result = stitcher
            .push_frame(stale_edge, Some(LongDirection::Down))
            .unwrap();
        assert!(matches!(stale_result, PushResult::Accepted { .. }));

        let result = stitcher
            .push_frame(refreshed, Some(LongDirection::Down))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected stale-edge recovery acceptance, got {result:?}");
        };
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        let stitched = stitcher.stitched.as_ref().unwrap();
        assert_eq!(stitched.height, 96);
        assert_eq!(stitched.current_origin_y, 48);
        assert_eq!(
            stitched_pixel_rgb(stitched, 11, 70),
            row_texture(width, 70)[33..36]
        );
        assert_eq!(
            stitched_pixel_rgb(stitched, 11, 95),
            row_texture(width, 95)[33..36]
        );
    }

    fn image_from_rgb_frame(frame: &RgbFrame) -> Image {
        let stride = frame.width * 4;
        let mut data = vec![0; stride as usize * frame.height as usize];
        for y in 0..frame.height {
            for x in 0..frame.width {
                let src = pixel_offset(frame, x, y);
                let dst = (y * stride + x * 4) as usize;
                data[dst] = frame.data[src + 2];
                data[dst + 1] = frame.data[src + 1];
                data[dst + 2] = frame.data[src];
                data[dst + 3] = 255;
            }
        }
        Image {
            width: frame.width,
            height: frame.height,
            stride,
            format: Format::Xrgb8888,
            data,
        }
    }

    #[test]
    fn push_frame_views_matches_rgb_frame_path_with_compose_crop() {
        let crop_y = 4;
        let previous_analysis = textured_frame(160, 120);
        let current_analysis = shifted_bottom_with_difference(&previous_analysis, 12, 6);
        let crop = ComposeCrop {
            x: 0,
            y: crop_y,
            width: 160,
            height: 48,
        };
        let previous_compose = crop_rgb_frame(&previous_analysis, crop).unwrap();
        let current_compose = crop_rgb_frame(&current_analysis, crop).unwrap();
        let previous_image = image_from_rgb_frame(&previous_analysis);
        let current_image = image_from_rgb_frame(&current_analysis);

        let mut rgb_stitcher = RawStitcher::new();
        rgb_stitcher
            .push_frame_with_analysis(
                previous_compose,
                previous_analysis,
                Some(LongDirection::Down),
            )
            .unwrap();
        let rgb_result = rgb_stitcher
            .push_frame_with_analysis(current_compose, current_analysis, Some(LongDirection::Down))
            .unwrap();

        let mut view_stitcher = RawStitcher::new();
        view_stitcher
            .push_frame_views(
                ImageRgbView::with_crop(&previous_image, crop).unwrap(),
                ImageRgbView::new(&previous_image).unwrap(),
                Some(LongDirection::Down),
            )
            .unwrap();
        let view_result = view_stitcher
            .push_frame_views(
                ImageRgbView::with_crop(&current_image, crop).unwrap(),
                ImageRgbView::new(&current_image).unwrap(),
                Some(LongDirection::Down),
            )
            .unwrap();

        assert_eq!(view_result, rgb_result);
        assert_eq!(view_stitcher.finish(), rgb_stitcher.finish());
    }

    #[test]
    fn perceptual_frame_match_rejects_when_not_better_than_zero() {
        let estimate = PerceptualMotionEstimate {
            delta_y: -8,
            median: 3.0,
            p75: 3.0,
            p90: 3.0,
            mean: 3.0,
            second_best_delta_y: None,
            second_best_median: None,
            non_adjacent_second_best_delta_y: None,
            non_adjacent_second_best_median: None,
            no_motion_median: Some(3.4),
            separation: None,
            overlap_rows: 40,
            band_count: 9,
        };
        let frame = textured_frame(160, 48);

        assert_eq!(
            perceptual_frame_match(Some(estimate), &frame, Some(SearchDirection::Down)),
            Err("weak-zero-margin")
        );
    }

    #[test]
    fn perceptual_frame_match_rejects_ambiguous_non_adjacent_candidate() {
        let estimate = PerceptualMotionEstimate {
            delta_y: -8,
            median: 3.0,
            p75: 3.0,
            p90: 3.0,
            mean: 3.0,
            second_best_delta_y: Some(-20),
            second_best_median: Some(3.2),
            non_adjacent_second_best_delta_y: Some(-20),
            non_adjacent_second_best_median: Some(3.2),
            no_motion_median: Some(20.0),
            separation: Some(0.2),
            overlap_rows: 40,
            band_count: 9,
        };
        let frame = textured_frame(160, 48);

        assert_eq!(
            perceptual_frame_match(Some(estimate), &frame, Some(SearchDirection::Down)),
            Err("weak-second-margin")
        );
    }

    #[test]
    fn perceptual_frame_match_allows_adjacent_second_best() {
        let estimate = PerceptualMotionEstimate {
            delta_y: -8,
            median: 3.0,
            p75: 3.0,
            p90: 3.0,
            mean: 3.0,
            second_best_delta_y: Some(-9),
            second_best_median: Some(3.1),
            non_adjacent_second_best_delta_y: Some(-20),
            non_adjacent_second_best_median: Some(5.0),
            no_motion_median: Some(20.0),
            separation: Some(2.0),
            overlap_rows: 40,
            band_count: 9,
        };
        let frame = textured_frame(160, 48);

        let match_info =
            perceptual_frame_match(Some(estimate), &frame, Some(SearchDirection::Down)).unwrap();
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(match_info.delta_y, 8);
    }

    #[test]
    fn vertical_mode_rejects_horizontal_motion() {
        let previous = textured_frame(160, 48);
        let current = shifted_right(&previous, 12);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Down))
            .unwrap();

        assert_eq!(
            stitcher
                .push_frame(current, Some(LongDirection::Down))
                .unwrap(),
            PushResult::NoMatch
        );
    }

    #[test]
    fn perceptual_motion_prefers_small_positive_shift_over_zero_with_tiny_differences() {
        let previous = textured_frame(160, 48);
        let current = shifted_top_with_tiny_difference(&previous, 1);

        let estimate = estimate_vertical_perceptual_motion(&previous, &current).unwrap();

        assert_eq!(estimate.delta_y, 1);
        assert!(estimate.median < 2.0);
        assert!(estimate.second_best_delta_y.is_some());
        assert!(estimate.second_best_median.is_some());
        assert!(estimate.no_motion_median.is_some());
        assert_eq!(estimate.overlap_rows, 47);
    }

    #[test]
    fn perceptual_motion_uses_positive_and_negative_dy_convention() {
        let previous = textured_frame(160, 48);
        let current_positive = shifted_top(&previous, 3);
        let current_negative = shifted_bottom(&previous, 4);

        let positive = estimate_vertical_perceptual_motion(&previous, &current_positive).unwrap();
        let negative = estimate_vertical_perceptual_motion(&previous, &current_negative).unwrap();

        assert_eq!(positive.delta_y, 3);
        assert_eq!(negative.delta_y, -4);
    }

    #[test]
    fn fast_motion_candidate_detects_vertical_delta_signs() {
        let previous = PerceptualFrame::from_source(&textured_frame(160, 48));
        let bottom = PerceptualFrame::from_source(&shifted_bottom(&textured_frame(160, 48), 4));
        let top = PerceptualFrame::from_source(&shifted_top(&textured_frame(160, 48), 3));

        let bottom_candidate =
            scan_fast_vertical_motion_candidate(&previous, &bottom, Some(SearchDirection::Down))
                .unwrap();
        let top_candidate =
            scan_fast_vertical_motion_candidate(&previous, &top, Some(SearchDirection::Up))
                .unwrap();

        assert_eq!(bottom_candidate.delta_y, -4);
        assert_eq!(top_candidate.delta_y, 3);
        assert!(
            scan_fast_vertical_motion_candidate(&previous, &bottom, Some(SearchDirection::Up))
                .is_none()
        );
    }

    #[test]
    fn fast_motion_ranked_scan_orders_lower_scores_first() {
        let previous = PerceptualFrame::from_source(&textured_frame(160, 80));
        let current = PerceptualFrame::from_source(&shifted_bottom(&textured_frame(160, 80), 11));

        let scan =
            scan_fast_vertical_motion(&previous, &current, Some(SearchDirection::Down)).unwrap();

        assert_eq!(scan.ranked.first().unwrap().delta_y, -11);
        assert!(scan
            .ranked
            .windows(2)
            .all(|window| compare_fast_motion_candidates(&window[0], &window[1]).is_le()));
        assert_eq!(scan.candidate.unwrap().delta_y, -11);
    }

    #[test]
    fn progressive_perceptual_search_accepts_fast_top_candidate() {
        let previous = textured_frame(160, 80);
        let current = shifted_bottom_with_difference(&previous, 12, 6);
        let previous_frame = PerceptualFrame::from_source(&previous);
        let current_frame = PerceptualFrame::from_source(&current);
        let scan =
            scan_fast_vertical_motion(&previous_frame, &current_frame, Some(SearchDirection::Down))
                .unwrap();

        assert!(scan
            .ranked
            .iter()
            .take(FAST_MOTION_PERCEPTUAL_FIRST_PASS)
            .any(|candidate| candidate.delta_y == -12));
        let (estimate, verify_pass) = estimate_vertical_perceptual_motion_from_ranked_deltas(
            &previous_frame,
            &current_frame,
            &current,
            Some(SearchDirection::Down),
            &scan.ranked,
        );
        let estimate = estimate.unwrap();

        assert_eq!(estimate.delta_y, -12);
        assert_eq!(verify_pass, Some(FastMotionVerifyPass::Top20));
        let match_info =
            perceptual_frame_match(Some(estimate), &current, Some(SearchDirection::Down)).unwrap();
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(match_info.delta_y, 12);
    }

    #[test]
    fn progressive_perceptual_search_falls_back_when_top_candidates_miss() {
        let previous = textured_frame(160, 80);
        let current = shifted_bottom_with_difference(&previous, 12, 6);
        let previous_frame = PerceptualFrame::from_source(&previous);
        let current_frame = PerceptualFrame::from_source(&current);
        let ranked: Vec<_> = (1..=FAST_MOTION_PERCEPTUAL_SECOND_PASS as i32)
            .map(|delta_y| {
                score_fast_motion_delta(&previous_frame, &current_frame, delta_y).unwrap()
            })
            .collect();

        let (estimate, verify_pass) = estimate_vertical_perceptual_motion_from_ranked_deltas(
            &previous_frame,
            &current_frame,
            &current,
            Some(SearchDirection::Down),
            &ranked,
        );
        assert!(estimate.is_none());
        assert_eq!(verify_pass, None);
        let full = estimate_vertical_perceptual_motion_from_frame(&previous_frame, &current_frame)
            .unwrap();
        assert_eq!(full.delta_y, -12);
    }

    #[test]
    fn progressive_perceptual_search_falls_back_when_partial_is_ambiguous() {
        let previous = periodic_textured_frame(160, 80, 10);
        let current = shifted_bottom_with_difference(&previous, 12, 6);
        let previous_frame = PerceptualFrame::from_source(&previous);
        let current_frame = PerceptualFrame::from_source(&current);
        let ranked = vec![
            score_fast_motion_delta(&previous_frame, &current_frame, -12).unwrap(),
            score_fast_motion_delta(&previous_frame, &current_frame, -22).unwrap(),
        ];

        let (estimate, verify_pass) = estimate_vertical_perceptual_motion_from_ranked_deltas(
            &previous_frame,
            &current_frame,
            &current,
            Some(SearchDirection::Down),
            &ranked,
        );
        assert!(estimate.is_none());
        assert_eq!(verify_pass, None);
        let full = estimate_vertical_perceptual_motion_from_frame(&previous_frame, &current_frame)
            .unwrap();
        assert_eq!(full.delta_y, -12);
    }

    #[test]
    fn fast_motion_trace_compares_against_negative_frame_match_delta() {
        let candidate = FastMotionCandidate {
            delta_y: -12,
            score: 0.5,
            overlap_rows: 36,
        };
        let match_info = FrameMatch {
            direction: AppendDirection::Bottom,
            overlap: 36,
            delta_x: 0,
            delta_y: 12,
            score: 1.0,
        };

        let trace = compare_fast_motion_candidate(
            Some(candidate),
            Some(match_info),
            false,
            Some(FastMotionVerifyPass::Top20),
        );

        assert_eq!(trace.reference_delta_y, Some(-12));
        assert_eq!(trace.agreement, FastMotionAgreement::HeavyExactDelta);
        assert_eq!(trace.verify_pass, Some(FastMotionVerifyPass::Top20));
    }

    #[test]
    fn fast_motion_candidate_frame_match_uses_append_delta_sign() {
        let frame = textured_frame(160, 48);
        let bottom = fast_motion_candidate_frame_match(
            FastMotionCandidate {
                delta_y: -12,
                score: 0.5,
                overlap_rows: 36,
            },
            &frame,
            Some(SearchDirection::Down),
        )
        .unwrap();
        let top = fast_motion_candidate_frame_match(
            FastMotionCandidate {
                delta_y: 12,
                score: 0.5,
                overlap_rows: 36,
            },
            &frame,
            Some(SearchDirection::Up),
        )
        .unwrap();

        assert_eq!(bottom.direction, AppendDirection::Bottom);
        assert_eq!(bottom.delta_y, 12);
        assert_eq!(bottom.overlap, 36);
        assert_eq!(top.direction, AppendDirection::Top);
        assert_eq!(top.delta_y, -12);
        assert_eq!(top.overlap, 36);
    }

    #[test]
    fn fast_motion_candidate_frame_match_rejects_direction_mismatch() {
        let frame = textured_frame(160, 48);

        assert_eq!(
            fast_motion_candidate_frame_match(
                FastMotionCandidate {
                    delta_y: -12,
                    score: 0.5,
                    overlap_rows: 36,
                },
                &frame,
                Some(SearchDirection::Up),
            ),
            Err("direction-mismatch")
        );
    }

    #[test]
    fn perceptual_motion_returns_no_estimate_for_too_small_frames() {
        let previous = textured_frame(160, PERCEPTUAL_MOTION_BAND_HEIGHT - 1);
        let current = previous.clone();

        assert!(estimate_vertical_perceptual_motion(&previous, &current).is_none());
    }

    #[test]
    fn perceptual_motion_returns_no_estimate_when_all_deltas_have_no_overlap() {
        let previous = textured_frame(16, 8);
        let current = textured_frame(16, 8);
        let config = PerceptualMotionConfig {
            delta_range: 0,
            band_height: 9,
            band_step: 1,
            bins: 4,
        };

        assert!(
            estimate_vertical_perceptual_motion_with_config(&previous, &current, config).is_none()
        );
    }

    #[test]
    fn perceptual_motion_delta_score_returns_no_estimate_without_overlap() {
        let previous = textured_frame(16, 8);
        let current = textured_frame(16, 8);
        let config = PerceptualMotionConfig {
            delta_range: 150,
            band_height: 8,
            band_step: 4,
            bins: 4,
        };
        let previous_frame = PerceptualFrame::from_source(&previous);
        let current_frame = PerceptualFrame::from_source(&current);
        let previous_signatures =
            precompute_perceptual_band_signatures(&previous_frame, 16, config);
        let current_signatures = precompute_perceptual_band_signatures(&current_frame, 16, config);
        let mut distances = Vec::new();

        assert!(score_perceptual_delta(
            &previous_frame,
            &current_frame,
            &previous_signatures,
            &current_signatures,
            8,
            config,
            &mut distances,
        )
        .is_none());
    }

    #[test]
    fn append_expands_bottom() {
        let previous = frame(32, 24, 10);
        let current = shifted_bottom(&previous, 2);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Down))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Down))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.height, 26);
        assert_eq!(stitched.current_origin_y, 2);
        assert_eq!(stitched.anchor_origin_y, 0);
    }

    #[test]
    fn append_expands_top() {
        let previous = frame(32, 24, 10);
        let current = shifted_top(&previous, 2);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Up))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Up))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.height, 26);
        assert_eq!(stitched.current_origin_y, 0);
        assert_eq!(stitched.anchor_origin_y, 2);
    }

    #[test]
    fn append_expands_right() {
        let previous = frame(32, 24, 10);
        let current = shifted_right(&previous, 4);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Right))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Right))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.width, 36);
        assert_eq!(stitched.current_origin_x, 4);
        assert_eq!(stitched.anchor_origin_x, 0);
    }

    #[test]
    fn append_expands_left() {
        let previous = frame(32, 24, 10);
        let current = shifted_left(&previous, 3);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Left))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Left))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.width, 35);
        assert_eq!(stitched.current_origin_x, 0);
        assert_eq!(stitched.anchor_origin_x, 3);
    }

    #[test]
    fn canvas_placement_updates_viewport_for_contained_reverse_scroll() {
        let world = textured_frame(32, 80);
        let first = crop_frame(&world, 0, 20, 32, 24);
        let bottom = crop_frame(&world, 0, 28, 32, 24);
        let contained = crop_frame(&world, 0, 22, 32, 24);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        assert!(matches!(
            stitcher
                .push_frame(bottom, Some(LongDirection::Down))
                .unwrap(),
            PushResult::Accepted { .. }
        ));
        let before = stitcher.stitched.clone().unwrap();

        let result = stitcher
            .push_frame(contained, Some(LongDirection::Up))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected contained canvas placement, got {result:?}");
        };
        assert_eq!(match_info.delta_y, -6);
        assert_eq!(stitcher.viewport_rect.unwrap().y, 2);
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!(after.width, before.width);
        assert_eq!(after.height, before.height);
        assert_eq!(after.data, before.data);
    }

    #[test]
    fn canvas_placement_appends_only_partial_top_extension() {
        let world = textured_frame(32, 80);
        let first = crop_frame(&world, 0, 20, 32, 24);
        let bottom = crop_frame(&world, 0, 28, 32, 24);
        let top = crop_frame(&world, 0, 12, 32, 24);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        stitcher
            .push_frame(bottom, Some(LongDirection::Down))
            .unwrap();
        let bottom_pixel_before = stitched_pixel_rgb(stitcher.stitched.as_ref().unwrap(), 11, 31);

        let result = stitcher.push_frame(top, Some(LongDirection::Up)).unwrap();

        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.stitched.as_ref().unwrap();
        assert_eq!(stitched.height, 40);
        assert_eq!(stitched.current_origin_y, 0);
        assert_eq!(stitched_pixel_rgb(stitched, 11, 0), world.pixel_rgb(11, 12));
        assert_eq!(stitched_pixel_rgb(stitched, 11, 39), bottom_pixel_before);
        assert_eq!(
            stitched_pixel_rgb(stitched, 11, 39),
            world.pixel_rgb(11, 51)
        );
    }

    #[test]
    fn canvas_placement_updates_viewport_for_contained_horizontal_reverse_scroll() {
        let world = textured_frame(80, 24);
        let first = crop_frame(&world, 20, 0, 24, 24);
        let right = crop_frame(&world, 28, 0, 24, 24);
        let contained = crop_frame(&world, 22, 0, 24, 24);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Right))
            .unwrap();
        stitcher
            .push_frame(right, Some(LongDirection::Right))
            .unwrap();
        let before = stitcher.stitched.clone().unwrap();

        let result = stitcher
            .push_frame(contained, Some(LongDirection::Left))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected contained canvas placement, got {result:?}");
        };
        assert_eq!(match_info.delta_x, -6);
        assert_eq!(stitcher.viewport_rect.unwrap().x, 2);
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!(after.width, before.width);
        assert_eq!(after.height, before.height);
        assert_eq!(after.data, before.data);
    }

    #[test]
    fn canvas_placement_finds_far_vertical_match_via_coarse_refine_search() {
        let world = textured_frame(32, 420);
        let stitched = StitchedFrame::from_first_frame(&world);
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: 32,
            height: 64,
        };
        let target = crop_frame(&world, 0, 217, 32, 64);

        let match_info = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target,
            SearchAxis::Vertical,
            Some(SearchDirection::Down),
        )
        .expect("expected far canvas placement match");

        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(match_info.delta_y, 217);
        assert_eq!(match_info.overlap, 64);
        assert!(match_info.score <= MATCH_PREFILTER_MAX_AVERAGE_DIFF);
    }

    #[test]
    fn canvas_placement_rejects_opposite_vertical_direction() {
        let world = textured_frame(32, 120);
        let stitched = StitchedFrame::from_first_frame(&crop_frame(&world, 0, 24, 32, 48));
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: 32,
            height: 48,
        };
        let target_above = crop_frame(&world, 0, 16, 32, 48);
        let target_below = crop_frame(&world, 0, 32, 32, 48);

        let down = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target_above,
            SearchAxis::Vertical,
            Some(SearchDirection::Down),
        );
        let up = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target_below,
            SearchAxis::Vertical,
            Some(SearchDirection::Up),
        );

        assert!(down.is_none());
        assert!(up.is_none());
    }

    #[test]
    fn canvas_placement_rejects_opposite_horizontal_direction() {
        let world = textured_frame(120, 32);
        let stitched = StitchedFrame::from_first_frame(&crop_frame(&world, 24, 0, 48, 32));
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: 48,
            height: 32,
        };
        let target_left = crop_frame(&world, 16, 0, 48, 32);
        let target_right = crop_frame(&world, 32, 0, 48, 32);

        let right = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target_left,
            SearchAxis::Horizontal,
            Some(SearchDirection::Right),
        );
        let left = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target_right,
            SearchAxis::Horizontal,
            Some(SearchDirection::Left),
        );

        assert!(right.is_none());
        assert!(left.is_none());
    }

    #[test]
    fn placement_pipeline_canvas_accept_matches_canvas_search() {
        let world = textured_frame(32, 420);
        let stitched = StitchedFrame::from_first_frame(&world);
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: 32,
            height: 64,
        };
        let previous_region = extract_stitched_region(&stitched, previous_rect).unwrap();
        let target = crop_frame(&world, 0, 217, 32, 64);
        let expected = find_canvas_placement_match(
            &stitched,
            previous_rect,
            &target,
            SearchAxis::Vertical,
            Some(SearchDirection::Down),
        )
        .unwrap();
        let mut pipeline = DefaultPlacementPipeline;

        let outcome = pipeline.place(PlacementInput {
            stitched: &stitched,
            previous_rect,
            previous_region: &previous_region,
            active_compose: &target,
            analysis_frame: &target,
            previous_perceptual_frame: None,
            search_direction: Some(SearchDirection::Down),
            profile_enabled: true,
        });

        assert_eq!(outcome.candidates.len(), 1);
        assert_eq!(outcome.candidates[0].source, PlacementSource::Canvas);
        assert_eq!(outcome.candidates[0].match_info, expected);
        assert_eq!(
            outcome.profile.path,
            Some(StitchDecisionPath::CanvasAccepted)
        );
    }

    #[test]
    fn placement_pipeline_delays_perceptual_prepare_when_canvas_accepts() {
        let world = textured_frame(32, 100);
        let stitched = StitchedFrame::from_first_frame(&world);
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: 32,
            height: 40,
        };
        let previous_region = extract_stitched_region(&stitched, previous_rect).unwrap();
        let target = crop_frame(&world, 0, 12, 32, 40);
        let previous_perceptual = PerceptualFrame::from_source(&previous_region);
        let mut pipeline = DefaultPlacementPipeline;

        let outcome = pipeline.place(PlacementInput {
            stitched: &stitched,
            previous_rect,
            previous_region: &previous_region,
            active_compose: &target,
            analysis_frame: &target,
            previous_perceptual_frame: Some(&previous_perceptual),
            search_direction: Some(SearchDirection::Down),
            profile_enabled: true,
        });

        assert_eq!(outcome.candidates[0].source, PlacementSource::Canvas);
        assert!(outcome.current_perceptual_frame.is_none());
        assert_eq!(outcome.perceptual_estimate, None);
        assert_eq!(outcome.profile.perceptual_prepare, Duration::default());
    }

    #[test]
    fn placement_pipeline_returns_fallback_candidate_without_mutating_stitcher_state() {
        let previous = textured_frame(64, 40);
        let current = shifted_right(&previous, 6);
        let stitched = StitchedFrame::from_first_frame(&frame(64, 40, 220));
        let previous_rect = ViewportRect {
            x: 0,
            y: 0,
            width: previous.width,
            height: previous.height,
        };
        let before = stitched.clone();
        let mut pipeline = DefaultPlacementPipeline;

        let outcome = pipeline.place(PlacementInput {
            stitched: &stitched,
            previous_rect,
            previous_region: &previous,
            active_compose: &current,
            analysis_frame: &current,
            previous_perceptual_frame: None,
            search_direction: Some(SearchDirection::Right),
            profile_enabled: true,
        });

        assert!(outcome
            .candidates
            .iter()
            .any(|candidate| candidate.source == PlacementSource::PreviousFrameFallback));
        assert_eq!(stitched, before);
    }

    #[test]
    fn append_preserves_old_pixels_in_overlap() {
        let previous = frame(32, 24, 10);
        let mut stitched = StitchedFrame::from_first_frame(&previous);
        let mut current = shifted_bottom(&previous, 4);
        for y in 0..20 {
            for x in 0..32 {
                let offset = pixel_offset(&current, x, y);
                current.data[offset..offset + 3].copy_from_slice(&[250, 250, 250]);
            }
        }
        let match_info = FrameMatch {
            direction: AppendDirection::Bottom,
            overlap: 20,
            delta_x: 0,
            delta_y: 4,
            score: 0.0,
        };
        let new_rect = append_frame_at_position(
            &mut stitched,
            ViewportRect {
                x: 0,
                y: 0,
                width: previous.width,
                height: previous.height,
            },
            &current,
            match_info,
        )
        .unwrap();
        assert_eq!(new_rect.y, 4);
        assert_eq!(stitched.height, 28);

        let old_overlap = pixel_offset(&previous, 7, 9);
        let stitched_overlap = pixel_offset(
            &RgbFrame {
                width: stitched.width,
                height: stitched.height,
                stride: stitched.stride,
                data: stitched.data.clone(),
            },
            7,
            9,
        );
        assert_eq!(
            stitched.data[stitched_overlap..stitched_overlap + 3],
            previous.data[old_overlap..old_overlap + 3]
        );

        let current_tail = pixel_offset(&current, 7, 23);
        let stitched_tail = (27 * stitched.stride + 7 * 3) as usize;
        assert_eq!(
            stitched.data[stitched_tail..stitched_tail + 3],
            current.data[current_tail..current_tail + 3]
        );
    }

    #[test]
    fn recovery_append_overwrites_only_proven_overlap_rows() {
        let previous = frame(32, 24, 10);
        let mut stitched = StitchedFrame::from_first_frame(&previous);
        let mut current = shifted_bottom(&previous, 4);
        for y in 0..20 {
            for x in 0..32 {
                let offset = pixel_offset(&current, x, y);
                current.data[offset..offset + 3].copy_from_slice(&[250, 250, 250]);
            }
        }
        let match_info = FrameMatch {
            direction: AppendDirection::Bottom,
            overlap: 20,
            delta_x: 0,
            delta_y: 4,
            score: 0.0,
        };

        append_frame_at_position_overwriting_overlap_rows(
            &mut stitched,
            ViewportRect {
                x: 0,
                y: 0,
                width: previous.width,
                height: previous.height,
            },
            &current,
            match_info,
            8,
            6,
        )
        .unwrap();

        assert_eq!(stitched.height, 28);
        assert_eq!(
            stitched_pixel_rgb(&stitched, 7, 11),
            previous.pixel_rgb(7, 11)
        );
        assert_eq!(
            stitched_pixel_rgb(&stitched, 7, 12),
            current.pixel_rgb(7, 8)
        );
        assert_eq!(
            stitched_pixel_rgb(&stitched, 7, 17),
            current.pixel_rgb(7, 13)
        );
        assert_eq!(
            stitched_pixel_rgb(&stitched, 7, 18),
            previous.pixel_rgb(7, 18)
        );
        assert_eq!(
            stitched_pixel_rgb(&stitched, 7, 27),
            current.pixel_rgb(7, 23)
        );
    }

    #[test]
    fn raw_overlap_recovery_detects_refresh_dirty_band() {
        let previous = frame(834, 491, 0);
        let mut current = shifted_bottom(&previous, 24);
        paint_rows(&mut current, 396, 37, 215);
        let match_info = FrameMatch {
            direction: AppendDirection::Bottom,
            overlap: 467,
            delta_x: 0,
            delta_y: 24,
            score: 0.45,
        };

        let recovery = direct_bottom_raw_overlap_recovery(&previous, &current, match_info)
            .expect("expected dirty overlap recovery");

        assert_eq!(recovery.overwrite_frame_y, 396);
        assert_eq!(recovery.overwrite_rows, 37);
    }

    #[test]
    fn converts_stitched_to_xrgb_image() {
        let previous = frame(2, 2, 10);
        let stitched = StitchedFrame::from_first_frame(&previous);
        let image = image_from_stitched_frame(&stitched).unwrap();
        assert_eq!(image.format, Format::Xrgb8888);
        assert_eq!(image.stride, 8);
        assert_eq!(image.data[0], previous.data[2]);
        assert_eq!(image.data[1], previous.data[1]);
        assert_eq!(image.data[2], previous.data[0]);
        assert_eq!(image.data[3], 255);
    }
}
