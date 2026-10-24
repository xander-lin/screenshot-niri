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
#[cfg(any(test, feature = "stitch-legacy"))]
mod motion;
mod perceptual;
#[cfg(feature = "stitch-legacy")]
mod placement;
mod rgb;
mod store;
mod types;

use canvas::{
    append_frame_at_position, append_frame_at_position_overwriting_overlap_rows,
    canvas_overlap_rect, extract_stitched_region, translate_viewport_rect,
    viewport_rect_covered_by_stitched,
};
#[cfg(test)]
use fixed_bands::detect_fixed_bands;
use fixed_bands::{FixedBandDetector, FixedBandObservation};
#[cfg(any(test, feature = "stitch-legacy"))]
#[cfg(feature = "stitch-legacy")]
use hash::estimate_vertical_perceptual_hash_motion_from_frame;
#[cfg(any(test, feature = "stitch-legacy"))]
#[cfg(test)]
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
use placement::{canvas_search_axis, find_canvas_placement_match};
// `WindowedEdgeMatcher` is the engine in a production build and a test subject
// in a `stitch-legacy` one, where `new_placement_engine` returns `LegacyPipeline`
// instead.
#[cfg_attr(feature = "stitch-legacy", allow(unused_imports))]
use engine::{
    BinLayout, CanvasRegion, EngineDecision, PlacementEngine, PlacementRequest, PrebuiltMeans,
    WindowedEdgeMatcher,
};
use rgb::{
    average_difference_same_source, average_luminance_difference_source, crop_rgb_frame,
    luminance_range_source,
    pixel_difference_source, pixel_offset, rgb_frame_from_source,
    rgb_frame_from_source_reusing, rgb_frame_from_source_reusing_with, validate_rgb_frame,
    validate_rgb_source, CroppedRgbSource,
};

pub use canvas::{
    image_from_stitched_frame, image_from_stitched_window_reusing, CanvasWindow,
};
use canvas::crop_rgb_frame_if_needed;
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
    FrameScreenOffset, MotionEstimate, PushResult, RgbFrame, RowCoverage, SearchAxis,
    SearchDirection,
    StitchDecisionPath, StitchProfileBreakdown, StitchedFrame, ViewportRect,
};

const MATCH_LINE_MAX_AVERAGE_DIFF: f64 = 3.0;
const MATCH_LINE_SAMPLE_STEP: u32 = 4;
const MATCH_PREFILTER_MAX_AVERAGE_DIFF: f64 = 12.0;
const MATCH_PREFILTER_SAMPLE_STEP: u32 = 2;
const MATCH_MIN_OVERLAP_PIXELS: u32 = 16;
/// Score ceiling for the fast-motion scan.
///
/// Only the legacy pipeline (and the tests of its scan) score deltas this way;
/// the shipped matcher gates on its own constants.
#[cfg_attr(not(any(test, feature = "stitch-legacy")), allow(dead_code))]
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
pub(super) const REFUSAL_PATIENCE: u32 = 8;

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

    /// The axis of the last accepted step — the capture's impulse.
    ///
    /// `None` before any step has been placed: only then is the hint the sole
    /// information available about which way the viewport is going.
    fn axis(self) -> Option<SearchAxis> {
        match self.direction? {
            AppendDirection::Top | AppendDirection::Bottom => Some(SearchAxis::Vertical),
            AppendDirection::Left | AppendDirection::Right => Some(SearchAxis::Horizontal),
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

#[cfg(any(test, feature = "stitch-legacy"))]
use motion::scan_fast_vertical_motion;
#[cfg(test)]
use motion::{compare_fast_motion_candidates, score_fast_motion_delta};

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
        Box::new(WindowedEdgeMatcher::new(SearchAxis::Vertical))
    }
}

pub struct RawStitcher {
    stitched: Option<StitchedFrame>,
    viewport_rect: Option<ViewportRect>,
    active_crop: Option<ComposeCrop>,
    previous_compose_frame: Option<RgbFrame>,
    /// Allocation handed back by the frame that `previous_compose_frame` just
    /// replaced, waiting to be filled with the next one.
    ///
    /// The stitcher keeps exactly two compose frames alive — the current and the
    /// previous — so this makes that pair cycle instead of allocating 6.2 MB per
    /// push (1080p) and re-faulting every page of it. It is deliberately *not*
    /// `previous_compose_frame`: a refused frame must leave the reference state
    /// untouched, so the buffer has to be recoverable without consuming it.
    spare_compose_buffer: Vec<u8>,
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
    /// Canvas row of the last accepted placement, in the engine's logical
    /// coordinates. Diagnostic only: it is what the placement was verified
    /// against, independent of how the pixel canvas re-based itself.
    last_engine_top: Option<i32>,
    /// Logical row that pixel-canvas row 0 currently corresponds to.
    ///
    /// The pixel canvas grows upward by prepending rows, so a frame's logical
    /// row and its pixel row differ by this origin. Diagnostics need it to map
    /// one onto the other; without it every prepended frame looks misplaced.
    canvas_origin: i32,
    /// Consecutive refusals, reset by any accepted frame.
    ///
    /// Drives the difference between `Pending` (a single unplaceable frame,
    /// which must not turn the border red) and `NoMatch` (a sustained failure
    /// the user needs to see).
    consecutive_refusals: u32,
}

impl RawStitcher {
    /// Whether anything in this build will read a `PerceptualFrame`.
    ///
    /// The luminance image is 4 bytes per pixel — 8.3 MB on a 1080p frame — and
    /// it is rebuilt for every pushed frame. Two things read it: the legacy
    /// pipeline's perceptual stage (`stitch-legacy`) and the deep-profile
    /// trace. The shipped windowed-edge matcher reads its boundary descriptor
    /// instead, so in a default build the frame was built and dropped unread:
    /// measured at 18.7% of a 1080p frame's CPU and 8.3 MB of its allocation.
    fn perceptual_frame_needed() -> bool {
        // Deliberately *not* `cfg!(test)`: a test build that keeps the frame
        // alive would hide a new reader of `previous_perceptual_frame` from the
        // test suite, because tests would still get `Some` while the shipped
        // build got `None`. Every test runs the same branch the product runs.
        cfg!(feature = "stitch-legacy") || crate::trace::trace_deep_profile_enabled()
    }

    /// The perceptual frame for this push, or `None` when nothing reads it.
    fn perceptual_frame_for(analysis_frame: &impl RgbSource) -> Option<PerceptualFrame> {
        Self::perceptual_frame_needed().then(|| PerceptualFrame::from_source(analysis_frame))
    }

    pub fn new() -> Self {
        Self {
            stitched: None,
            viewport_rect: None,
            active_crop: None,
            previous_compose_frame: None,
            spare_compose_buffer: Vec::new(),
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
            last_engine_top: None,
            canvas_origin: 0,
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
        let recycled = std::mem::take(&mut self.spare_compose_buffer);
        let frame = rgb_frame_from_source_reusing(compose_frame, recycled)?;
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
        if let Some(replaced) = self.previous_compose_frame.replace(frame) {
            self.recycle_compose_frame(replaced);
        }
        self.previous_perceptual_frame = Self::perceptual_frame_for(analysis_frame);
        self.previous_perceptual_delta = None;
        // A geometry change is a *known* step, not a matched one: the box moved
        // by exactly `delta`, so the content moved with it. Recording it keeps
        // the axis pointing where the capture is really going — after a
        // horizontal nudge the next frame must be looked for horizontally even
        // though the arrow key still says "down".
        //
        // A resize (delta 0) says nothing about direction and clears the
        // memory instead, so the next frame falls back to the hint.
        if delta_x != 0 || delta_y != 0 {
            self.accepted_direction.record(match_info.direction);
        } else {
            self.accepted_direction = AcceptedDirectionMemory::default();
        }
        self.last_screen_offset = Some(new_offset);
        // A box edit does not invalidate what the matcher has already learned
        // unless it changed the *descriptor's* geometry. Every stored row is
        // 48 bins across the frame's width, so the rows stay comparable while
        // the width and the horizontal extent are unchanged — a height resize,
        // a vertical move. Resetting in those cases threw the whole canvas
        // away, and with it the anchor's only evidence: a box shrunk below the
        // scroll step then refused every frame for good, because the guard's
        // reach is one frame tall and the anchor had nothing left to match.
        //
        // A width change or a horizontal move re-cuts the bins (or compares
        // different columns), so only those re-seed.
        let layout_changed = new_crop.width != crop.width || delta_x != 0;
        if layout_changed {
            self.engine.reset();
        }
        // The engine's rows are the *crop's*, and its positions are logical
        // canvas rows, never the pixel canvas's re-based row: `new_rect.y` is
        // `frame_y - min_y` once the canvas has grown upward.
        let engine_top = frame_y + new_crop.y as i32;
        self.last_engine_top = Some(engine_top);
        self.canvas_origin = frame_y - new_rect.y;
        let previous = self
            .previous_compose_frame
            .as_ref()
            .ok_or("missing compose frame")?;
        self.engine
            .commit_known_step(new_rect, engine_top, &CroppedRgbSource::new(previous, new_crop)?);
        if layout_changed {
            // The rows that were invalidated are still in the pixel canvas:
            // rebuild the descriptor from those pixels instead of starting the
            // matcher over with one frame.
            if let Some(stitched) = self.stitched.as_ref() {
                self.engine.reseed_from_canvas(
                    stitched,
                    new_rect.x,
                    new_crop.width,
                    self.canvas_origin,
                );
            }
        }
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
            self.previous_perceptual_frame = Self::perceptual_frame_for(analysis_frame);
            self.last_profile_breakdown.path = Some(StitchDecisionPath::Initialized);
            return Ok(PushResult::Initialized);
        }

        let active_crop = self.active_crop.ok_or("missing active crop")?;
        let active_compose = CroppedRgbSource::new(compose_frame, active_crop)?;

        let previous_rect = self.viewport_rect.ok_or("missing viewport rect")?;
        // The near-duplicate check samples every fourth pixel in both axes, so
        // it does not need the previous viewport as a buffer: reading the canvas
        // through a view costs the samples and nothing else. Materialising the
        // region cost a 6.2 MB allocation and copy per push on 1080p — the
        // largest remaining per-frame allocation.
        let stitched = self.stitched.as_ref().ok_or("missing stitched frame")?;
        let previous = CanvasRegion::new(
            stitched,
            u32::try_from(previous_rect.x)?,
            u32::try_from(previous_rect.y)?,
            previous_rect.width,
            previous_rect.height,
        );
        let duplicate_started_at = profile_enabled.then(Instant::now);
        let duplicate_difference = average_difference_same_source(&previous, &active_compose);
        if let Some(started_at) = duplicate_started_at {
            self.last_profile_breakdown.duplicate_check += started_at.elapsed();
        }
        // "Identical" is relative to the contrast the content actually has. An
        // absolute threshold (mean diff <= 2.0) treats real 60 px scrolling on
        // a two-gray page (delta 2) as a duplicate — every moving frame is
        // dropped and the canvas never grows. Cap the threshold at the old
        // absolute value so normal-contrast content behaves exactly as before.
        let duplicate_threshold = (0.1 * luminance_range_source(&active_compose))
            .clamp(0.25, self.duplicate_threshold);
        if duplicate_difference <= duplicate_threshold {
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

        // Recycle the buffer of the frame we are about to replace. The stitcher
        // keeps exactly two frames alive, so this makes the pair of 6.2 MB
        // allocations cycle instead of being freed and re-faulted every push.
        let recycled = std::mem::take(&mut self.spare_compose_buffer);
        // The descriptor's bins are accumulated while the frame is converted.
        // Building the ladder from the finished frame read it again — another
        // 25 MB pass at 4K, over memory that had just left the cache — and the
        // conversion already has every pixel in hand.
        //
        // The ladder describes the *cropped* frame, so the fusion is only exact
        // when the active crop is the whole frame (the shipped configuration;
        // the fixed-band detector is trace-only). A real crop falls back to the
        // engine reading the cropped frame below.
        let fused_layout = (active_crop.x == 0
            && active_crop.y == 0
            && active_crop.width == compose_frame.width()
            && active_crop.height == compose_frame.height())
        .then(|| BinLayout::for_across(compose_frame.width()))
        .flatten();
        let mut fused_means = fused_layout
            .map(|layout| vec![0f32; compose_frame.height() as usize * layout.bins() as usize]);
        let compose_frame = rgb_frame_from_source_reusing_with(compose_frame, recycled, |y, row| {
            if let (Some(layout), Some(means)) = (fused_layout.as_ref(), fused_means.as_mut()) {
                let bins = layout.bins() as usize;
                let start = y as usize * bins;
                layout.means_into(row, &mut means[start..start + bins]);
            }
        })?;
        let prebuilt_means = match (fused_layout, fused_means.as_deref()) {
            (Some(layout), Some(means)) => Some(PrebuiltMeans {
                along: compose_frame.height(),
                layout,
                means,
            }),
            _ => None,
        };
        // The active crop is the whole frame in the shipped configuration (the
        // fixed-band detector is trace-only, so its top/bottom are zero), and
        // copying a whole 1080p frame to obtain an identical one cost 6.2 MB of
        // allocation per push. Crop only when the crop is not the whole frame.
        let cropped_compose = crop_rgb_frame_if_needed(&compose_frame, active_crop)?;
        let active_compose_frame: &RgbFrame = cropped_compose.as_ref().unwrap_or(&compose_frame);

        // The impulse decides which axis is asked first.
        //
        // The capture is moving one way until it isn't, and the last accepted
        // step is the only thing that knows which way that was — a released
        // arrow key can be stale, and the content can turn without a keypress.
        // The hint seeds the very first step, before any impulse exists.
        //
        // The two axes are not scored against each other. The impulse axis
        // answers first; if it cannot place the frame, the other axis is asked
        // once, restricted to the placements that still overlap the previous
        // frame. Whichever axis answers, wins; if neither does, the frame is
        // refused.
        let primary_axis = self.impulse_axis(search_direction);
        let mut placed_axis = primary_axis;
        let mut decision = self.place_on_axis(
            primary_axis,
            previous_rect,
            &active_compose,
            search_direction,
            true,
            prebuilt_means,
        );
        let primary_reason = match &decision {
            EngineDecision::Reject { reason } => Some(*reason),
            EngineDecision::Place { .. } => None,
        };
        // "Nothing moved" is not an answer on the impulse axis while the pixels
        // changed: the duplicate check has already established that this frame
        // differs from the previous one, so an axis that can only say "the same
        // place" has not explained anything. That is what a stale impulse looks
        // like at a turn — the axis the capture just left keeps answering zero,
        // because movement along the other axis barely disturbs its descriptor.
        // So a zero-delta answer asks the other axis too; it is kept only when
        // that axis has nothing better, which is the honest reading of content
        // that changed in place.
        let primary_is_zero = match &decision {
            EngineDecision::Place { match_info, .. } => {
                match_info.delta_x == 0 && match_info.delta_y == 0
            }
            EngineDecision::Reject { .. } => false,
        };
        if primary_reason.is_some() || primary_is_zero {
            let attempt = self.place_on_axis(
                primary_axis.other(),
                previous_rect,
                &active_compose,
                search_direction,
                false,
                prebuilt_means,
            );
            let improved = match &attempt {
                EngineDecision::Place { match_info, .. } => {
                    !primary_is_zero || match_info.delta_x != 0 || match_info.delta_y != 0
                }
                EngineDecision::Reject { .. } => false,
            };
            if improved {
                placed_axis = primary_axis.other();
                decision = attempt;
            }
        }
        let horizontal = placed_axis == SearchAxis::Horizontal;

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
                //
                // The refused frame still owns a 6.2 MB (1080p) conversion
                // buffer, and a refusal is not rare — `real-1` refuses 257 of
                // 461 frames. Hand it back before dropping the frame, or every
                // refusal pays a fresh allocation and a page fault per page.
                self.recycle_compose_frame(compose_frame);
                return self.reject_frame(reason);
            }
        };

        if let Some(recovery) =
            self.find_dirty_overlap_recovery(previous_rect, active_compose_frame, match_info)
        {
            let append_started_at = profile_enabled.then(Instant::now);
            let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
            let new_rect = append_frame_at_position_overwriting_overlap_rows(
                stitched,
                previous_rect,
                active_compose_frame,
                recovery.match_info,
                recovery.overwrite_frame_y,
                recovery.overwrite_rows,
            )?;
            if let Some(started_at) = append_started_at {
                self.last_profile_breakdown.append_frame += started_at.elapsed();
            }
            // The engine commit runs first: `active_compose_frame` may borrow
            // `compose_frame`, which `commit_accepted` takes ownership of.
            self.last_engine_top = Some(candidate_rect.y);
            self.canvas_origin = candidate_rect.y - new_rect.y;
            self.engine
                .commit(new_rect, candidate_rect.y, active_compose_frame);
            self.commit_accepted(
                new_rect,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                recovery.match_info.direction,
            );
            return Ok(PushResult::Accepted {
                match_info: recovery.match_info,
            });
        }

        if self.try_apply_match(
            previous_rect,
            active_compose_frame,
            match_info,
            profile_enabled,
        )? {
            let applied = self.viewport_rect.ok_or("missing viewport rect after apply")?;
            if horizontal {
                // The engine's canvas is one column of rows: every row's
                // descriptor was built from whichever column happened to
                // capture it first, and the engine has no x dimension to tell
                // the columns apart. A horizontal scroll changes the column, so
                // those rows are no longer comparable — keeping them would make
                // the next vertical step read the new column against the old
                // one and refuse. Re-seed from the frame the horizontal matcher
                // just verified against the pixel canvas.
                self.engine.reset();
                self.last_engine_top = Some(applied.y);
                self.canvas_origin = 0;
                self.engine.commit(applied, applied.y, active_compose_frame);
            } else {
                self.last_engine_top = Some(candidate_rect.y);
                self.canvas_origin = candidate_rect.y - applied.y;
                self.engine
                    .commit(applied, candidate_rect.y, active_compose_frame);
            }
            // After the engine commit: `active_compose_frame` may borrow
            // `compose_frame`, which `commit_accepted` takes ownership of.
            self.commit_accepted(
                applied,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                match_info.direction,
            );
            return Ok(PushResult::Accepted { match_info });
        }

        if let Some(recovery) = self.find_nomatch_recovery(previous_rect, active_compose_frame) {
            let append_started_at = profile_enabled.then(Instant::now);
            let stitched = self.stitched.as_mut().ok_or("missing stitched frame")?;
            let new_rect = append_frame_at_position_overwriting_overlap_rows(
                stitched,
                previous_rect,
                active_compose_frame,
                recovery.match_info,
                recovery.overwrite_frame_y,
                recovery.overwrite_rows,
            )?;
            if let Some(started_at) = append_started_at {
                self.last_profile_breakdown.append_frame += started_at.elapsed();
            }
            // The engine commit runs first: `active_compose_frame` may borrow
            // `compose_frame`, which `commit_accepted` takes ownership of.
            self.last_engine_top = Some(candidate_rect.y);
            self.canvas_origin = candidate_rect.y - new_rect.y;
            self.engine
                .commit(new_rect, candidate_rect.y, active_compose_frame);
            self.commit_accepted(
                new_rect,
                compose_frame,
                analysis_frame,
                Some(StitchDecisionPath::CanvasAccepted),
                None,
                recovery.match_info.direction,
            );
            return Ok(PushResult::Accepted {
                match_info: recovery.match_info,
            });
        }

        self.last_reject_reason = Some(RejectReason::NoCandidate);
        self.recycle_compose_frame(compose_frame);
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

    /// The axis the capture is moving along.
    fn impulse_axis(&self, search_direction: Option<SearchDirection>) -> SearchAxis {
        if let Some(axis) = self.accepted_direction.axis() {
            return axis;
        }
        match search_direction {
            Some(SearchDirection::Left | SearchDirection::Right) => SearchAxis::Horizontal,
            _ => SearchAxis::Vertical,
        }
    }

    /// Asks one axis for a placement.
    ///
    /// `primary` separates the two roles an axis can play. The impulse axis is
    /// the capture's own direction: both directions are ordinary (a scroll-back
    /// is a step like any other) and it may fall back to the whole-canvas anchor
    /// once the border is red. The other axis is only ever asked for the
    /// predicted step, in the one direction the user indicated, and has no
    /// anchor.
    fn place_on_axis(
        &mut self,
        axis: SearchAxis,
        previous_rect: ViewportRect,
        frame: &impl RgbSource,
        search_direction: Option<SearchDirection>,
        primary: bool,
        prebuilt_means: Option<PrebuiltMeans<'_>>,
    ) -> EngineDecision {
        // The hint is a sign, not an axis: passing "down" to the horizontal
        // matcher (or "left" to the vertical engine) would filter out every
        // candidate either of them has.
        let hint = match axis {
            SearchAxis::Vertical => search_direction
                .filter(|direction| matches!(direction, SearchDirection::Up | SearchDirection::Down)),
            SearchAxis::Horizontal => search_direction.filter(|direction| {
                matches!(direction, SearchDirection::Left | SearchDirection::Right)
            }),
        };
        match axis {
            SearchAxis::Vertical => match self.stitched.as_ref() {
                Some(stitched) => self.engine.place(&PlacementRequest {
                    stitched,
                    previous_rect,
                    frame,
                    search_direction: hint,
                    prebuilt_means,
                }),
                None => EngineDecision::Reject {
                    reason: RejectReason::Uninitialised,
                },
            },
            SearchAxis::Horizontal => {
                // The horizontal axis has no persistent descriptor: it is built
                // on demand from the pixels the capture already holds, for the
                // strip the comparison can use (see `place_horizontally`). Its
                // anchor window does not exist — recovery belongs to the axis
                // the capture is on — so `primary` has nothing to widen here.
                let Some(stitched) = self.stitched.as_ref() else {
                    return EngineDecision::Reject {
                        reason: RejectReason::Uninitialised,
                    };
                };
                // On the impulse axis both directions are ordinary. As a
                // fallback the direction has to come from the user: nothing
                // about the content says which side it moved to, so without it
                // there is no horizontal question to ask.
                let direction = if primary { None } else { hint };
                if !primary && direction.is_none() {
                    return EngineDecision::Reject {
                        reason: RejectReason::NoCandidate,
                    };
                }
                #[cfg(not(feature = "stitch-legacy"))]
                {
                    engine::place_horizontally(stitched, previous_rect, frame, direction)
                }
                #[cfg(feature = "stitch-legacy")]
                {
                    // The axis and the direction are the caller's, not the
                    // hint's: after a horizontal edit the hint can still say
                    // "down" while the capture is moving sideways, and the
                    // primary axis searches both ways.
                    self.legacy_horizontal_placement(stitched, previous_rect, frame, direction)
                }
            }
        }
    }

    #[cfg(feature = "stitch-legacy")]
    fn legacy_horizontal_placement(
        &self,
        stitched: &StitchedFrame,
        previous_rect: ViewportRect,
        frame: &impl RgbSource,
        search_direction: Option<SearchDirection>,
    ) -> EngineDecision {
        match find_canvas_placement_match(
            stitched,
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
            None => EngineDecision::Reject {
                reason: RejectReason::NoCandidate,
            },
        }
    }

    /// Hands a conversion buffer back to the recycler.
    ///
    /// Only the allocation is kept — never the contents — so this may be called
    /// on a frame that was refused, as well as on the one `commit_accepted`
    /// replaces. Both matter: accepting every frame is not the common case (the
    /// `real-1` corpus refuses 257 of 461), and a refusal used to drop its
    /// buffer on the floor, which put the whole 6.2 MB (1080p) allocation and
    /// its page faults back on the next push.
    fn recycle_compose_frame(&mut self, frame: RgbFrame) {
        self.spare_compose_buffer = frame.data;
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
        self.previous_perceptual_frame = Self::perceptual_frame_for(analysis_frame);
        if let Some(replaced) = self.previous_compose_frame.replace(compose_frame) {
            self.recycle_compose_frame(replaced);
        }
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
        // Inside the bounding box is not the same as already captured: on a
        // ragged canvas the frame can land in a never-captured region, and
        // skipping the append would drop its pixels while reporting success.
        if viewport_rect_covered_by_stitched(stitched, candidate_rect)? {
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

    /// Trace-only: logs when the perceptual motion estimate changes.
    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
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

    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
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

    /// Canvas row of the most recent accepted placement, in logical coordinates.
    /// The canvas row the engine last asked for.
    ///
    /// Read by `replay-frames` (a separate binary crate, so this is invisible to
    /// the lib build) and by tests.
    #[allow(dead_code)]
    pub fn last_engine_top(&self) -> Option<i32> {
        self.last_engine_top
    }

    /// Logical row that pixel-canvas row 0 corresponds to.
    #[allow(dead_code)]
    pub fn canvas_origin(&self) -> i32 {
        self.canvas_origin
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

    /// The descriptor the matcher built for the last frame, as a grayscale
    /// image `(width, height, data)`.
    ///
    /// A debug aid for offline analysis: dumping it next to the frame shows
    /// whether the matcher lost the evidence or merely decided against it.
    #[allow(dead_code)]
    pub fn last_descriptor(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.engine.last_descriptor()
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

/// Trace-only: describes how the fast-motion candidate compares with the eventual placement.
#[cfg_attr(not(any(feature = "stitch-legacy", feature = "trace-logs")), allow(dead_code))]
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

/// Trace-only: describes the candidate's delta against the reference.
#[cfg_attr(not(any(feature = "stitch-legacy", feature = "trace-logs")), allow(dead_code))]
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

/// Trace-only: the reference delta a fast-motion candidate is checked against.
#[cfg_attr(not(any(feature = "stitch-legacy", feature = "trace-logs")), allow(dead_code))]
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
    let Some(row) = stitched.store.with_row(stitched_y as i32, |row| row.to_vec()) else {
        return f64::MAX;
    };
    let mut total = 0u64;
    let mut samples = 0u32;
    for x in (0..width).step_by(MATCH_LINE_SAMPLE_STEP as usize) {
        let stitched_offset = x as usize * 3;
        let frame_offset = pixel_offset(frame, x, frame_y);
        total += (i32::from(row[stitched_offset]) - i32::from(frame.data[frame_offset]))
            .unsigned_abs() as u64;
        total += (i32::from(row[stitched_offset + 1])
            - i32::from(frame.data[frame_offset + 1]))
        .unsigned_abs() as u64;
        total += (i32::from(row[stitched_offset + 2])
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
        frame.pixel(x, y).expect("captured pixel")
    }

    /// The canvas's pixels as one packed buffer, for comparisons that used to
    /// read `stitched.data` directly.
    fn stitched_rgb(frame: &StitchedFrame) -> Vec<u8> {
        frame.to_rgb_buffer()
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
        assert_eq!(stitched_rgb(&stitched), frame.data);
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
            .push_frame_with_analysis(frame.clone(), previous_analysis.clone(), None)
            .unwrap();
        // The gate is off in a test build on purpose — tests run the branch the
        // shipped binary runs — so seed the baseline here. The invariant under
        // test is that the duplicate path leaves it alone.
        stitcher.previous_perceptual_frame = Some(PerceptualFrame::from_source(&previous_analysis));
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
        let before = stitcher.stitched.as_ref().unwrap().pixel_digest();
        let before_height = stitcher.stitched.as_ref().unwrap().height;
        assert_eq!(
            push_until_refused(&mut stitcher, &stale, Some(LongDirection::Down)),
            PushResult::NoMatch
        );
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!(after.height, before_height, "a refused jump must not grow the canvas");
        assert_eq!(after.pixel_digest(), before, "a refused jump must not rewrite pixels");
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
        let view_canvas = view_stitcher.finish().expect("view canvas");
        let rgb_canvas = rgb_stitcher.finish().expect("rgb canvas");
        assert_eq!(
            (view_canvas.width, view_canvas.height, view_canvas.stride),
            (rgb_canvas.width, rgb_canvas.height, rgb_canvas.stride)
        );
        assert_eq!(
            view_canvas.to_rgb_buffer(),
            rgb_canvas.to_rgb_buffer(),
            "the composed-view path must stitch exactly what the RGB path stitches"
        );
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
    fn fast_motion_scan_ranks_candidates_by_comparison_order() {
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
    fn fast_motion_scan_accepts_its_top_candidate() {
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
    fn fast_motion_scan_falls_back_when_top_candidates_miss() {
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
    fn fast_motion_scan_falls_back_when_partial_is_ambiguous() {
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

    /// The fixture needs structure *along* the horizontal axis. A frame whose
    /// rows are constant carries no `|dL/dx|` at all, so there is nothing to
    /// match a sideways shift against and refusing it is the honest answer —
    /// that is what this test used to rely on the old pixel scan to paper over.
    #[test]
    fn append_expands_right() {
        let previous = textured_frame(32, 24);
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
        let previous = textured_frame(32, 24);
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

    /// A sideways move needs a sideways hint.
    ///
    /// The hint names the direction of a fallback attempt, and nothing in the
    /// frame says which side it moved to. With the impulse and the hint both
    /// vertical there is no horizontal question to ask, so the frame is refused
    /// rather than guessed at — the same frame with a Right hint is
    /// `append_expands_right`. This is the rule the old
    /// `vertical_mode_rejects_horizontal_motion` protected; the mechanism is now
    /// the axis routing, so the reason it refuses is worth stating.
    #[test]
    fn a_sideways_move_under_a_vertical_hint_is_refused() {
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
        let before = stitcher.stitched.as_ref().unwrap().pixel_digest();
        let before_size = {
            let canvas = stitcher.stitched.as_ref().unwrap();
            (canvas.width, canvas.height)
        };

        let result = stitcher
            .push_frame(contained, Some(LongDirection::Up))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected contained canvas placement, got {result:?}");
        };
        assert_eq!(match_info.delta_y, -6);
        assert_eq!(stitcher.viewport_rect.unwrap().y, 2);
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!((after.width, after.height), before_size);
        assert_eq!(after.pixel_digest(), before, "a contained step must not repaint the canvas");
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
        let before = stitcher.stitched.as_ref().unwrap().pixel_digest();
        let before_size = {
            let canvas = stitcher.stitched.as_ref().unwrap();
            (canvas.width, canvas.height)
        };

        let result = stitcher
            .push_frame(contained, Some(LongDirection::Left))
            .unwrap();

        let PushResult::Accepted { match_info } = result else {
            panic!("expected contained canvas placement, got {result:?}");
        };
        assert_eq!(match_info.delta_x, -6);
        assert_eq!(stitcher.viewport_rect.unwrap().x, 2);
        let after = stitcher.stitched.as_ref().unwrap();
        assert_eq!((after.width, after.height), before_size);
        assert_eq!(after.pixel_digest(), before, "a contained step must not repaint the canvas");
    }

    #[test]
    #[cfg(feature = "stitch-legacy")]
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
    #[cfg(feature = "stitch-legacy")]
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
    #[cfg(feature = "stitch-legacy")]
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
        let before = stitched.pixel_digest();
        let previous_rect = ViewportRect { x: 0, y: 0, width: 32, height: 60 };
        let target = crop_frame(&world, 0, 20, 32, 60);

        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        engine.commit(previous_rect, 0, &crop_frame(&world, 0, 0, 32, 60));

        let request = PlacementRequest {
            stitched: &stitched,
            previous_rect,
            frame: &target,
            search_direction: Some(SearchDirection::Down),
            prebuilt_means: None,
        };
        let first = engine.place(&request);
        let second = engine.place(&request);
        assert_eq!(first, second, "place must be deterministic and side-effect free");
        assert_eq!(
            stitched.pixel_digest(),
            before,
            "place must not touch the stitched frame"
        );
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
        assert_eq!(
            stitched.pixel(7, 9),
            Some([
                previous.data[old_overlap],
                previous.data[old_overlap + 1],
                previous.data[old_overlap + 2],
            ])
        );

        let current_tail = pixel_offset(&current, 7, 23);
        assert_eq!(
            stitched.pixel(7, 27),
            Some([
                current.data[current_tail],
                current.data[current_tail + 1],
                current.data[current_tail + 2],
            ]),
            "the appended tail row must hold the frame's own last row"
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
        assert_eq!(Some(taller.data[0]), stitched.pixel(0, 0).map(|p| p[0]));
        assert_eq!(
            Some(first.data[0]),
            stitched.pixel(0, 4).map(|p| p[0]),
            "the old content moved down by four rows"
        );
    }

    #[test]
    fn converts_uncovered_stitched_corners_to_transparent() {
        let previous = frame(4, 2, 10);
        let mut stitched = StitchedFrame::from_first_frame(&previous);
        // Simulate a ragged canvas: only the left half of each row is captured.
        stitched.row_coverage = vec![RowCoverage::Spans(vec![(0, 2)]); 2];
        let image = image_from_stitched_frame(&stitched).unwrap();
        // Covered pixel keeps full alpha; uncovered right half is transparent.
        assert_eq!(image.data[3], 255);
        assert_eq!(image.data[4 * 2 + 0], 0);
        assert_eq!(image.data[4 * 2 + 3], 0);
    }

    /// The same guarantee, for the overlay's *recycled* buffer.
    ///
    /// The preview path hands the previous frame's allocation back in, so
    /// "uncovered pixels stay transparent" is no longer free: a pixel must be
    /// written even when nothing captured it, or the previous frame shows
    /// through. The overlay paints every pixel with a non-zero alpha, so a
    /// stale byte here is visible content in the corner of a T/L/arch capture.
    #[test]
    fn a_recycled_preview_buffer_clears_pixels_the_new_window_never_captured() {
        let previous = frame(4, 2, 10);
        let mut stitched = StitchedFrame::from_first_frame(&previous);
        // First pass: every pixel is covered, so the buffer comes back opaque.
        let covered =
            image_from_stitched_window_reusing(&stitched, CanvasWindow::full(&stitched), Vec::new())
                .unwrap();
        assert!(covered.opaque, "a rectangular canvas is fully opaque");
        assert!(covered
            .image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255));

        // The canvas is ragged now: the right half of each row was never
        // captured. Reusing the opaque buffer must not resurrect those pixels.
        stitched.row_coverage = vec![RowCoverage::Spans(vec![(0, 2)]); 2];
        let ragged = image_from_stitched_window_reusing(
            &stitched,
            CanvasWindow::full(&stitched),
            covered.image.data,
        )
        .unwrap();
        assert!(!ragged.opaque, "a ragged canvas is not opaque");
        for column in 0..4 {
            let alpha = ragged.image.data[column * 4 + 3];
            let want = if column < 2 { 255 } else { 0 };
            assert_eq!(
                alpha, want,
                "column {column} alpha after recycling a fully opaque buffer"
            );
        }
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
        assert!(stitched.row_coverage.iter().all(|r| r == &RowCoverage::Full));
    }

    /// An edit box that is nudged right, then back by a pixel, then right
    /// again, must leave the capture pointing the way it is really going.
    ///
    /// Every geometry change is a *known* step — the box moved by exactly
    /// `new_offset - last_offset`, so the frame is placed there without
    /// matching — and the pixels of the frame are placed to the right of the
    /// original ones throughout. The question this pins is what the stitcher
    /// believes afterwards: the next real scroll is horizontal, while the
    /// arrow key still says "down".
    #[test]
    fn an_edit_box_oscillating_by_one_pixel_keeps_the_horizontal_direction() {
        let page = |x: u32, y: u32| -> u8 {
            let mut hash =
                (x / 5).wrapping_mul(0x9E37_79B1) ^ (y / 5).wrapping_mul(0x85EB_CA77);
            hash ^= hash >> 15;
            hash = hash.wrapping_mul(0x2545_F491);
            hash ^= hash >> 13;
            (((x / 30 + y / 20) % 4) as u8) * 40 + (hash & 63) as u8
        };
        let frame_at = |frame_x: u32, frame_y: u32| -> RgbFrame {
            let (width, height) = (120u32, 80u32);
            let stride = width * 3;
            let mut data = Vec::with_capacity((stride * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let value = page(frame_x + x, frame_y + y);
                    data.extend_from_slice(&[value, value, value]);
                }
            }
            RgbFrame {
                width,
                height,
                stride,
                data,
            }
        };
        let offset = |x: i32, y: i32| FrameScreenOffset { x, y };
        let mut stitcher = RawStitcher::new();
        let mut push = |frame: RgbFrame, screen: FrameScreenOffset| {
            stitcher
                .push_frame_with_offset(&frame, &frame, Some(LongDirection::Down), screen)
                .unwrap()
        };

        assert!(matches!(push(frame_at(0, 0), offset(0, 0)), PushResult::Initialized));

        // The box moves right by 8 px: the frame's content sits 8 px right of
        // the content already captured.
        match push(frame_at(8, 0), offset(8, 0)) {
            PushResult::Accepted { match_info } => {
                assert_eq!((match_info.delta_x, match_info.delta_y), (8, 0));
            }
            other => panic!("the box moving right must place the frame, got {other:?}"),
        }
        // Corrected back by one pixel.
        match push(frame_at(7, 0), offset(7, 0)) {
            PushResult::Accepted { match_info } => {
                assert_eq!((match_info.delta_x, match_info.delta_y), (-1, 0));
            }
            other => panic!("the correction must place the frame, got {other:?}"),
        }
        // And one pixel right again.
        match push(frame_at(8, 0), offset(8, 0)) {
            PushResult::Accepted { match_info } => {
                assert_eq!((match_info.delta_x, match_info.delta_y), (1, 0));
            }
            other => panic!("the second correction must place the frame, got {other:?}"),
        }

        // The capture now scrolls right while the hint still says "down".
        // Nothing about the arrow key is right any more; the last verified
        // step was horizontal and that is what has to be followed.
        match push(frame_at(38, 0), offset(8, 0)) {
            PushResult::Accepted { match_info } => {
                assert_eq!(
                    (match_info.delta_x, match_info.delta_y),
                    (30, 0),
                    "the scroll is horizontal and 30 px long"
                );
            }
            other => panic!(
                "after a horizontal edit the next scroll must be placed on the \
                 horizontal axis, got {other:?}"
            ),
        }

        let stitched = stitcher.stitched().unwrap();
        assert_eq!((stitched.width, stitched.height), (158, 80));
        for y in 0..80 {
            for x in 0..158 {
                let expected = page(x, y);
                assert_eq!(
                    stitched_pixel_rgb(stitched, x, y),
                    [expected, expected, expected],
                    "canvas pixel ({x},{y}) must be the page"
                );
            }
        }
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
        assert!(stitched.row_coverage.iter().all(|r| r == &RowCoverage::Full));
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
        assert!(stitched.row_coverage.iter().all(|r| r == &RowCoverage::Full));
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
        assert_eq!(stitched.row_coverage[0], RowCoverage::Spans(vec![(0, 4)]));
        assert_eq!(stitched.row_coverage[5], RowCoverage::Spans(vec![(3, 7)]));
        // Uncovered top-right corner of the L stays transparent on output;
        // the bottom row is covered only where the old canvas sits (x 3..7).
        let image = image_from_stitched_frame(stitched).unwrap();
        assert_eq!(image.data[6 * 4 + 3], 0);
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
        let mut row = vec![0u8; stitched.stride as usize];
        for y in (0..stitched.height).step_by(20) {
            let want = row_texture(width, y);
            assert!(stitched.copy_row(y, &mut row), "canvas row {y} is missing");
            assert_eq!(
                &row[..width as usize * 3],
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
        let mut row = vec![0u8; stitched.stride as usize];
        for y in (0..stitched.height).step_by(20) {
            let want = row_texture(width, y);
            assert!(stitched.copy_row(y, &mut row), "canvas row {y} is missing");
            assert_eq!(
                &row[..width as usize * 3],
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

    /// A refusal must still hand its conversion buffer back.
    ///
    /// Refusals are not exceptional: `real-1` refuses 257 of its 461 frames. If
    /// the buffer only comes back on an accept, every refused frame allocates
    /// 6.2 MB (1080p) and faults in every page of it — which is exactly what the
    /// recycling is meant to stop.
    #[test]
    fn a_refused_frame_hands_its_conversion_buffer_back() {
        let width = 64;
        let height = 200;
        let mut stitcher = RawStitcher::new();
        for top in [0u32, 40, 80] {
            let frame = frame_from_world_rows(width, &(top..top + height).collect::<Vec<_>>());
            stitcher.push_frame(frame, None).unwrap();
        }

        // Start from "nothing to recycle", so a buffer after the refusal can
        // only have come from the refused frame itself.
        stitcher.spare_compose_buffer = Vec::new();
        let jump = frame_from_world_rows(width, &(900..900 + height).collect::<Vec<_>>());
        assert_eq!(
            push_until_refused(&mut stitcher, &jump, None),
            PushResult::NoMatch
        );
        assert!(
            stitcher.spare_compose_buffer.len() >= width as usize * height as usize * 3,
            "a refused frame dropped its conversion buffer: {} bytes spare",
            stitcher.spare_compose_buffer.len()
        );
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

    /// An arch (∩): scroll up on the left, widen at the top, move the box to
    /// the right and scroll down. The canvas must hold all three parts *and*
    /// keep the space between the legs transparent — it was never selected, so
    /// it was never captured.
    ///
    /// Two defects made this impossible before. A frame whose rect fell inside
    /// the canvas *bounding box* was treated as already captured, so the right
    /// leg was dropped while still reported as accepted; and per-row coverage
    /// was a hull, so the gap between the legs was painted opaque.
    ///
    /// The geometry path itself is engine-agnostic, but `LegacyPipeline` is an
    /// A/B artifact that was never taught to place frames across a selection
    /// move, so this asserts the shipped engine only.
    #[cfg(not(feature = "stitch-legacy"))]
    #[test]
    fn an_arch_keeps_the_gap_between_its_legs_transparent() {
        const PAGE_WIDTH: u32 = 320;
        const HEIGHT: u32 = 100;
        // One page row per page row: the content is the same no matter which
        // column or which selection captured it.
        let page: Vec<Vec<u8>> = (0..2000u32).map(|row| row_texture(PAGE_WIDTH, row)).collect();
        let crop = |x: u32, top: u32, width: u32| -> RgbFrame {
            let stride = width * 3;
            let mut data = Vec::with_capacity((stride * HEIGHT) as usize);
            for y in 0..HEIGHT {
                let row = &page[(top + y) as usize];
                data.extend_from_slice(&row[(x * 3) as usize..((x + width) * 3) as usize]);
            }
            RgbFrame { width, height: HEIGHT, stride, data }
        };
        let mut stitcher = RawStitcher::new();
        // Left leg: scroll up with the box on the left.
        let left = FrameScreenOffset { x: 0, y: 0 };
        let first = crop(0, 400, 100);
        assert!(matches!(
            stitcher.push_frame_with_offset(&first, &first, Some(LongDirection::Up), left),
            Ok(PushResult::Initialized)
        ));
        for top in [350u32, 300, 250, 200] {
            let frame = crop(0, top, 100);
            assert!(
                matches!(
                    stitcher.push_frame_with_offset(&frame, &frame, Some(LongDirection::Up), left),
                    Ok(PushResult::Accepted { .. })
                ),
                "the left leg must keep extending at page row {top}"
            );
        }
        // Widen at the top: same position, three times the width.
        let widened = crop(0, 200, 300);
        assert!(matches!(
            stitcher.push_frame_with_offset(&widened, &widened, Some(LongDirection::Up), left),
            Ok(PushResult::Accepted { .. })
        ));
        // Narrow again, this time on the right, then scroll down.
        let right = FrameScreenOffset { x: 200, y: 0 };
        let moved = crop(200, 200, 100);
        assert!(matches!(
            stitcher.push_frame_with_offset(&moved, &moved, Some(LongDirection::Up), right),
            Ok(PushResult::Accepted { .. })
        ));
        for top in [250u32, 300, 350, 400] {
            let frame = crop(200, top, 100);
            assert!(
                matches!(
                    stitcher.push_frame_with_offset(&frame, &frame, Some(LongDirection::Down), right),
                    Ok(PushResult::Accepted { .. })
                ),
                "the right leg must keep extending at page row {top}"
            );
        }

        let stitched = stitcher.stitched().unwrap();
        assert_eq!((stitched.width, stitched.height), (300, 300));
        // Canvas row = page row - 200.
        assert_eq!(stitched.row_coverage[0], RowCoverage::Full, "the top bar spans everything");
        assert_eq!(stitched.row_coverage[50], RowCoverage::Full);
        for row in [100usize, 150, 200, 250, 299] {
            assert_eq!(
                stitched.row_coverage[row],
                RowCoverage::Spans(vec![(0, 100), (200, 300)]),
                "row {row} must hold both legs and nothing between them"
            );
        }

        let image = image_from_stitched_frame(stitched).unwrap();
        let alpha = |x: usize, y: usize| image.data[(y * 300 + x) * 4 + 3];
        let rgb = |x: usize, y: usize| {
            let at = (y * 300 + x) * 4;
            [image.data[at + 2], image.data[at + 1], image.data[at]]
        };
        // The gap between the legs was never selected: transparent.
        for (x, y) in [(150, 200), (100, 299), (199, 120)] {
            assert_eq!(alpha(x, y), 0, "({x},{y}) lies between the legs and must be transparent");
        }
        // Both legs carry the page content they captured.
        for (x, y, page_row) in [(50, 200, 400usize), (250, 200, 400), (250, 299, 499)] {
            assert_eq!(alpha(x, y), 255, "({x},{y}) is inside a leg and must be opaque");
            let expected = &page[page_row][x * 3..x * 3 + 3];
            assert_eq!(rgb(x, y), [expected[0], expected[1], expected[2]]);
        }
        // The top bar spans the full width.
        assert_eq!(alpha(150, 50), 255);
        let expected = &page[250][150 * 3..150 * 3 + 3];
        assert_eq!(rgb(150, 50), [expected[0], expected[1], expected[2]]);
    }

    /// A page whose only feature is a single horizontal boundary between two
    /// grays: no texture, no second edge, nothing else to match on.
    ///
    /// The boundary row is the entire evidence budget — one scoring row. Every
    /// frame that contains it must be placed at the row it really occupies,
    /// including the frame where it sits on the first row. A frame that is
    /// entirely one gray carries no evidence at all, and refusing it is the
    /// only honest answer; the test asserts that too, so a future "fix" that
    /// invents a placement out of nothing fails here.
    ///
    /// Gated off for `LegacyPipeline`: the old canvas scan does not use the
    /// boundary-row evidence gate at all, so this says nothing about it.
    #[cfg(not(feature = "stitch-legacy"))]
    #[test]
    fn a_single_gray_boundary_is_enough_to_stitch() {
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 120;
        const BOUNDARY: u32 = 600;
        let page_row = |row: u32| if row < BOUNDARY { 90u8 } else { 170 };
        let frame_at = |top: u32| -> RgbFrame {
            let stride = WIDTH * 3;
            let mut data = Vec::with_capacity((stride * HEIGHT) as usize);
            for y in 0..HEIGHT {
                let gray = page_row(top + y);
                for _ in 0..WIDTH {
                    data.extend_from_slice(&[gray, gray, gray]);
                }
            }
            RgbFrame {
                width: WIDTH,
                height: HEIGHT,
                stride,
                data,
            }
        };

        let mut stitcher = RawStitcher::new();
        // The first frame must contain the boundary: it is the only frame that
        // is not matched against anything.
        assert!(matches!(
            stitcher.push_frame(frame_at(520), Some(LongDirection::Down)).unwrap(),
            PushResult::Initialized
        ));

        // The boundary walks up through the frame, ending one row below its
        // top — the least evidence a frame can carry and still have any.
        let mut deltas = Vec::new();
        for top in [540u32, 560, 580, 599] {
            match stitcher.push_frame(frame_at(top), Some(LongDirection::Down)).unwrap() {
                PushResult::Accepted { match_info } => deltas.push(match_info.delta_y),
                other => panic!("the boundary is inside the frame at {top}, got {other:?}"),
            }
        }
        assert_eq!(deltas, vec![20, 20, 20, 19], "each step must land on its real row");

        // The boundary has just left the top of the frame: rows 600..720 are
        // one flat gray. There is nothing to compare, so this is refused once
        // the patience runs out — and that is the correct answer, not a bug.
        let flat = frame_at(600);
        assert_eq!(
            push_until_refused(&mut stitcher, &flat, Some(LongDirection::Down)),
            PushResult::NoMatch
        );

        // Scroll back up: the boundary re-enters the frame. The canvas still
        // holds it, so placement resumes even though eight frames were refused.
        let mut back = Vec::new();
        for top in [580u32, 560, 540] {
            match stitcher.push_frame(frame_at(top), Some(LongDirection::Up)).unwrap() {
                PushResult::Accepted { match_info } => back.push(match_info.delta_y),
                other => panic!("the boundary is back in view at {top}, got {other:?}"),
            }
        }
        assert_eq!(back, vec![-19, -20, -20]);

        // The canvas row really is the page row: frame 520 initialised it, so
        // page row 600 is canvas row 80.
        let stitched = stitcher.stitched().unwrap();
        assert_eq!((stitched.width, stitched.height), (WIDTH, 199));
        assert!(stitched.row_coverage.iter().all(|row| row == &RowCoverage::Full));
        assert_eq!(stitched_pixel_rgb(stitched, 10, 79), [90, 90, 90]);
        assert_eq!(stitched_pixel_rgb(stitched, 10, 80), [170, 170, 170]);
    }

    /// The large-scale structure of the page used by the snake tests:
    /// random-height brightness bands down the page, random-width bands across
    /// it.
    ///
    /// `assert_snake_path` combines them with a hashed 8×8 block texture, and
    /// that texture is not decoration. The vertical descriptor keeps `|dL/dy|`
    /// per bin and nothing else, so a page that is only a sum of a row term and
    /// a column term has the *same* vertical descriptor in every column — a
    /// vertical step would then match a column it never captured. Real pages
    /// carry text and images whose row boundaries differ across columns.
    fn snake_page(page_w: u32, page_h: u32) -> (Vec<u8>, Vec<u8>) {
        snake_page_seeded(page_w, page_h, 0x5eed_1234)
    }

    /// The same page from an explicit seed, so the randomised sessions can walk
    /// over content the hand-written shapes never see.
    fn snake_page_seeded(page_w: u32, page_h: u32, seed: u32) -> (Vec<u8>, Vec<u8>) {
        fn lcg(state: &mut u32) -> u32 {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *state >> 16
        }
        let mut state = seed;
        let mut row_band = Vec::with_capacity(page_h as usize);
        while (row_band.len() as u32) < page_h {
            let run = 3 + lcg(&mut state) % 10;
            let value = 50 + lcg(&mut state) % 150;
            for _ in 0..run.min(page_h - row_band.len() as u32) {
                row_band.push(value as u8);
            }
        }
        let mut col_band = Vec::with_capacity(page_w as usize);
        while (col_band.len() as u32) < page_w {
            let run = 5 + lcg(&mut state) % 9;
            let value = lcg(&mut state) % 256;
            for _ in 0..run.min(page_w - col_band.len() as u32) {
                col_band.push(value as u8);
            }
        }
        (row_band, col_band)
    }

    /// One pixel of the synthetic page both the serpentine and the randomised
    /// groups walk over: a large-scale brightness band on each axis plus a
    /// hashed 8x8 block texture.
    ///
    /// The bands give a walk its structure; the texture is what makes one
    /// column's vertical descriptor unlike another's. A page that is merely the
    /// *sum* of a row term and a column term has the same `|dL/dy|` in every
    /// column, and a vertical step would then match a column it never captured —
    /// real pages carry text and images, whose row boundaries differ across
    /// columns.
    fn banded_page_pixel(row_band: &[u8], col_band: &[u8], x: u32, y: u32) -> u8 {
        let band = (u16::from(row_band[y as usize]) + u16::from(col_band[x as usize])) / 2;
        let mut hash = (x / 8).wrapping_mul(0x9E37_79B1) ^ (y / 8).wrapping_mul(0x85EB_CA77);
        hash ^= hash >> 15;
        hash = hash.wrapping_mul(0x2545_F491);
        hash ^= hash >> 13;
        (band + (hash & 63) as u16).min(255) as u8
    }

    /// Start positions of a serpentine walk: uniform steps, plus the page edge
    /// when the last uniform step would stop short of it.
    fn snake_positions(page: u32, view: u32, step: u32) -> Vec<u32> {
        let last = page - view;
        let mut positions: Vec<u32> = (0..)
            .map(|index| index * step)
            .take_while(|&position| position < last)
            .collect();
        positions.push(last);
        positions
    }

    /// Walks a serpentine path over `snake_page` and asserts that every step
    /// lands on its true delta and that the finished canvas is the page itself.
    fn assert_snake_path(
        label: &str,
        page_w: u32,
        page_h: u32,
        view_w: u32,
        view_h: u32,
        step_x: u32,
        step_y: u32,
    ) {
        let (row_band, col_band) = snake_page(page_w, page_h);
        let page = |x: u32, y: u32| -> u8 { banded_page_pixel(&row_band, &col_band, x, y) };
        let frame_at = |frame_x: u32, frame_y: u32| -> RgbFrame {
            let stride = view_w * 3;
            let mut data = Vec::with_capacity((stride * view_h) as usize);
            for y in 0..view_h {
                for x in 0..view_w {
                    let value = page(frame_x + x, frame_y + y);
                    data.extend_from_slice(&[value, value, value]);
                }
            }
            RgbFrame {
                width: view_w,
                height: view_h,
                stride,
                data,
            }
        };

        let xs = snake_positions(page_w, view_w, step_x);
        let ys = snake_positions(page_h, view_h, step_y);
        assert_eq!(
            (xs.last(), ys.last()),
            (Some(&(page_w - view_w)), Some(&(page_h - view_h))),
            "{label}: the walk must reach the bottom-right corner"
        );
        let mut path: Vec<(u32, u32)> = Vec::new();
        for (row_index, &y) in ys.iter().enumerate() {
            let mut row = xs.clone();
            if row_index % 2 == 1 {
                row.reverse();
            }
            for x in row {
                path.push((x, y));
            }
        }

        let mut stitcher = RawStitcher::new();
        for (index, &(x, y)) in path.iter().enumerate() {
            let direction = if index == 0 {
                LongDirection::Down
            } else {
                let (previous_x, previous_y) = path[index - 1];
                if y != previous_y {
                    LongDirection::Down
                } else if x > previous_x {
                    LongDirection::Right
                } else {
                    LongDirection::Left
                }
            };
            let result = stitcher
                .push_frame(frame_at(x, y), Some(direction))
                .unwrap();
            if index == 0 {
                assert!(
                    matches!(result, PushResult::Initialized),
                    "{label}: the first frame must initialise, got {result:?}"
                );
                continue;
            }
            let (previous_x, previous_y) = path[index - 1];
            let expected = (
                x as i32 - previous_x as i32,
                y as i32 - previous_y as i32,
            );
            match result {
                PushResult::Accepted { match_info } => assert_eq!(
                    (match_info.delta_x, match_info.delta_y),
                    expected,
                    "{label}: step {index} from ({previous_x},{previous_y}) to ({x},{y})"
                ),
                other => panic!(
                    "{label}: step {index} from ({previous_x},{previous_y}) to ({x},{y}) \
                     must be placed, got {other:?}"
                ),
            }
        }

        let stitched = stitcher.stitched().unwrap();
        assert_eq!(
            (
                stitched.width,
                stitched.height,
                stitched.anchor_origin_x,
                stitched.anchor_origin_y
            ),
            (page_w, page_h, 0, 0),
            "{label}: the finished canvas must be the page"
        );
        for y in 0..page_h {
            for x in 0..page_w {
                let expected = page(x, y);
                assert_eq!(
                    stitched_pixel_rgb(stitched, x, y),
                    [expected, expected, expected],
                    "{label}: canvas pixel ({x},{y})"
                );
            }
        }
    }

    /// Serpentine capture over a page wider than the viewport: right along the
    /// top, down, left, down, right again. The canvas has to grow in both axes
    /// and the two scroll axes have to hand over to each other.
    #[test]
    fn a_snake_path_reconstructs_a_page_wider_than_the_viewport() {
        assert_snake_path("half overlap", 1600, 1200, 640, 480, 320, 240);
    }

    /// The same walk on a wider page where consecutive frames overlap by only a
    /// quarter of the viewport. Overlap is the only thing that makes a step
    /// placeable, so this is the tighter case, not a bigger one.
    #[test]
    fn a_snake_path_works_on_a_wider_page_with_quarter_overlap() {
        assert_snake_path("quarter overlap", 1920, 1200, 640, 480, 480, 240);
    }

    /// Randomised multi-step capture sessions: random initial selections,
    /// random box drags and resizes, random scrolling on both axes, and walks
    /// that cross their own path.
    ///
    /// Gated off for `LegacyPipeline`: the old canvas scan has no impulse axis,
    /// no anchor window and no colour verification, so it refuses steps the
    /// shipped engine places (measured on these very seeds). This group asserts
    /// the engine the product actually runs.
    #[cfg(not(feature = "stitch-legacy"))]
    mod random_sessions {
        use super::*;

        // The serpentine and arch cases above are hand-written shapes: one path
        // down, one across, one notch. A real capture is not that tidy — the user
        // scrolls up and down, wanders sideways, drags the box around and reshapes
        // it, and the path crosses itself over content the canvas already holds.
        //
        // These sessions generate that shape from a seed: every step is a random
        // operation (scroll, box drag, resize), every frame is checked against the
        // delta its true position implies, and the finished canvas is compared to
        // the page pixel for pixel — including the rows and columns the walk never
        // visited, which must stay transparent.

        /// Deterministic xorshift32: a random case must stay reproducible from its
        /// seed alone, so a failure is always a failure.
        struct WalkRng(u32);

        impl WalkRng {
            fn new(seed: u32) -> Self {
                // Mix the seed so neighbouring seeds do not start in adjacent
                // states, and keep the state non-zero: xorshift is a fixed point
                // at zero.
                let mixed = seed.wrapping_mul(0x9e37_79b9);
                Self(if mixed == 0 { 1 } else { mixed })
            }

            fn next(&mut self) -> u32 {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                self.0 = x;
                x
            }

            /// Inclusive `low..=high`.
            fn between(&mut self, low: i32, high: i32) -> i32 {
                debug_assert!(low <= high, "empty range {low}..={high}");
                low + (self.next() % (high - low + 1) as u32) as i32
            }

            fn below(&mut self, bound: usize) -> usize {
                (self.next() as usize) % bound.max(1)
            }

            fn percent(&mut self, percent: u32) -> bool {
                self.next() % 100 < percent
            }
        }

        fn rectangles_overlap(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> bool {
            let (ax, ay, aw, ah) = a;
            let (bx, by, bw, bh) = b;
            ax < bx + bw as i32 && bx < ax + aw as i32 && ay < by + bh as i32 && by < ay + ah as i32
        }

        /// How far a box's origin may move when the box is `width` wide on a
        /// `page`-wide axis: a small nudge while it fits, widened as much as the new
        /// size needs when it no longer does.
        fn origin_range(position: i32, width: u32, page: u32) -> (i32, i32) {
            let room = page as i32 - width as i32 - position;
            let low = (-position).max(-60).min(room);
            let high = room.min(60).max(low);
            (low, high)
        }

        /// One simulated interactive capture.
        ///
        /// `x`/`y` is the page rectangle the selection box currently shows, and
        /// `screen_*` is the offset that rectangle reports to the stitcher. They
        /// move together when the box is dragged or reshaped (a known geometry
        /// step) and only the page position moves when the content scrolls.
        struct RandomSession {
            stitcher: RawStitcher,
            row_band: Vec<u8>,
            col_band: Vec<u8>,
            page_w: u32,
            page_h: u32,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            screen_x: i32,
            screen_y: i32,
            visited: Vec<(i32, i32, u32, u32)>,
            /// Steps that landed on content captured more than one step earlier:
            /// the crossings this group exists to exercise.
            crossings: usize,
            horizontal_steps: usize,
            vertical_steps: usize,
            edits: usize,
        }

        impl RandomSession {
            /// A session with a randomly chosen selection: its size, its position on
            /// the page and the screen offset it reports are all drawn from the
            /// seed, so no two seeds even start from the same box.
            fn new(seed: u32, page_w: u32, page_h: u32) -> Self {
                let mut rng = WalkRng::new(seed ^ 0x9e37_79b9);
                let width = rng.between((page_w / 4) as i32, (page_w / 3) as i32) as u32;
                let height = rng.between((page_h / 5) as i32, (page_h / 4) as i32) as u32;
                let x = rng.between(0, (page_w - width) as i32);
                let y = rng.between(0, (page_h - height) as i32);
                let screen_x = rng.between(0, 800);
                let screen_y = rng.between(0, 800);
                let (row_band, col_band) =
                    snake_page_seeded(page_w, page_h, seed.wrapping_mul(2_654_435_761));
                Self {
                    stitcher: RawStitcher::new(),
                    row_band,
                    col_band,
                    page_w,
                    page_h,
                    x,
                    y,
                    width,
                    height,
                    screen_x,
                    screen_y,
                    visited: Vec::new(),
                    crossings: 0,
                    horizontal_steps: 0,
                    vertical_steps: 0,
                    edits: 0,
                }
            }

            fn pixel(&self, x: u32, y: u32) -> u8 {
                banded_page_pixel(&self.row_band, &self.col_band, x, y)
            }

            fn frame(&self) -> RgbFrame {
                let stride = self.width * 3;
                let mut data = Vec::with_capacity((stride * self.height) as usize);
                for row in 0..self.height {
                    for column in 0..self.width {
                        let value =
                            self.pixel((self.x + column as i32) as u32, (self.y + row as i32) as u32);
                        data.extend_from_slice(&[value, value, value]);
                    }
                }
                RgbFrame { width: self.width, height: self.height, stride, data }
            }

            /// Feeds the current rectangle to the stitcher and checks the delta.
            fn push(&mut self, direction: Option<LongDirection>, label: &str) {
                let frame = self.frame();
                let offset = FrameScreenOffset { x: self.screen_x, y: self.screen_y };
                let result = self
                    .stitcher
                    .push_frame_with_offset(&frame, &frame, direction, offset)
                    .unwrap_or_else(|error| panic!("{label}: push failed: {error}"));
                let current = (self.x, self.y, self.width, self.height);
                match (self.visited.last().copied(), result) {
                    (None, PushResult::Initialized) => {}
                    (None, other) => {
                        panic!("{label}: the first frame must initialise, got {other:?}")
                    }
                    (Some((previous_x, previous_y, _, _)), PushResult::Accepted { match_info }) => {
                        let expected = (self.x - previous_x, self.y - previous_y);
                        assert_eq!(
                            (match_info.delta_x, match_info.delta_y),
                            expected,
                            "{label}: the page moved by {expected:?} but the frame was placed at {:?}",
                            (match_info.delta_x, match_info.delta_y)
                        );
                    }
                    (Some(previous), other) => panic!(
                        "{label}: step {} from {previous:?} to {current:?} must be placed, got {other:?}",
                        self.visited.len()
                    ),
                }
                self.note_crossing();
                self.visited.push(current);
            }

            fn note_crossing(&mut self) {
                let current = (self.x, self.y, self.width, self.height);
                // Consecutive frames always overlap, so they say nothing; a frame
                // that overlaps anything older is a crossing.
                let revisited = self
                    .visited
                    .iter()
                    .rev()
                    .skip(1)
                    .any(|&earlier| rectangles_overlap(current, earlier));
                if revisited {
                    self.crossings += 1;
                }
            }

            /// Scrolls the content inside the box: the box stays put on screen, the
            /// page moves under it.
            fn scroll(&mut self, dx: i32, dy: i32) {
                if dx != 0 {
                    self.horizontal_steps += 1;
                } else {
                    self.vertical_steps += 1;
                }
                self.x += dx;
                self.y += dy;
            }

            /// Drags the box over the page without scrolling (edit mode): the
            /// content moves with the box, so the reported offset moves too.
            fn move_box(&mut self, dx: i32, dy: i32) {
                self.edits += 1;
                self.x += dx;
                self.y += dy;
                self.screen_x += dx;
                self.screen_y += dy;
            }

            /// Reshapes the box, optionally moving its origin as an edge drag does.
            fn resize(&mut self, dx: i32, dy: i32, width: u32, height: u32) {
                self.edits += 1;
                self.x += dx;
                self.y += dy;
                self.screen_x += dx;
                self.screen_y += dy;
                self.width = width;
                self.height = height;
            }

            /// A random scroll step along one axis: always at least `min` pixels,
            /// never longer than `max`, always inside the page. The direction comes
            /// from the room available, so the walk never stalls against an edge.
            fn scroll_step(&self, rng: &mut WalkRng, horizontal: bool, min: i32, max: i32) -> i32 {
                let (position, extent, page) = if horizontal {
                    (self.x, self.width as i32, self.page_w as i32)
                } else {
                    (self.y, self.height as i32, self.page_h as i32)
                };
                let forward = page - extent - position;
                let back = position;
                let magnitude = rng.between(min, max.min(forward.max(back)));
                if rng.percent(50) && forward >= magnitude {
                    magnitude
                } else if back >= magnitude {
                    -magnitude
                } else {
                    magnitude
                }
            }

            /// A scroll step that heads back toward a rectangle captured earlier:
            /// the capture doubles back over content it already holds, which is what
            /// makes the path cross itself. One axis only — the matcher places a
            /// single-axis step, and a diagonal one is a different (unsupported)
            /// question.
            fn return_step(&self, rng: &mut WalkRng) -> (i32, i32) {
                let (target_x, target_y, _, _) = self.visited[rng.below(self.visited.len())];
                let distance_x = (target_x - self.x).abs();
                let distance_y = (target_y - self.y).abs();
                let max_x = (self.width / 3) as i32;
                let max_y = (self.height / 3) as i32;
                if distance_x > 0 && distance_x >= distance_y {
                    ((target_x - self.x).clamp(-max_x, max_x), 0)
                } else if distance_y > 0 {
                    (0, (target_y - self.y).clamp(-max_y, max_y))
                } else {
                    // The target is where the box already is: step aside instead.
                    (self.scroll_step(rng, true, 8, max_x), 0)
                }
            }

            /// A random box drag that stays on the page. A drag is a known geometry
            /// step, so it may move both axes at once.
            fn drag_step(&self, rng: &mut WalkRng) -> (i32, i32) {
                let room_right = self.page_w as i32 - self.width as i32 - self.x;
                let room_down = self.page_h as i32 - self.height as i32 - self.y;
                loop {
                    let dx = rng.between(-self.x.min(120), room_right.min(120));
                    let dy = rng.between(-self.y.min(120), room_down.min(120));
                    if dx != 0 || dy != 0 {
                        return (dx, dy);
                    }
                }
            }

            /// A random reshape: the size always changes, and the origin may move as
            /// an edge drag does. Both stay on the page.
            fn resize_step(&self, rng: &mut WalkRng) -> (i32, i32, u32, u32) {
                let min_width = 140u32;
                let min_height = 110u32;
                let max_width = self.page_w / 3;
                let max_height = self.page_h / 4;
                let mut width = rng.between(min_width as i32, max_width as i32) as u32;
                let height = rng.between(min_height as i32, max_height as i32) as u32;
                if width == self.width && height == self.height {
                    // Guarantee a geometry change so this really is an edit step.
                    width = if width + 16 <= max_width { width + 16 } else { width - 16 };
                }
                let (dx_lo, dx_hi) = origin_range(self.x, width, self.page_w);
                let (dy_lo, dy_hi) = origin_range(self.y, height, self.page_h);
                (rng.between(dx_lo, dx_hi), rng.between(dy_lo, dy_hi), width, height)
            }

            /// Compares the finished canvas to the page: every captured pixel must
            /// be the page's, every never-captured pixel must be empty, and the
            /// canvas must be exactly the bounding box of what the walk visited.
            fn assert_canvas(&self, label: &str, min_crossings: usize, min_edits: usize) {
                assert!(
                    self.crossings >= min_crossings,
                    "{label}: the walk must cross its own path at least {min_crossings} times, \
                     got {} — the shape is too simple to be the case this group exists for",
                    self.crossings
                );
                assert!(
                    self.edits >= min_edits,
                    "{label}: the session must contain at least {min_edits} box edits, got {}",
                    self.edits
                );
                let left = self.visited.iter().map(|&(x, _, _, _)| x).min().unwrap();
                let top = self.visited.iter().map(|&(_, y, _, _)| y).min().unwrap();
                let right = self.visited.iter().map(|&(x, _, w, _)| x + w as i32).max().unwrap();
                let bottom = self.visited.iter().map(|&(_, y, _, h)| y + h as i32).max().unwrap();
                let stitched = self.stitcher.stitched().expect("a stitched canvas");
                // The canvas is anchored on the first frame; `anchor_origin` is the
                // distance the canvas grew left/up of it, so the page coordinate of
                // canvas column 0 is the first frame's page x minus that growth.
                let anchor_x = self.visited[0].0 - stitched.anchor_origin_x;
                let anchor_y = self.visited[0].1 - stitched.anchor_origin_y;
                assert_eq!(
                    (anchor_x, anchor_y),
                    (left, top),
                    "{label}: the canvas must start at the top-left of everything captured"
                );
                assert_eq!(
                    (stitched.width, stitched.height),
                    ((right - anchor_x) as u32, (bottom - anchor_y) as u32),
                    "{label}: the canvas must be the bounding box of everything captured"
                );
                for canvas_y in 0..stitched.height {
                    let page_y = anchor_y + canvas_y as i32;
                    let expected = self.expected_spans(anchor_x, page_y, stitched.width);
                    let coverage = if stitched.row_coverage.is_empty() {
                        RowCoverage::Full
                    } else {
                        stitched.row_coverage[canvas_y as usize].clone()
                    };
                    for canvas_x in 0..stitched.width {
                        let covered = coverage.contains(canvas_x);
                        let expected_covered =
                            expected.iter().any(|&(lo, hi)| canvas_x >= lo && canvas_x < hi);
                        assert_eq!(
                            covered, expected_covered,
                            "{label}: coverage at canvas ({canvas_x},{canvas_y})"
                        );
                        let pixel = stitched_pixel_rgb(stitched, canvas_x, canvas_y);
                        if covered {
                            let value = self.pixel(
                                (anchor_x + canvas_x as i32) as u32,
                                page_y as u32,
                            );
                            assert_eq!(
                                pixel,
                                [value, value, value],
                                "{label}: content at canvas ({canvas_x},{canvas_y})"
                            );
                        } else {
                            assert_eq!(
                                pixel,
                                [0, 0, 0],
                                "{label}: never-captured canvas ({canvas_x},{canvas_y}) must stay empty"
                            );
                        }
                    }
                }
            }

            /// Captured x-extents of one page row, in canvas coordinates.
            fn expected_spans(&self, anchor_x: i32, page_y: i32, width: u32) -> Vec<(u32, u32)> {
                let mut spans: Vec<(u32, u32)> = Vec::new();
                for &(x, y, frame_width, frame_height) in &self.visited {
                    if page_y < y || page_y >= y + frame_height as i32 {
                        continue;
                    }
                    let lo = (x - anchor_x).max(0) as u32;
                    let hi = ((x - anchor_x) + frame_width as i32).min(width as i32) as u32;
                    if lo < hi {
                        spans.push((lo, hi));
                    }
                }
                spans.sort_unstable();
                let mut merged: Vec<(u32, u32)> = Vec::new();
                for (lo, hi) in spans {
                    match merged.last_mut() {
                        Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                        _ => merged.push((lo, hi)),
                    }
                }
                merged
            }
        }

        /// A long random scroll walk on a page larger than the box: both axes,
        /// arbitrary directions, steps up to a third of the box, and a bias toward
        /// doubling back over captured content so the path crosses itself.
        #[test]
        fn a_random_scroll_walk_reconstructs_the_page() {
            for seed in [0x5eed_0001u32, 0x5eed_0002, 0x5eed_0003] {
                let label = format!("scroll walk seed {seed:#x}");
                let mut rng = WalkRng::new(seed);
                let mut session = RandomSession::new(seed, 840, 1260);
                session.push(Some(LongDirection::Down), &format!("{label}: first frame"));
                let mut hint = Some(LongDirection::Down);
                for step in 1..=40 {
                    let label = format!("{label}: step {step}");
                    let horizontal = rng.percent(40);
                    let (dx, dy) = if step > 3 && rng.percent(25) {
                        session.return_step(&mut rng)
                    } else if horizontal {
                        (session.scroll_step(&mut rng, true, 8, 90), 0)
                    } else {
                        (0, session.scroll_step(&mut rng, false, 8, 70))
                    };
                    if dx != 0 {
                        // A horizontal step needs a horizontal hint: nothing in the
                        // frame says which side it moved to.
                        hint = Some(if dx > 0 { LongDirection::Right } else { LongDirection::Left });
                    } else if rng.percent(30) {
                        hint = Some(if dy > 0 { LongDirection::Down } else { LongDirection::Up });
                    }
                    session.scroll(dx, dy);
                    session.push(hint, &label);
                }
                assert!(
                    session.horizontal_steps >= 5 && session.vertical_steps >= 5,
                    "{label}: both axes must be exercised, got {} horizontal and {} vertical steps",
                    session.horizontal_steps,
                    session.vertical_steps
                );
                session.assert_canvas(&label, 6, 0);
            }
        }

        /// The same idea with edit mode mixed in: between scrolls the box is dragged
        /// over the page and reshaped, so the selection (not just the content) moves
        /// and the canvas becomes a ragged, self-crossing shape.
        #[test]
        fn a_random_edit_box_session_reconstructs_the_page() {
            for seed in [0xed17_0001u32, 0xed17_0002, 0xed17_0003] {
                let label = format!("edit session seed {seed:#x}");
                let mut rng = WalkRng::new(seed);
                let mut session = RandomSession::new(seed, 960, 1320);
                session.push(Some(LongDirection::Down), &format!("{label}: first frame"));
                let mut hint = Some(LongDirection::Down);
                for step in 1..=36 {
                    let label = format!("{label}: step {step}");
                    match rng.next() % 10 {
                        // Drag the box over the page: a known geometry step.
                        0..=2 => {
                            let (dx, dy) = session.drag_step(&mut rng);
                            session.move_box(dx, dy);
                        }
                        // Reshape it, sometimes moving its origin.
                        3..=4 => {
                            let (dx, dy, width, height) = session.resize_step(&mut rng);
                            session.resize(dx, dy, width, height);
                        }
                        // Scroll the content inside it.
                        _ => {
                            let (dx, dy) = if rng.percent(40) {
                                (session.scroll_step(&mut rng, true, 8, 100), 0)
                            } else {
                                (0, session.scroll_step(&mut rng, false, 8, 80))
                            };
                            if dx != 0 {
                                hint = Some(if dx > 0 {
                                    LongDirection::Right
                                } else {
                                    LongDirection::Left
                                });
                            } else if rng.percent(30) {
                                hint = Some(if dy > 0 { LongDirection::Down } else { LongDirection::Up });
                            }
                            session.scroll(dx, dy);
                        }
                    }
                    session.push(hint, &label);
                }
                assert!(
                    session.horizontal_steps >= 3 && session.vertical_steps >= 3,
                    "{label}: both axes must be exercised, got {} horizontal and {} vertical steps",
                    session.horizontal_steps,
                    session.vertical_steps
                );
                session.assert_canvas(&label, 4, 8);
            }
        }

        /// A box that also got *narrower* must keep stitching.
        ///
        /// The descriptor is 48 bins across the frame's width, so a width
        /// change really does invalidate every stored row — but not the pixels
        /// those rows describe, which are still in the canvas. The matcher is
        /// rebuilt from those pixels instead of from the edited frame alone;
        /// without it the capture kept one frame of history, and a scroll-back
        /// taller than the box could never be recovered.
        #[test]
        fn a_narrowed_box_rebuilds_the_matcher_from_the_canvas() {
            let label = "narrowed box, scroll back";
            let mut session = RandomSession::new(7, 1200, 20_000);
            session.x = 150;
            session.y = 5000;
            session.width = 900;
            session.height = 900;
            session.screen_x = 120;
            session.screen_y = 120;
            session.push(Some(LongDirection::Down), &format!("{label}: first frame"));
            for step in 0..12 {
                session.scroll(0, 300);
                session.push(Some(LongDirection::Up), &format!("{label}: up {step}"));
            }
            // Right edge in, bottom edge up: a different width *and* a box
            // shorter than the 300 px the content moves between frames.
            session.resize(0, 0, 700, 260);
            session.push(None, &format!("{label}: narrow"));
            for step in 0..6 {
                session.scroll(0, -300);
                session.push(Some(LongDirection::Down), &format!("{label}: back {step}"));
            }
            session.assert_canvas(label, 0, 1);
        }

        /// A box shrunk below the scroll step must keep stitching.
        ///
        /// The resize used to reset the matcher's canvas. A step taller than the
        /// new box overlaps nothing, so the guard had no position it could even
        /// score, and the anchor — the only stage that can answer a jump — had
        /// no history left to match against. The capture then stalled for good:
        /// every frame pending, the preview frozen. The whole scroll-back is
        /// replayed here because the first accepted step is not the point; every
        /// one of them must land on its true delta.
        #[test]
        fn a_shrunk_box_still_follows_a_scroll_back_longer_than_itself() {
            let label = "shrunk box, scroll back";
            let mut session = RandomSession::new(7, 1200, 20_000);
            session.x = 150;
            session.y = 5000;
            session.width = 900;
            session.height = 900;
            session.screen_x = 120;
            session.screen_y = 120;
            session.push(Some(LongDirection::Down), &format!("{label}: first frame"));
            for step in 0..12 {
                session.scroll(0, 300);
                session.push(Some(LongDirection::Up), &format!("{label}: up {step}"));
            }
            // The bottom edge dragged up: same origin, a box shorter than the
            // 300 px the content moves between frames.
            session.resize(0, 0, 900, 260);
            session.push(None, &format!("{label}: shrink"));
            for step in 0..11 {
                session.scroll(0, -300);
                session.push(Some(LongDirection::Down), &format!("{label}: back {step}"));
            }
            session.assert_canvas(label, 0, 1);
        }
    }
}
