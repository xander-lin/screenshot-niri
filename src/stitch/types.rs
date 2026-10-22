use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedBands {
    pub top: u32,
    pub bottom: u32,
}

/// Captured horizontal extent of one canvas row.
///
/// Three states rather than a list because they are not equally common and the
/// common ones must not allocate: a rectangular capture is `Full` on every row,
/// and a ragged one is `Full` almost everywhere with `Spans` only where the
/// selection moved. Building a `Vec` per row cost ~13% of an accepted frame on
/// the real corpus (8 500 rows × one allocation each).
///
/// `Spans` exists because a ragged capture can have *disjoint* filled extents in
/// one row — the two legs of an arch, whose space between them was never
/// selected and must stay transparent. A hull of the two would paint that space
/// opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowCoverage {
    /// Nothing was captured in this row.
    Empty,
    /// The whole row is captured.
    Full,
    /// Sorted, disjoint, non-adjacent captured extents `(start_x, end_x)`.
    Spans(Vec<(u32, u32)>),
}

impl RowCoverage {
    /// Collapses a scratch interval list into the cheapest representation.
    pub(super) fn from_spans(spans: &[(u32, u32)], width: u32) -> Self {
        match spans {
            [] => RowCoverage::Empty,
            [(0, end)] if *end >= width => RowCoverage::Full,
            _ => RowCoverage::Spans(spans.to_vec()),
        }
    }

    /// Appends this row's extents shifted by `dx` into `out`, merged with what
    /// `out` already holds and clipped to `clip`.
    ///
    /// `Full` is resolved against `source_width` — the canvas width *when the
    /// row was written*, not the current one. A canvas only grows, so using the
    /// new width would claim the widening strip was captured too: the strip the
    /// user just selected and has not captured yet.
    pub(super) fn push_into(
        &self,
        out: &mut Vec<(u32, u32)>,
        dx: u32,
        source_width: u32,
        clip: u32,
    ) {
        match self {
            RowCoverage::Empty => {}
            RowCoverage::Full => crate::stitch::canvas::merge_interval(
                out,
                dx.min(clip),
                dx.saturating_add(source_width).min(clip),
            ),
            RowCoverage::Spans(spans) => {
                for &(start, end) in spans {
                    crate::stitch::canvas::merge_interval(
                        out,
                        start.saturating_add(dx).min(clip),
                        end.saturating_add(dx).min(clip),
                    );
                }
            }
        }
    }

    /// Whether `x` was captured in this row.
    pub fn contains(&self, x: u32) -> bool {
        match self {
            RowCoverage::Empty => false,
            RowCoverage::Full => true,
            RowCoverage::Spans(spans) => spans.iter().any(|&(start, end)| x >= start && x < end),
        }
    }

    /// Whether the whole span `[start, end)` was captured in this row.
    pub(super) fn covers(&self, start: u32, end: u32) -> bool {
        match self {
            RowCoverage::Empty => false,
            RowCoverage::Full => true,
            // Adjacent intervals are merged, so one entry must cover the span:
            // pieces that merely add up leave a hole between them.
            RowCoverage::Spans(spans) => spans
                .iter()
                .any(|&(lo, hi)| lo <= start && hi >= end),
        }
    }

    /// Number of captured pixels in this row, capped at `width`.
    pub fn captured_width(&self, width: u32) -> u32 {
        match self {
            RowCoverage::Empty => 0,
            RowCoverage::Full => width,
            RowCoverage::Spans(spans) => spans
                .iter()
                .map(|&(start, end)| end.saturating_sub(start))
                .sum(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub current_origin_x: i32,
    pub current_origin_y: i32,
    pub anchor_origin_x: i32,
    pub anchor_origin_y: i32,
    pub compose_width: u32,
    pub compose_height: u32,
    pub active_crop: ComposeCrop,
    pub fixed_bands: FixedBands,
    /// Captured horizontal extent of each canvas row.
    ///
    /// An empty *outer* vector means every row is fully covered (the
    /// rectangular case, and the default for a canvas that never went ragged).
    pub row_coverage: Vec<RowCoverage>,
}

/// On-screen top-left offset (in capture buffer pixels) of the region a frame
/// was captured from. Consecutive frames share an offset while the selection
/// box is unchanged; a changed offset marks a selection-box edit whose frame
/// must be stitched at a known, deterministic canvas position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameScreenOffset {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendDirection {
    Bottom,
    Top,
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameMatch {
    pub direction: AppendDirection,
    pub overlap: u32,
    pub delta_x: i32,
    pub delta_y: i32,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PushStreakKind {
    Duplicate,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Initialized,
    Accepted { match_info: FrameMatch },
    Duplicate,
    /// The frame could not be placed, but the refusal has not repeated long
    /// enough to count as a failure.
    ///
    /// A single unplaceable frame is not evidence that the content stopped
    /// overlapping. A rendering seam, a repaint in flight or a momentarily
    /// low-contrast region can make one frame genuinely ambiguous while the
    /// very next one matches cleanly. Reporting that first frame as a failure
    /// is what makes the border flash red on content that plainly overlaps.
    ///
    /// Like [`PushResult::NoMatch`] this must not advance any accepted state;
    /// it differs only in what it tells the user.
    Pending,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuplicateAnalysisMotion {
    pub changed_top: u32,
    pub changed_bottom: u32,
    pub strongest_y: u32,
    pub strongest_diff: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastMotionCandidate {
    pub delta_y: i32,
    pub score: f64,
    pub overlap_rows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastMotionAgreement {
    NoCandidate,
    HeavyExactDelta,
    HeavySameDirection,
    HeavyDifferentDelta,
    HeavyOppositeDirection,
    FallbackExactDelta,
    FallbackSameDirection,
    FallbackOppositeDirection,
    MissedHeavyAccept,
    MissedFallbackAccept,
    CandidateOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastMotionVerifyPass {
    Top20,
    Top50,
    #[allow(dead_code)]
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastMotionTrace {
    pub candidate: Option<FastMotionCandidate>,
    pub reference_delta_y: Option<i32>,
    pub agreement: FastMotionAgreement,
    pub verify_pass: Option<FastMotionVerifyPass>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StitchDecisionPath {
    Initialized,
    Duplicate,
    CanvasAccepted,
    PerceptualAccepted,
    FallbackAccepted,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StitchProfileBreakdown {
    pub path: Option<StitchDecisionPath>,
    pub duplicate_check: Duration,
    pub perceptual_prepare: Duration,
    pub fixed_bands: Duration,
    pub canvas_match: Duration,
    pub perceptual_match: Duration,
    pub fallback_match: Duration,
    pub apply_match: Duration,
    pub append_frame: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub struct MotionEstimate {
    pub delta_y: i32,
    pub confidence: f64,
    pub matched_groups: usize,
    pub total_candidate_groups: usize,
}
