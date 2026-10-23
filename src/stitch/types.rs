use std::time::Duration;

use super::store::{CanvasMemoryReport, CanvasStore};

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
    #[cfg_attr(not(any(test, feature = "stitch-legacy")), allow(dead_code))]
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

/// Rows per tile in the canvas store.
///
/// Fixed rather than per-session: the append path moves one band at a time when
/// the canvas grows, and a band has to be a whole number of rows for a tile to
/// hold whole rows of pixels. 64 rows is where the measured ratio flattens
/// (5.5% lz4 at 64 rows vs 5.3% at 128 and 5.2% at 256, for twice and four times
/// the granularity), so it is the smallest band that does not cost ratio.
pub const CANVAS_TILE_ROWS: u32 = 64;

pub struct StitchedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// The canvas pixels, in row bands, packed when cold.
    ///
    /// Deliberately not a `Vec<u8>`: a long capture keeps every pixel it has
    /// ever stitched for the whole session (16.5 MB on `real-1`, 34.9 MB on
    /// `real-long`), and most of it is never read again — the stitcher touches
    /// the previous frame's rectangle, the overlay displays at most one
    /// screenful, and everything else waits for the final PNG. Tiles that have
    /// gone cold are packed; reading one unpacks it into a bounded working set.
    ///
    /// `StitchedFrame` is `!Clone` for that reason (it used to be), and every
    /// read goes through the store's accessors.
    pub(crate) store: CanvasStore,
    pub current_origin_x: i32,
    pub current_origin_y: i32,
    pub anchor_origin_x: i32,
    pub anchor_origin_y: i32,
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
    pub compose_width: u32,
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
    pub compose_height: u32,
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
    pub active_crop: ComposeCrop,
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
    pub fixed_bands: FixedBands,
    /// Captured horizontal extent of each canvas row.
    ///
    /// An empty *outer* vector means every row is fully covered (the
    /// rectangular case, and the default for a canvas that never went ragged).
    pub row_coverage: Vec<RowCoverage>,
}

impl std::fmt::Debug for StitchedFrame {
    /// Summarises the store rather than dumping pixels: a `Debug` print of a
    /// 12 000-row canvas is not something anyone wants in a test failure.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let report = self.store.report();
        f.debug_struct("StitchedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("origin", &(self.anchor_origin_x, self.anchor_origin_y))
            .field("current", &(self.current_origin_x, self.current_origin_y))
            .field("memory", &report)
            .field("coverage_rows", &self.row_coverage.len())
            .finish()
    }
}

/// The canvas accessors the stitcher and the tests read it through.
///
/// A `StitchedFrame` is deliberately `!Clone`, so these are the only way to
/// look at its pixels: the canvas lives in tiles that may be packed, and every
/// read has to go through the store. Two of them (`copy_region`,
/// `memory_report`) have no caller in the shipped binary — they exist because
/// they are the accessors a caller would reach for, and because the memory
/// report is what the tile budget is documented against — so they are marked
/// rather than deleted.
impl StitchedFrame {
    /// One canvas pixel, or `None` where the canvas never captured anything.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 3]> {
        self.store.pixel(x, y as i32)
    }

    /// Copies one row's RGB bytes into `out` (`stride` bytes).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn copy_row(&self, y: u32, out: &mut [u8]) -> bool {
        self.store.row_into(0, y as i32, out)
    }

    /// Copies a rectangle out, row by row, into `out` at `out_stride`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn copy_region(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        out: &mut [u8],
        out_stride: u32,
    ) -> bool {
        self.store.region_into(x, y as i32, width, height, out, out_stride)
    }

    /// The whole canvas as packed RGB rows, in canvas order.
    ///
    /// For consumers that need every pixel at once — the final PNG encode, the
    /// offline replay tool — and only there: it materialises the canvas that the
    /// store exists to avoid materialising.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn to_rgb_buffer(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.stride as usize * self.height as usize];
        for y in 0..self.height {
            let start = y as usize * self.stride as usize;
            let end = start + self.stride as usize;
            self.copy_row(y, &mut out[start..end]);
        }
        out
    }

    /// Identity of the pixel content, independent of how it is stored.
    ///
    /// Two frames with the same digest hold the same pixels whether one of them
    /// has its bands packed and the other does not, which is what makes it
    /// usable as a "nothing was written" assertion across the store.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pixel_digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let _ = self.store.with_row_digest(&mut hash);
        hash
    }

    /// What the canvas costs right now, in bytes.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn memory_report(&self) -> CanvasMemoryReport {
        self.store.report()
    }
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
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
    PerceptualAccepted,
    #[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
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

/// The axis a placement search runs along.
///
/// Lives beside `SearchDirection` because both belong to the shipped matcher:
/// the direction is what the user hinted, the axis is what that hint *seeds*.
/// The old whole-canvas scan (which used to define this) is feature-gated, and
/// production code must not have to import from a module it does not build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchAxis {
    #[default]
    Vertical,
    Horizontal,
}

impl SearchAxis {
    pub fn other(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}


#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub struct MotionEstimate {
    pub delta_y: i32,
    pub confidence: f64,
    pub matched_groups: usize,
    pub total_candidate_groups: usize,
}
