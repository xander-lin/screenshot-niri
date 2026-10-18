use std::error::Error;
#[cfg(any(test, feature = "stitch-legacy"))]
use std::time::Duration;
use std::time::Instant;

#[cfg(test)]
use wayland_client::protocol::wl_shm::Format;

#[cfg(test)]
use crate::image::Image;
use crate::wayland::selection::LongDirection;

mod canvas;
mod engine;
mod fixed_bands;
mod hash;
mod motion;
mod perceptual;
mod placement;
mod rgb;
mod types;

use canvas::{
    append_frame_at_position, append_frame_at_position_overwriting_overlap_rows,
    canvas_overlap_rect, extract_stitched_region, translate_viewport_rect,
    viewport_rect_within_stitched,
};
#[cfg(test)]
use fixed_bands::detect_fixed_bands;
use fixed_bands::{FixedBandDetector, FixedBandObservation};
#[cfg(any(test, feature = "stitch-legacy"))]
use hash::estimate_vertical_perceptual_hash_motion_from_frame;
#[cfg(any(test, feature = "stitch-legacy"))]
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
#[cfg(any(test, feature = "stitch-legacy"))]
use perceptual::perceptual_frame_match;
use perceptual::{PerceptualFrame, PerceptualMotionEstimate, PERCEPTUAL_MOTION_ADJACENT_DELTA};
#[cfg(feature = "stitch-legacy")]
use placement::canvas_search_axis;
use placement::{find_canvas_placement_match, SearchAxis};
use engine::{EngineDecision, PlacementEngine, PlacementRequest, WindowedEdgeMatcher};
use rgb::{
    average_difference_same_source, average_luminance_difference_source, crop_rgb_frame,
    pixel_difference_source, pixel_offset, rgb_frame_from_source, validate_rgb_frame,
    validate_rgb_source, CroppedRgbSource,
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
pub use rgb::{average_difference_same, rgb_frame_from_image, ImageRgbView, RgbSource};
pub use engine::RejectReason;
pub use types::{
    AppendDirection, ComposeCrop, DuplicateAnalysisMotion, FastMotionAgreement,
    FastMotionCandidate, FastMotionTrace, FastMotionVerifyPass, FixedBands, FrameMatch,
    FrameScreenOffset, MotionEstimate, PushResult, RgbFrame, SearchDirection, StitchDecisionPath,
    StitchProfileBreakdown, StitchedFrame, ViewportRect,
};

const MATCH_LINE_MAX_AVERAGE_DIFF: f64 = 3.0;
const MATCH_LINE_SAMPLE_STEP: u32 = 4;
const MATCH_PREFILTER_MAX_AVERAGE_DIFF: f64 = 12.0;
const MATCH_PREFILTER_SAMPLE_STEP: u32 = 2;
const MATCH_MIN_OVERLAP_PIXELS: u32 = 16;
const FAST_MOTION_MAX_SCORE: f64 = 8.0;
const DUPLICATE_ANALYSIS_ROW_DIFF_THRESHOLD: f64 = 3.0;
/// Rows of *proven* overlap the stale-edge recovery needs before it may place a
/// frame, as a fraction of the frame height (with an absolute floor).
///
/// A fixed floor of 8 was too few: on real 700-row content it accepted a
/// 212-row offset off a 10-row prefix. The recovery runs only after every
/// matcher gate has refused, so a thin prefix here silently overrides all of
/// them. Scaling with the frame keeps small frames testable while requiring
/// real evidence on real viewports.
const NOMATCH_RECOVERY_MIN_STABLE_ROWS: u32 = 8;
const NOMATCH_RECOVERY_MIN_STABLE_FRACTION_DIVISOR: u32 = 20;
const NOMATCH_RECOVERY_MIN_DIRTY_ROWS: u32 = 4;
const NOMATCH_RECOVERY_SMALL_DIRTY_ROWS: u32 = 64;
const NOMATCH_RECOVERY_MAX_DIRTY_ROWS: u32 = 192;
const NOMATCH_RECOVERY_MAX_DIRTY_RATIO: f64 = 0.35;
const NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF: f64 = 8.0;
const NOMATCH_RECOVERY_RAW_STABLE_MAX_PIXEL_DIFF: u32 = 24;
/// The stable prefix must actually look like the canvas.
///
/// Without this the recovery path accepts any position where a handful of rows
/// happen to agree: it is a `NoMatch` fallback, so it ran *after* every matcher
/// gate had already refused, and a permissive gate here silently undid them.
/// Measured: a 682-row jump on real content was accepted at a 212-row offset
/// with a stable-prefix score of 10.7 against `MATCH_LINE_MAX_AVERAGE_DIFF`
/// of 3.0.
const NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF: f64 = MATCH_LINE_MAX_AVERAGE_DIFF;
/// Below this engine score the frame already agrees with the canvas, so the
/// dirty-edge recovery has nothing to repair and must not pay for its scan.
const NOMATCH_RECOVERY_CLEAN_MATCH_SCORE: f64 = 0.02;

/// Consecutive refusals before the failure is reported as `NoMatch`.
///
/// One unplaceable frame is not evidence that the content stopped overlapping.
/// The border turns red only once the failure has repeated this many times in a
/// row, which at 30 fps is about a quarter of a second — long enough to ride out
/// a rendering seam, short enough that a real jump is still reported promptly.
const REFUSAL_PATIENCE: u32 = 8;

#[cfg(feature = "stitch-legacy")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementSource {
    Canvas,
    Perceptual,
    PreviousFrameFallback,
}

#[cfg(feature = "stitch-legacy")]
#[derive(Debug, Clone, Copy, PartialEq)]
struct FramePlacement {
    match_info: FrameMatch,
    source: PlacementSource,
    fast_motion_trace: Option<FastMotionTrace>,
}

#[cfg(feature = "stitch-legacy")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PlacementProfile {
    path: Option<StitchDecisionPath>,
    canvas_match: Duration,
    perceptual_prepare: Duration,
    perceptual_match: Duration,
    fallback_match: Duration,
}

#[cfg(feature = "stitch-legacy")]
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

#[cfg(feature = "stitch-legacy")]
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

#[cfg(feature = "stitch-legacy")]
#[derive(Debug, Default, Clone)]
struct DefaultPlacementPipeline;

#[cfg(feature = "stitch-legacy")]
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

#[cfg(feature = "stitch-legacy")]
fn placement_fast_motion_trace(
    candidate: Option<FastMotionCandidate>,
    match_info: Option<FrameMatch>,
    fallback: bool,
    verify_pass: Option<FastMotionVerifyPass>,
) -> Option<FastMotionTrace> {
    (crate::trace::trace_fast_motion_enabled() || crate::trace::fast_motion_accept_enabled())
        .then(|| compare_fast_motion_candidate(candidate, match_info, fallback, verify_pass))
}

#[cfg(feature = "stitch-legacy")]
fn placement_source_label(source: PlacementSource) -> &'static str {
    match source {
        PlacementSource::Canvas => "canvas",
        PlacementSource::Perceptual => "perceptual",
        PlacementSource::PreviousFrameFallback => "fallback",
    }
}

#[cfg(feature = "stitch-legacy")]
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

/// Builds the placement engine for this build.
///
/// Feature-gated selection is the point: `stitch-legacy` swaps in the previous
/// canvas-scan matcher so both can be measured on identical frames, and the
/// production build has no reference to the old code at all.
fn new_placement_engine() -> Box<dyn PlacementEngine> {
    #[cfg(feature = "stitch-legacy")]
    {
        Box::new(engine::LegacyPipeline)
    }
    #[cfg(not(feature = "stitch-legacy"))]
    {
        Box::new(WindowedEdgeMatcher::new())
    }
}

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
    last_screen_offset: Option<FrameScreenOffset>,
    /// The placement strategy. Swappable by design: the windowed edge matcher
    /// is the production one, the legacy canvas scan is feature gated, and the
    /// tests use a scripted mock.
    engine: Box<dyn PlacementEngine>,
    /// Why the last frame was refused, for the overlay and the trace.
    last_reject_reason: Option<RejectReason>,
    /// Consecutive refusals, reset by any accepted frame.
    ///
    /// Drives the difference between `Pending` (a single unplaceable frame,
    /// which must not turn the border red) and `NoMatch` (a sustained failure
    /// the user needs to see).
    consecutive_refusals: u32,
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
            last_screen_offset: None,
            engine: new_placement_engine(),
            last_reject_reason: None,
            consecutive_refusals: 0,
        }
    }

    #[allow(dead_code)]
    pub fn push_frame(
        &mut self,
        frame: RgbFrame,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_with_analysis(frame.clone(), frame, direction)
    }

    #[allow(dead_code)]
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn push_frame_views(
        &mut self,
        compose_frame: ImageRgbView<'_>,
        analysis_frame: ImageRgbView<'_>,
        direction: Option<LongDirection>,
    ) -> Result<PushResult, Box<dyn Error>> {
        self.push_frame_sources(&compose_frame, &analysis_frame, direction)
    }

    /// Pushes a frame tagged with the on-screen offset of its capture region.
    ///
    /// While the offset matches the previously pushed frame (and the frame
    /// keeps the active geometry), frames flow through the regular matcher
    /// pipeline. When the offset changes — the selection box was resized in
    /// edit mode — the frame is stitched at the deterministic position implied
    /// by the offset delta: the still-overlapping part must show the same
    /// content (edit mode blocks scrolling), so the frame extends the canvas
    /// by exactly the resized strips and the canvas may become ragged
    /// (T/L-shaped), with never-captured corners left transparent.
    pub fn push_frame_with_offset<C, A>(
        &mut self,
        compose_frame: &C,
        analysis_frame: &A,
        direction: Option<LongDirection>,
        screen_offset: FrameScreenOffset,
    ) -> Result<PushResult, Box<dyn Error>>
    where
        C: RgbSource,
        A: RgbSource,
    {
        validate_rgb_source(compose_frame)?;
        validate_rgb_source(analysis_frame)?;
        let geometry_changed = match (self.stitched.as_ref(), self.last_screen_offset) {
            (None, _) => false,
            (Some(_), last_offset) => {
                Some(screen_offset) != last_offset
                    || compose_frame.width() != self.active_crop.map_or(0, |crop| crop.width)
                    || compose_frame.height() != self.active_crop.map_or(0, |crop| crop.height)
            }
        };
        if !geometry_changed {
            let result = self.push_frame_sources(compose_frame, analysis_frame, direction)?;
            if matches!(result, PushResult::Initialized) {
                self.last_screen_offset = Some(screen_offset);
            }
            return Ok(result);
        }
        self.push_geometry_frame(compose_frame, analysis_frame, screen_offset)
    }

    fn push_geometry_frame(
        &mut self,
        compose_frame: &impl RgbSource,
        analysis_frame: &impl RgbSource,
        new_offset: FrameScreenOffset,
    ) -> Result<PushResult, Box<dyn Error>> {
        let frame = rgb_frame_from_source(compose_frame)?;
        let previous_rect = self.viewport_rect.ok_or("missing viewport rect")?;
        let last_offset = self.last_screen_offset.ok_or("missing last screen offset")?;
        let crop = self.active_crop.ok_or("missing active crop")?;
        // The previous viewport anchors the crop's top row on screen; the new
        // frame is consumed whole (crop resets), anchored at its own top row.
        let delta_x = new_offset.x - last_offset.x;
        let delta_y = new_offset.y - (last_offset.y + crop.y as i32);
        let frame_x = previous_rect
            .x
            .checked_add(delta_x)
            .ok_or("geometry frame x overflow")?;
        let frame_y = previous_rect
            .y
            .checked_add(delta_y)
            .ok_or("geometry frame y overflow")?;

        let stitched = self.stitched.as_ref().ok_or("missing stitched frame")?;
        let overlap = canvas_overlap_rect(stitched, &frame, frame_x, frame_y);
        let (overlap_len, score) = match overlap {
            Some((x0, y0, x1, y1)) => {
                let width = x1.saturating_sub(x0);
                let height = y1.saturating_sub(y0);
                let canvas_region = extract_stitched_region(
                    stitched,
                    ViewportRect { x: x0 as i32, y: y0 as i32, width, height },
                )?;
                let frame_region = crop_rgb_frame(
                    &frame,
                    ComposeCrop {
                        x: (x0 as i32 - frame_x) as u32,
                        y: (y0 as i32 - frame_y) as u32,
                        width,
                        height,
                    },
                )?;
                (height.min(width), average_difference_same(&canvas_region, &frame_region))
            }
            None => (0, 0.0),
        };
        let direction = if delta_y < 0 || (delta_y == 0 && delta_x == 0) {
            AppendDirection::Top
        } else if delta_y > 0 {
            AppendDirection::Bottom
        } else if delta_x > 0 {
            AppendDirection::Right
        } else {
            AppendDirection::Left
        };
        let match_info = FrameMatch { direction, overlap: overlap_len, delta_x, delta_y, score };

        let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
        let new_rect = append_frame_at_position(stitched, previous_rect, &frame, match_info)?;
        self.viewport_rect = Some(new_rect);
        let bands = FixedBands::default();
        let new_crop = bands
            .active_crop(frame.width, frame.height)
            .ok_or("invalid active crop after geometry change")?;
        self.active_crop = Some(new_crop);
        self.fixed_detector = FixedBandDetector::default();
        self.previous_compose_frame = Some(frame);
        self.previous_perceptual_frame = Some(PerceptualFrame::from_source(analysis_frame));
        self.previous_perceptual_delta = None;
        self.accepted_direction = AcceptedDirectionMemory::default();
        self.last_screen_offset = Some(new_offset);
        // The canvas geometry changed underneath the engine; re-seed it from
        // the new viewport instead of letting it search a stale ladder.
        self.engine.reset();
        self.engine.commit(
            new_rect,
            new_rect.y,
            &CroppedRgbSource::new(
                &self.previous_compose_frame.clone().ok_or("missing compose frame")?,
                new_crop,
            )?,
        );
        self.last_profile_breakdown.path = Some(StitchDecisionPath::CanvasAccepted);
        Ok(PushResult::Accepted { match_info })
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
            let viewport_rect = ViewportRect {
                x: 0,
                y: 0,
                width: active_crop.width,
                height: active_crop.height,
            };
            self.engine.reset();
            self.engine
                .commit(viewport_rect, 0, &CroppedRgbSource::new(&compose_frame, active_crop)?);
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

        // Horizontal scrolling is a different axis with its own unmeasured
        // behaviour, so it keeps the previous canvas-scan matcher. The windowed
        // edge matcher is vertical-only by construction: its descriptor is the
        // derivative along the scroll axis, which for a horizontal scroll would
        // have to be the column derivative instead.
        let horizontal = matches!(
            search_direction,
            Some(SearchDirection::Left | SearchDirection::Right)
        );
        let decision = if horizontal {
            self.legacy_horizontal_placement(previous_rect, &active_compose, search_direction)
        } else {
            self.engine.place(&PlacementRequest {
                stitched: self.stitched.as_ref().ok_or("missing stitched frame")?,
                previous_rect,
                frame: &active_compose,
                search_direction,
            })
        };

        // A refusal is not the end of the pipeline: the stale-edge recovery
        // below handles a canvas edge that changed underneath us (a refreshed
        // footer, a late-loading image), which the matcher cannot see because
        // the canvas itself is wrong there. It only runs when the engine has
        // nothing, and it validates the overlap it writes.
        let (candidate_rect, match_info) = match decision {
            EngineDecision::Place { rect, match_info } => (rect, match_info),
            EngineDecision::Reject { reason } => {
                self.last_reject_reason = Some(reason);
                if crate::trace::trace_verbose_enabled() {
                    crate::trace_log!("stitch: engine refused reason={}", reason.as_str());
                }
                // No stale-edge recovery here on purpose.
                //
                // That path was written for the old matcher, whose failure
                // mode was "found no candidate at all": it then searched the
                // canvas for a row-prefix match and rewrote a dirty strip.
                // The windowed edge matcher's refusal is a *decision* — the
                // runner-up was too close, or there was no evidence — and
                // running a second, looser matcher on top of it silently undoes
                // the gate. Measured on the 480-frame corpus: enabling it here
                // turns 2 silently misplaced frames into 29, all of them on
                // repeated content, because row-prefix similarity reports
                // 70-250 "stable" rows at offsets hundreds of rows wrong.
                //
                // The dirty-edge case is still handled, from the accept path:
                // `find_dirty_overlap_recovery` runs when the engine *did*
                // produce a candidate and the canvas edge needs rewriting.
                return self.reject_frame(reason);
            }
        };

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
            self.commit_accepted(
                new_rect,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                recovery.match_info.direction,
            );
            self.engine
                .commit(new_rect, candidate_rect.y, &active_compose_frame);
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
            let applied = self.viewport_rect.ok_or("missing viewport rect after apply")?;
            self.commit_accepted(
                applied,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                match_info.direction,
            );
            self.engine
                .commit(applied, candidate_rect.y, &active_compose_frame);
            return Ok(PushResult::Accepted { match_info });
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
            self.commit_accepted(
                new_rect,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                recovery.match_info.direction,
            );
            self.engine
                .commit(new_rect, candidate_rect.y, &active_compose_frame);
            return Ok(PushResult::Accepted {
                match_info: recovery.match_info,
            });
        }

        self.last_reject_reason = Some(RejectReason::NoCandidate);
        self.reject_frame(RejectReason::NoCandidate)
    }

    /// Refuses the frame without touching any accepted state.
    ///
    /// The single reject exit: `viewport_rect`, both reference frames, the
    /// direction memory and the delta prediction all stay where the last
    /// *verified* frame left them, so a user who scrolls back recovers instead
    /// of the canvas silently skipping content.
    ///
    /// A refusal is reported as [`PushResult::Pending`] until it has repeated
    /// `REFUSAL_PATIENCE` times in a row. One unplaceable frame is not
    /// evidence that the content stopped overlapping — a frame can land on a
    /// rendering seam and be genuinely ambiguous while the next one matches
    /// cleanly — so the border only turns red once the failure is sustained.
    fn reject_frame(&mut self, _reason: RejectReason) -> Result<PushResult, Box<dyn Error>> {
        self.last_profile_breakdown.path = Some(StitchDecisionPath::NoMatch);
        self.consecutive_refusals = self.consecutive_refusals.saturating_add(1);
        if self.consecutive_refusals < REFUSAL_PATIENCE {
            crate::trace_log!(
                "stitch: refusal {} of {} ({}), still pending",
                self.consecutive_refusals,
                REFUSAL_PATIENCE,
                self.last_reject_reason
                    .map(|reason| reason.as_str())
                    .unwrap_or("unknown")
            );
            return Ok(PushResult::Pending);
        }
        Ok(PushResult::NoMatch)
    }

    /// Horizontal placement still uses the previous canvas-scan matcher.
    ///
    /// Kept separate from `engine` on purpose: the windowed edge matcher is
    /// built around the derivative along the scroll axis, and the horizontal
    /// case has not been measured. Silently treating a horizontal request as
    /// vertical would be a regression dressed up as a simplification.
    #[cfg(not(feature = "stitch-legacy"))]
    fn legacy_horizontal_placement(
        &mut self,
        previous_rect: ViewportRect,
        frame: &impl RgbSource,
        search_direction: Option<SearchDirection>,
    ) -> EngineDecision {
        match find_canvas_placement_match(
            match self.stitched.as_ref() {
                Some(stitched) => stitched,
                None => return EngineDecision::Reject { reason: RejectReason::Uninitialised },
            },
            previous_rect,
            frame,
            SearchAxis::Horizontal,
            search_direction,
        ) {
            Some(match_info) => EngineDecision::Place {
                rect: ViewportRect {
                    x: previous_rect.x + match_info.delta_x,
                    y: previous_rect.y + match_info.delta_y,
                    width: previous_rect.width,
                    height: previous_rect.height,
                },
                match_info,
            },
            None => EngineDecision::Reject { reason: RejectReason::NoCandidate },
        }
    }

    #[cfg(feature = "stitch-legacy")]
    fn legacy_horizontal_placement(
        &mut self,
        previous_rect: ViewportRect,
        frame: &impl RgbSource,
        search_direction: Option<SearchDirection>,
    ) -> EngineDecision {
        self.legacy_placement(previous_rect, frame, search_direction)
    }

    #[cfg(feature = "stitch-legacy")]
    fn legacy_placement(
        &mut self,
        previous_rect: ViewportRect,
        frame: &impl RgbSource,
        search_direction: Option<SearchDirection>,
    ) -> EngineDecision {
        let axis = canvas_search_axis(search_direction);
        match find_canvas_placement_match(
            match self.stitched.as_ref() {
                Some(stitched) => stitched,
                None => return EngineDecision::Reject { reason: RejectReason::Uninitialised },
            },
            previous_rect,
            frame,
            axis,
            search_direction,
        ) {
            Some(match_info) => EngineDecision::Place {
                rect: ViewportRect {
                    x: previous_rect.x + match_info.delta_x,
                    y: previous_rect.y + match_info.delta_y,
                    width: previous_rect.width,
                    height: previous_rect.height,
                },
                match_info,
            },
            None => EngineDecision::Reject { reason: RejectReason::NoCandidate },
        }
    }

    /// The only place that advances the "where the last accepted frame is"
    /// state. `viewport_rect`, both reference frames, the direction memory and
    /// the trace slots move together or not at all.
    ///
    /// Every accept path calls this; the reject path calls nothing. That is the
    /// invariant from `found.md` lesson 14: a frame whose position could not be
    /// verified must not become the reference for the next frame's delta,
    /// because that delta would be applied to a `viewport_rect` that never
    /// moved.
    #[allow(clippy::too_many_arguments)]
    fn commit_accepted(
        &mut self,
        new_rect: ViewportRect,
        compose_frame: RgbFrame,
        analysis_frame: &impl RgbSource,
        path: Option<StitchDecisionPath>,
        fast_motion_trace: Option<FastMotionTrace>,
        direction: AppendDirection,
    ) {
        self.viewport_rect = Some(new_rect);
        self.previous_perceptual_frame = Some(PerceptualFrame::from_source(analysis_frame));
        self.previous_compose_frame = Some(compose_frame);
        self.last_profile_breakdown.path = path;
        self.last_fast_motion_trace = fast_motion_trace;
        self.accepted_direction.record(direction);
        self.consecutive_refusals = 0;
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
        let recovery = match direction {
            AppendDirection::Bottom => recover_stale_bottom_edge(stitched, previous_rect, frame),
            AppendDirection::Top => recover_stale_top_edge(stitched, previous_rect, frame),
            AppendDirection::Right | AppendDirection::Left => None,
        }?;
        // Final gate on the recovery path.
        //
        // The recovery exists for one situation: the canvas edge changed
        // underneath us (a refreshed footer, a late-loading image) while the
        // rest of the frame still matches. It runs only after every matcher
        // gate has already refused, so it must not be able to place a frame on
        // weak evidence — that would silently undo those gates.
        //
        // Measured: without this, `burst`-style jumps were accepted at offsets
        // hundreds of rows off, with stable-prefix scores of 6.8-12.0 against
        // `MATCH_LINE_MAX_AVERAGE_DIFF` of 3.0.
        // The recovery exists for one situation: the canvas edge changed
        // underneath us (a refreshed footer, a late-loading image) while the
        // rest of the frame still matches. It runs only after every matcher
        // gate has already refused, so it must not be able to place a frame on
        // weak evidence — that would silently undo those gates.
        //
        // The gate is the *proven* overlap, not the average difference: a
        // refreshed strip legitimately scores high on the dirty rows, so a
        // difference threshold here would reject the very case the path exists
        // for. What it may not do is place a frame off a handful of rows.
        // Scope, not a threshold.
        //
        // The stale-edge recovery exists for one situation: the canvas edge
        // changed underneath us (a refreshed footer, a late-loading image)
        // while the rest of the frame still matches, so the displacement is
        // small. It was written for the old matcher, whose failure mode was
        // "no candidate at all".
        //
        // Under the windowed edge matcher it is a *bypass*: the engine has
        // already refused the frame on its own evidence, and this path then
        // places it using row-prefix similarity, which on repeated content
        // (cards, tables, code) reports 70-250 "stable" rows at offsets
        // hundreds of rows wrong. Measured: 11 silently misplaced frames per
        // 480 with no gate, 29 with a stable-row threshold.
        //
        // So the recovery is confined to the displacement it was designed for:
        // at most one viewport of travel. Anything larger is a jump, and a jump
        // must stay refused so the user can scroll back.
        let displacement = recovery.match_info.delta_y.unsigned_abs();
        let max_displacement = frame.height.max(1);
        if displacement > max_displacement {
            return None;
        }
        Some(recovery)
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
        // Cheap preconditions, in order of cost.
        //
        // The recovery exists for a canvas edge that changed underneath us: a
        // refreshed footer, a late-loading image. Two things follow, and both
        // are cheap to test before paying for the per-row RGB scan:
        //
        //  1. the frame has to reach the edge, otherwise there is no edge to
        //     rewrite;
        //  2. the engine's own match has to be *imperfect*, otherwise the edge
        //     agreed with the frame and nothing needs repairing.
        //
        // Measured: the scan costs ~4.3 ms/frame (43% of the frame budget at
        // 10 ms) and returned `None` on every frame of a clean 20-frame scroll,
        // which is exactly what (2) predicts.
        let reaches_edge = match match_info.direction {
            AppendDirection::Bottom => {
                let bottom = previous_rect
                    .y
                    .saturating_add(match_info.delta_y)
                    .saturating_add(i32::try_from(frame.height).unwrap_or(i32::MAX));
                bottom >= i32::try_from(stitched.height).unwrap_or(i32::MAX)
            }
            AppendDirection::Top => {
                let top = previous_rect.y.saturating_add(match_info.delta_y);
                top <= 0
            }
            AppendDirection::Right | AppendDirection::Left => false,
        };
        if !reaches_edge {
            return None;
        }
        // A clean match means the edge already agrees with the frame.
        if match_info.score <= NOMATCH_RECOVERY_CLEAN_MATCH_SCORE {
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
                if recovery.score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
                    return None;
                }
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

    #[cfg(feature = "stitch-legacy")]
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

    #[allow(dead_code)]
    pub fn last_duplicate_difference(&self) -> Option<f64> {
        self.last_duplicate_difference
    }

    #[allow(dead_code)]
    pub fn last_duplicate_analysis_difference(&self) -> Option<f64> {
        self.last_duplicate_analysis_difference
    }

    #[allow(dead_code)]
    pub fn last_duplicate_analysis_motion(&self) -> Option<DuplicateAnalysisMotion> {
        self.last_duplicate_analysis_motion
    }

    #[allow(dead_code)]
    pub fn last_fast_motion_trace(&self) -> Option<FastMotionTrace> {
        self.last_fast_motion_trace
    }

    #[allow(dead_code)]
    pub fn last_profile_breakdown(&self) -> StitchProfileBreakdown {
        self.last_profile_breakdown
    }

    /// Why the last frame was refused, for the overlay and the trace.
    #[allow(dead_code)]
    pub fn last_reject_reason(&self) -> Option<RejectReason> {
        self.last_reject_reason
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

#[allow(dead_code)]
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
    if candidate.score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
        return None;
    }
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
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, stable_rows + overwrite_rows, frame.height) {
        return None;
    }
    let dirty_score =
        average_top_dirty_score(stitched, frame, frame_y_u32, stable_rows, overwrite_rows)?;
    if dirty_score < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF {
        return None;
    }
    let score = average_top_row_score(stitched, frame, frame_y_u32, stable_rows)?;
    // The stable prefix is the only evidence this position is real. If it does
    // not look like the canvas, the recovery is a guess dressed up as a match.
    if score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
        return None;
    }
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
        if !dirty_canvas_recovery_shape_for_rows(stable_rows, existing_overlap, frame.height)
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
    if score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
        return None;
    }
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
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, existing_overlap, frame.height)
        || bottom_dirty_average_difference(stitched, frame, existing_overlap, stable_rows)
            < NOMATCH_RECOVERY_DIRTY_MIN_AVERAGE_DIFF
    {
        return None;
    }
    let score = average_bottom_row_score(stitched, frame, existing_overlap, stable_rows)?;
    if score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
        return None;
    }
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
    if !dirty_canvas_recovery_shape_for_rows(stable_rows, stable_rows + overwrite_rows, frame.height) {
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
    if score > NOMATCH_RECOVERY_STABLE_MAX_AVERAGE_DIFF {
        return None;
    }
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

/// `frame_rows` is the frame height: the evidence a placement needs scales with
/// how much of the frame the canvas can actually corroborate, not with how many
/// rows the dirty-shape test happened to accept.
fn dirty_canvas_recovery_shape_for_rows(
    stable_rows: u32,
    proven_rows: u32,
    frame_rows: u32,
) -> bool {
    let required = NOMATCH_RECOVERY_MIN_STABLE_ROWS
        .max(frame_rows.div_ceil(NOMATCH_RECOVERY_MIN_STABLE_FRACTION_DIVISOR));
    if stable_rows < required || stable_rows >= proven_rows {
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

    /// Drives `stitcher` with the same frame until the refusal is sustained.
    ///
    /// A single unplaceable frame is reported as `Pending`, not `NoMatch`: the
    /// border must not turn red for one ambiguous frame. Tests that care about
    /// the refusal contract push the same frame `REFUSAL_PATIENCE` times and
    /// assert the last result.
    fn push_until_refused(
        stitcher: &mut RawStitcher,
        frame: &RgbFrame,
        direction: Option<LongDirection>,
    ) -> PushResult {
        let mut result = PushResult::Pending;
        for _ in 0..REFUSAL_PATIENCE {
            result = stitcher.push_frame(frame.clone(), direction).unwrap();
        }
        result
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
            panic!("expected acceptance despite the damaged overlap, got {result:?}");
        };
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(match_info.delta_y, 12);
        assert_eq!(match_info.overlap, 36);
        // The score used to be a raw pixel difference, so asserting it was
        // above MATCH_LINE_MAX_AVERAGE_DIFF proved "the raw path could not have
        // found this". The engine scores boundary positions instead, so the
        // equivalent proof is that the raw pixel path still reports no overlap
        // (asserted above) while the frame is nevertheless placed correctly.
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
        let result = push_until_refused(&mut stitcher, &current_compose, Some(LongDirection::Down));
        let _ = current_analysis;

        assert_eq!(result, PushResult::NoMatch);
    }

    // The stale-edge recovery used to be reachable only after the matcher
    // returned "no candidate at all". Under the windowed edge matcher that
    // path is gone on purpose (see the comment in `push_frame_sources`): it
    // was a second, looser matcher that silently undid the gates.
    //
    // What must still work is the case it existed for — the canvas edge is
    // stale while the rest of the frame matches — and that is now handled from
    // the accept path by `find_dirty_overlap_recovery`. These two tests pin
    // that contract from both sides.

    /// A dirty strip at the canvas edge is rewritten once the overlap is proven.
    #[test]
    fn dirty_bottom_edge_is_repaired_when_the_frame_still_matches() {
        let width = 128;
        let first = frame_from_world_rows(width, &(0..160).collect::<Vec<_>>());
        let mut stitched_next = frame_from_world_rows(width, &(40..200).collect::<Vec<_>>());
        // The canvas bottom 20 rows are stale (a refreshed footer).
        for y in 140..160 {
            paint_full_row(&mut stitched_next, y, 170u8.wrapping_add(y as u8));
        }
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        let result = stitcher
            .push_frame(stitched_next, Some(LongDirection::Down))
            .unwrap();
        assert!(
            matches!(result, PushResult::Accepted { .. }),
            "a 40-row shift with a stale 20-row tail must still be placed"
        );
        let stitched = stitcher.stitched.as_ref().unwrap();
        assert_eq!(stitched.current_origin_y, 40);
    }

    /// A frame that does not overlap the canvas at all is refused, and the
    /// canvas is left untouched — no recovery path may place it anyway.
    #[test]
    fn a_jump_past_the_canvas_is_refused_even_with_a_stale_edge() {
        let width = 128;
        let first = frame_from_world_rows(width, &(0..160).collect::<Vec<_>>());
        let mut stale = frame_from_world_rows(width, &(400..560).collect::<Vec<_>>());
        for y in 140..160 {
            paint_full_row(&mut stale, y, 200u8.wrapping_add(y as u8));
        }
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        let before = stitcher.stitched.as_ref().unwrap().clone();
        assert_eq!(
            push_until_refused(&mut stitcher, &stale, Some(LongDirection::Down)),
            PushResult::NoMatch
        );
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!(after.height, before.height, "a refused jump must not grow the canvas");
        assert_eq!(after.data, before.data, "a refused jump must not rewrite pixels");
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
            push_until_refused(&mut stitcher, &current, Some(LongDirection::Down)),
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
        // Realistic content: distinct rows with visible horizontal boundaries.
        // A 32px-wide synthetic frame has no boundary at all once the ladder
        // bins it, which would make this a test of the descriptor's resolution
        // rather than of the append geometry.
        let previous = frame_from_world_rows(64, &(0..80).collect::<Vec<_>>());
        let current = frame_from_world_rows(64, &(2..82).collect::<Vec<_>>());
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Down))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Down))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.height, 82);
        assert_eq!(stitched.current_origin_y, 2);
        assert_eq!(stitched.anchor_origin_y, 0);
    }

    #[test]
    fn append_expands_top() {
        let previous = frame_from_world_rows(64, &(2..82).collect::<Vec<_>>());
        let current = frame_from_world_rows(64, &(0..80).collect::<Vec<_>>());
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(previous, Some(LongDirection::Up))
            .unwrap();
        let result = stitcher
            .push_frame(current, Some(LongDirection::Up))
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.finish().unwrap();
        assert_eq!(stitched.height, 82);
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
        let world = frame_from_world_rows(128, &(0..300).collect::<Vec<_>>());
        let first = crop_frame(&world, 0, 100, 128, 80);
        let bottom = crop_frame(&world, 0, 108, 128, 80);
        let top = crop_frame(&world, 0, 92, 128, 80);
        let mut stitcher = RawStitcher::new();
        stitcher
            .push_frame(first, Some(LongDirection::Down))
            .unwrap();
        stitcher
            .push_frame(bottom, Some(LongDirection::Down))
            .unwrap();
        let bottom_pixel_before = stitched_pixel_rgb(stitcher.stitched.as_ref().unwrap(), 11, 87);

        let result = stitcher.push_frame(top, Some(LongDirection::Up)).unwrap();

        assert!(matches!(result, PushResult::Accepted { .. }));
        let stitched = stitcher.stitched.as_ref().unwrap();
        // Canvas spans page rows 92..188: the 80-row frame started at 100, the
        // second extended it to 188, and the third reached back to 92.
        assert_eq!(stitched.height, 96);
        assert_eq!(stitched.current_origin_y, 0);
        assert_eq!(stitched_pixel_rgb(stitched, 11, 0), world.pixel_rgb(11, 92));
        assert_eq!(stitched_pixel_rgb(stitched, 11, 95), bottom_pixel_before);
        assert_eq!(
            stitched_pixel_rgb(stitched, 11, 95),
            world.pixel_rgb(11, 187)
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

    // The three tests that used to assert the old pipeline's *shape*
    // (`candidates`, `PlacementSource`, lazy perceptual preparation) are
    // re-expressed below against the contracts they actually encoded. The new
    // engine returns one decision instead of a candidate list, so asserting the
    // list would have been asserting an implementation detail.

    /// Contract: a vertical shift is placed at the offset the content implies,
    /// and the decision is recorded as a canvas accept.
    #[test]
    fn engine_places_a_vertical_shift_at_the_content_offset() {
        let world = textured_frame(32, 140);
        let mut stitcher = RawStitcher::new();
        let first = crop_frame(&world, 0, 0, 32, 60);
        stitcher.push_frame(first, Some(LongDirection::Down)).unwrap();

        let target = crop_frame(&world, 0, 24, 32, 60);
        let result = stitcher
            .push_frame(target, Some(LongDirection::Down))
            .unwrap();
        let PushResult::Accepted { match_info } = result else {
            panic!("expected acceptance for a 24-row shift, got {result:?}");
        };
        assert_eq!(match_info.delta_y, 24);
        assert_eq!(match_info.direction, AppendDirection::Bottom);
        assert_eq!(stitcher.viewport_rect.unwrap().y, 24);
        assert_eq!(
            stitcher.last_profile_breakdown.path,
            Some(StitchDecisionPath::CanvasAccepted)
        );
    }

    /// Contract: placement never depends on a perceptual baseline. The old
    /// pipeline deferred the perceptual pass when the canvas matched first;
    /// the new engine has no perceptual pass at all, which is a stronger
    /// version of the same property.
    #[test]
    fn engine_places_without_any_perceptual_baseline() {
        let world = textured_frame(32, 140);
        let mut stitcher = RawStitcher::new();
        let first = crop_frame(&world, 0, 0, 32, 60);
        stitcher.push_frame(first, Some(LongDirection::Down)).unwrap();
        // No perceptual baseline is reachable from outside, but the assertion
        // that matters is that placement succeeds and costs no perceptual time.
        let target = crop_frame(&world, 0, 20, 32, 60);
        assert!(matches!(
            stitcher.push_frame(target, Some(LongDirection::Down)).unwrap(),
            PushResult::Accepted { .. }
        ));
        let profile = stitcher.last_profile_breakdown();
        assert_eq!(profile.perceptual_prepare, Duration::default());
        assert_eq!(profile.perceptual_match, Duration::default());
        assert_eq!(profile.fallback_match, Duration::default());
    }

    /// Contract: `place` is pure. It may not mutate the canvas, the caller's
    /// stitched frame, or any accepted state — only `commit` does.
    #[test]
    fn engine_place_is_pure_and_does_not_mutate_state() {
        let world = textured_frame(32, 140);
        let stitched = StitchedFrame::from_first_frame(&crop_frame(&world, 0, 0, 32, 60));
        let before = stitched.clone();
        let previous_rect = ViewportRect { x: 0, y: 0, width: 32, height: 60 };
        let target = crop_frame(&world, 0, 20, 32, 60);

        let mut engine = WindowedEdgeMatcher::new();
        engine.commit(previous_rect, 0, &crop_frame(&world, 0, 0, 32, 60));

        let request = PlacementRequest {
            stitched: &stitched,
            previous_rect,
            frame: &target,
            search_direction: Some(SearchDirection::Down),
        };
        let first = engine.place(&request);
        let second = engine.place(&request);
        assert_eq!(first, second, "place must be deterministic and side-effect free");
        assert_eq!(stitched, before, "place must not touch the stitched frame");
        assert!(matches!(first, EngineDecision::Place { .. }));
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
        assert_eq!(image.format, Format::Argb8888);
        assert_eq!(image.stride, 8);
        assert_eq!(image.data[0], previous.data[2]);
        assert_eq!(image.data[1], previous.data[1]);
        assert_eq!(image.data[2], previous.data[0]);
        assert_eq!(image.data[3], 255);
    }

    #[test]
    fn geometry_offset_accounts_for_fixed_top_band_anchor() {
        let mut stitcher = RawStitcher::new();
        // Rows carry their frame-local index so placements are distinguishable.
        let first = frame(4, 4, 90);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first, None, FrameScreenOffset { x: 0, y: 0 }),
            Ok(PushResult::Initialized)
        ));

        // Simulate a detected fixed top band: the active crop's top row sits
        // two screen rows below the region top (crop.y == 2).
        let crop = FixedBands { top: 2, bottom: 0 }.active_crop(4, 4).unwrap();
        stitcher.active_crop = Some(crop);

        // The selection box extends upward, so the new region's top row is two
        // rows above the previous offset: same crop-anchored screen content.
        let mut taller = frame(4, 6, 90);
        for y in 0..6 {
            for x in 0..4 {
                let offset = (y * taller.stride + x * 3) as usize;
                taller.data[offset..offset + 3].copy_from_slice(&[40 + y as u8; 3]);
            }
        }
        let taller_analysis = taller.clone();
        let result = stitcher
            .push_frame_with_offset(&taller, &taller_analysis, None, FrameScreenOffset { x: 0, y: -2 })
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        // delta_y = new_offset.y - (last_offset.y + crop.y) = -2 - 2 = -4, so
        // the 6-row frame tops out 4 canvas rows above the old viewport and
        // the canvas grows upward: old rows shift to 4..8, frame rows 0..6.
        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.height, 8);
        assert_eq!(stitched.current_origin_y, 0);
        // Canvas row 0 shows the frame's top row; the old content moved down
        // by exactly the anchor-corrected delta.
        assert_eq!(taller.data[0], stitched.data[0]);
        assert_eq!(first.data[0], stitched.data[(4 * stitched.stride) as usize]);
    }

    #[test]
    fn converts_uncovered_stitched_corners_to_transparent() {
        let previous = frame(4, 2, 10);
        let mut stitched = StitchedFrame::from_first_frame(&previous);
        // Simulate a ragged canvas: only the left half of each row is captured.
        stitched.row_coverage = vec![(0, 2), (0, 2)];
        let image = image_from_stitched_frame(&stitched).unwrap();
        // Covered pixel keeps full alpha; uncovered right half is transparent.
        assert_eq!(image.data[3], 255);
        assert_eq!(image.data[4 * 2 + 0], 0);
        assert_eq!(image.data[4 * 2 + 3], 0);
    }

    #[test]
    fn geometry_offset_extension_grows_canvas_and_preserves_overlap() {
        let mut stitcher = RawStitcher::new();
        // First frame: 4x4 grey.
        let first = frame(4, 4, 90);
        let first_analysis = frame(4, 4, 90);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first_analysis, None, FrameScreenOffset { x: 0, y: 0 }),
            Ok(PushResult::Initialized)
        ));

        // Selection box extended downward by 2 rows: same top-left, taller.
        let mut extended = frame(4, 6, 90);
        // The bottom strip carries new (brighter) static content.
        for y in 4..6 {
            for x in 0..4 {
                let offset = (y * extended.stride + x * 3) as usize;
                extended.data[offset..offset + 3].copy_from_slice(&[200, 200, 200]);
            }
        }
        let extended_analysis = extended.clone();
        let result = stitcher
            .push_frame_with_offset(&extended, &extended_analysis, None, FrameScreenOffset { x: 0, y: 0 })
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.width, 4);
        assert_eq!(stitched.height, 6);
        // Old overlap content is preserved (not overwritten by the frame).
        let image = image_from_stitched_frame(stitched).unwrap();
        assert_eq!(image.data[0], 90);
        // New strip captured below the previous content.
        let strip = &image.data[(4 * 4) * 4..];
        assert_eq!(&strip[0..3], &[200, 200, 200]);
        // Everything is covered after a pure bottom extension.
        assert!(stitched.row_coverage.iter().all(|&(s, e)| s == 0 && e == 4));
    }

    #[test]
    fn geometry_offset_widening_leaves_transparent_corner() {
        let mut stitcher = RawStitcher::new();
        let first = frame(4, 4, 90);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first, None, FrameScreenOffset { x: 100, y: 100 }),
            Ok(PushResult::Initialized)
        ));

        // Selection box widened to the right by 4 px on the same screen rows.
        let mut widened = frame(8, 4, 90);
        for y in 0..4 {
            for x in 4..8 {
                let offset = (y * widened.stride + x * 3) as usize;
                widened.data[offset..offset + 3].copy_from_slice(&[180, 180, 180]);
            }
        }
        let widened_analysis = widened.clone();
        let result = stitcher
            .push_frame_with_offset(&widened, &widened_analysis, None, FrameScreenOffset { x: 100, y: 100 })
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.width, 8);
        assert_eq!(stitched.height, 4);
        // All rows fully covered: the widened frame spans the whole width.
        assert!(stitched.row_coverage.iter().all(|&(s, e)| s == 0 && e == 8));
    }

    #[test]
    fn geometry_offset_shrink_then_normal_push_continues() {
        let mut stitcher = RawStitcher::new();
        let first = frame(6, 6, 90);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first, None, FrameScreenOffset { x: 0, y: 0 }),
            Ok(PushResult::Initialized)
        ));

        // Shrink: selection box pulled in from the right by 2 px.
        let shrunk = frame(4, 6, 90);
        let shrunk_analysis = shrunk.clone();
        let result = stitcher
            .push_frame_with_offset(&shrunk, &shrunk_analysis, None, FrameScreenOffset { x: 0, y: 0 })
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        // A later same-geometry frame goes through the normal pipeline again
        // (accepted as a duplicate because the content is unchanged).
        let later = frame(4, 6, 90);
        let result = stitcher
            .push_frame_with_offset(&later, &later, None, FrameScreenOffset { x: 0, y: 0 })
            .unwrap();
        assert!(matches!(result, PushResult::Duplicate));

        // Canvas keeps the original width; the shrunken rows stay covered.
        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.width, 6);
        assert!(stitched.row_coverage.iter().all(|&(s, e)| s == 0 && e == 6));
    }

    #[test]
    fn wide_narrow_wide_scroll_preserves_notches_and_fills_recaptured_strips() {
        let world = textured_frame(80, 320);
        let mut stitcher = RawStitcher::new();
        let wide = crop_frame(&world, 0, 0, 80, 80);
        let origin = FrameScreenOffset { x: 100, y: 100 };
        let narrow_origin = FrameScreenOffset { x: 120, y: 100 };
        assert!(matches!(stitcher.push_frame_with_offset(&wide, &wide, None, origin).unwrap(), PushResult::Initialized));
        let narrow = crop_frame(&world, 20, 0, 40, 80);
        assert!(matches!(stitcher.push_frame_with_offset(&narrow, &narrow, None, narrow_origin).unwrap(), PushResult::Accepted { .. }));
        for y in (20..=120).step_by(20) {
            let next = crop_frame(&world, 20, y, 40, 80);
            assert!(matches!(stitcher.push_frame_with_offset(&next, &next, Some(LongDirection::Down), narrow_origin).unwrap(), PushResult::Accepted { .. }));
        }
        let widened = crop_frame(&world, 0, 120, 80, 80);
        assert!(matches!(stitcher.push_frame_with_offset(&widened, &widened, None, origin).unwrap(), PushResult::Accepted { .. }));
        let stitched = stitcher.stitched().unwrap();
        assert_eq!((stitched.width, stitched.height), (80, 200));
        let image = image_from_stitched_frame(stitched).unwrap();
        for y in 0..200 {
            for x in 0..80 {
                let expected_covered = y < 80 || y >= 120 || (20..60).contains(&x);
                let offset = ((y * 80 + x) * 4) as usize;
                assert_eq!(image.data[offset + 3], if expected_covered { 255 } else { 0 }, "coverage at {x},{y}");
                if expected_covered {
                    let source = ((y * world.width + x) * 3) as usize;
                    assert_eq!(&image.data[offset..offset + 3], &[world.data[source + 2], world.data[source + 1], world.data[source]], "pixels at {x},{y}");
                }
            }
        }
        // Shrink and expand once more without scrolling: earlier pixels survive.
        let narrow = crop_frame(&world, 20, 120, 40, 80);
        stitcher.push_frame_with_offset(&narrow, &narrow, None, narrow_origin).unwrap();
        stitcher.push_frame_with_offset(&widened, &widened, None, origin).unwrap();
        assert_eq!(image_from_stitched_frame(stitcher.stitched().unwrap()).unwrap().data, image.data);
    }

    #[test]
    fn geometry_offset_movement_translates_the_frame() {
        let mut stitcher = RawStitcher::new();
        let first = frame(4, 4, 90);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first, None, FrameScreenOffset { x: 10, y: 10 }),
            Ok(PushResult::Initialized)
        ));

        // Selection moved left by 3 and up by 2 (screen), same size.
        let moved = frame(4, 4, 90);
        let moved_analysis = moved.clone();
        let result = stitcher
            .push_frame_with_offset(&moved, &moved_analysis, None, FrameScreenOffset { x: 7, y: 8 })
            .unwrap();
        assert!(matches!(result, PushResult::Accepted { .. }));

        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.width, 7);
        assert_eq!(stitched.height, 6);
        // The frame moved left/up, so it occupies the canvas top-left corner
        // while the original content shifted to x 3..7, y 2..6.
        assert_eq!(stitched.current_origin_x, 0);
        assert_eq!(stitched.current_origin_y, 0);
        assert_eq!(stitched.row_coverage[0], (0, 4));
        assert_eq!(stitched.row_coverage[5], (3, 7));
        // Uncovered top-right corner of the L stays transparent on output;
        // the bottom row is covered only where the old canvas sits (x 3..7).
        let image = image_from_stitched_frame(stitched).unwrap();
        assert_eq!(image.data[(0 * 7 + 6) * 4 + 3], 0);
        assert_eq!(image.data[(5 * 7 + 0) * 4 + 3], 0);
        assert_eq!(image.data[(5 * 7 + 3) * 4 + 3], 255);
    }

    // --- P0-1: a rejected frame must not become the next frame's reference ---

    /// Sequence from `docs/dms-stitch-comparison.md` §6.3: small steps, one jump
    /// that breaks overlap, then small steps again. Before the fix the frames
    /// after the jump were matched against the *rejected* frame and stitched at
    /// `previous_rect + 40`, silently dropping 320 page rows.
    #[test]
    fn rejected_frame_does_not_advance_the_reference_state() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            assert!(matches!(
                stitcher.push_frame(frame, None).unwrap(),
                PushResult::Initialized | PushResult::Accepted { .. }
            ));
        }
        let before = (
            stitcher.viewport_rect,
            stitcher.previous_compose_frame.as_ref().map(|f| f.data.clone()),
        );

        for top in [400u32, 440, 480] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            assert_eq!(
                push_until_refused(&mut stitcher, &frame, None),
                PushResult::NoMatch,
                "frame at page row {top} has no overlap with the canvas and must be refused"
            );
        }

        // Nothing about the accepted state may have moved.
        let after = (
            stitcher.viewport_rect,
            stitcher.previous_compose_frame.as_ref().map(|f| f.data.clone()),
        );
        assert_eq!(before.0, after.0, "viewport_rect moved on a rejected frame");
        assert_eq!(before.1, after.1, "compose reference moved on a rejected frame");

        // And the canvas must not have grown with unverified content.
        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.height, 280, "canvas grew with a rejected frame");
        assert_eq!(stitched.current_origin_y, 80);

        // Every canvas row still maps to its true page row.
        for y in (0..stitched.height).step_by(20) {
            let want = row_texture(width, y);
            let offset = (y * stitched.stride) as usize;
            assert_eq!(
                &stitched.data[offset..offset + width as usize * 3],
                &want[..],
                "canvas row {y} does not match page row {y}"
            );
        }
    }

    /// After a rejection the user scrolls back into the canvas; the stitcher
    /// must recover instead of staying stuck on the old anchor.
    #[test]
    fn recovers_after_scrolling_back_from_a_rejection() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            stitcher.push_frame(frame, None).unwrap();
        }
        for top in [400u32, 440, 480] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            assert_eq!(
                push_until_refused(&mut stitcher, &frame, None),
                PushResult::NoMatch
            );
        }

        // Scroll back so the frame overlaps the committed canvas (80..280).
        let frame = frame_from_world_rows(width, &(120..120 + height).collect::<Vec<_>>());
        let PushResult::Accepted { match_info } = stitcher.push_frame(frame, None).unwrap() else {
            panic!("expected recovery once content overlaps the canvas again");
        };
        assert_eq!(match_info.delta_y, 40);
        let stitched = stitcher.stitched().unwrap();
        assert_eq!(stitched.current_origin_y, 120);
        // Canvas extends only by the genuinely new 40 rows at the bottom.
        assert_eq!(stitched.height, 320);
        for y in (0..stitched.height).step_by(20) {
            let want = row_texture(width, y);
            let offset = (y * stitched.stride) as usize;
            assert_eq!(
                &stitched.data[offset..offset + width as usize * 3],
                &want[..],
                "canvas row {y} does not match page row {y} after recovery"
            );
        }
    }

    /// One unplaceable frame must not be reported as a failure.
    ///
    /// The border is driven straight from `PushResult`, so `Pending` is what
    /// keeps a single ambiguous frame from flashing red on content that plainly
    /// overlaps. `NoMatch` only arrives once the refusal has repeated.
    #[test]
    fn a_single_refusal_is_pending_and_a_sustained_one_is_no_match() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            stitcher.push_frame(frame, None).unwrap();
        }

        let jump = frame_from_world_rows(width, &(900..900 + height).collect::<Vec<_>>());
        for expected in 1..REFUSAL_PATIENCE {
            assert_eq!(
                stitcher.push_frame(jump.clone(), None).unwrap(),
                PushResult::Pending,
                "refusal {expected} of {REFUSAL_PATIENCE} must stay pending"
            );
        }
        assert_eq!(
            stitcher.push_frame(jump.clone(), None).unwrap(),
            PushResult::NoMatch,
            "a sustained refusal must be reported"
        );
        // The patience counter must not run past the threshold.
        assert_eq!(stitcher.consecutive_refusals, REFUSAL_PATIENCE);
    }

    /// An accepted frame clears the patience, so an isolated refusal between
    /// two good frames never accumulates into a red border.
    #[test]
    fn an_accepted_frame_clears_the_refusal_patience() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            stitcher.push_frame(frame, None).unwrap();
        }

        let jump = frame_from_world_rows(width, &(900..900 + height).collect::<Vec<_>>());
        for _ in 0..REFUSAL_PATIENCE - 1 {
            assert_eq!(
                stitcher.push_frame(jump.clone(), None).unwrap(),
                PushResult::Pending
            );
        }
        assert_eq!(stitcher.consecutive_refusals, REFUSAL_PATIENCE - 1);

        // Back into the canvas: accepted, so the next refusal starts at one.
        let back = frame_from_world_rows(width, &(120..120 + height).collect::<Vec<_>>());
        assert!(matches!(
            stitcher.push_frame(back, None).unwrap(),
            PushResult::Accepted { .. }
        ));
        assert_eq!(stitcher.consecutive_refusals, 0);

        assert_eq!(
            stitcher.push_frame(jump, None).unwrap(),
            PushResult::Pending,
            "patience must restart after an acceptance"
        );
    }

    /// A refusal must not disturb the direction memory either: the next
    /// accepted frame still gets the stale-edge recovery path it depends on.
    #[test]
    fn rejection_preserves_accepted_direction_memory() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            stitcher.push_frame(frame, None).unwrap();
        }
        let before = stitcher.accepted_direction;
        let frame = frame_from_world_rows(width, &(900..900 + height).collect::<Vec<_>>());
        assert_eq!(
            push_until_refused(&mut stitcher, &frame, None),
            PushResult::NoMatch
        );
        assert_eq!(stitcher.accepted_direction, before);
        assert_eq!(
            stitcher.accepted_direction.vertical_direction(),
            Some(AppendDirection::Bottom)
        );
    }
}
