use std::error::Error;
use std::time::{Duration, Instant};

#[cfg(test)]
use wayland_client::protocol::wl_shm::Format;

#[cfg(test)]
use crate::image::Image;


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

    fn push_frame_sources<C, A>(
        &mut self,
        compose_frame: &C,
        analysis_frame: &A,
        direction: Option<SearchDirection>,
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
            search_direction: direction,
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

    pub fn push_frame_views(
        &mut self,
        compose_frame: ImageRgbView<'_>,
        analysis_frame: ImageRgbView<'_>,
        direction: Option<SearchDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_sources(&compose_frame, &analysis_frame, direction)
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
    use crate::image::Image as CrateImage;
    use wayland_client::protocol::wl_shm::Format as WlFormat;

    #[test]
    fn raw_stitcher_creates_and_accepts() {
        let mut stitcher = RawStitcher::new();

        fn make_image(width: u32, rows: &[u8]) -> CrateImage {
            let stride = width * 4;
            CrateImage { width, height: rows.len() as u32, stride, format: WlFormat::Xrgb8888, data: rows.iter().flat_map(|v| [*v; 4]).collect() }
        }

        let img1 = make_image(1, &[10, 20, 30]);
        let img1b = make_image(1, &[10, 20, 30]);
        let img2 = make_image(1, &[20, 30, 40]);
        let img2b = make_image(1, &[20, 30, 40]);

        let view1 = ImageRgbView::new(&img1).unwrap();
        let view1b = ImageRgbView::new(&img1b).unwrap();
        let view2 = ImageRgbView::new(&img2).unwrap();
        let view2b = ImageRgbView::new(&img2b).unwrap();

        assert_eq!(stitcher.push_frame_views(view1, view1b, Some(SearchDirection::Down)).unwrap(), PushResult::Initialized);

        let result = stitcher.push_frame_views(view2, view2b, Some(SearchDirection::Down)).unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.height, 4);
    }
}
