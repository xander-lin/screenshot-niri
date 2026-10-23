//! Frame placement engines.
//!
//! The stitcher's job is not "find the most similar position" but "know when it
//! cannot know". This module holds that decision behind a small interface so the
//! implementation can be swapped, and ships two of them:
//!
//! - [`WindowedEdgeMatcher`] — the production engine. Compares *where luminance
//!   changes* (horizontal boundaries) instead of luminance itself, searches
//!   three successively wider windows row by row, and refuses to answer when the
//!   runner-up is too close.
//! - [`LegacyPipeline`] — the previous canvas-scan matcher, kept behind the
//!   `stitch-legacy` feature so the two can be compared on identical frames.
//!
//! `ScriptedEngine` (test only) is the mock implementation: it returns a
//! scripted sequence of decisions, which is how the consumers of the interface
//! are tested without any image data.
//!
//! See `docs/edge-stitch-design.md` for the measurements behind every constant
//! here, and `docs/stitch-direction-and-dms.md` for the head-to-head against
//! DMS's stitcher.

#[cfg(test)]
use std::collections::VecDeque;

use super::placement::SearchAxis;
use super::rgb::RgbSource;
use super::{
    minimum_overlap, AppendDirection, FrameMatch, SearchDirection, StitchedFrame, ViewportRect,
};

/// Column bins per row. Measured floor, not a preference: 8 bins leave 42
/// silently misplaced frames on periodic content, 48 leave none.
pub(super) const EDGE_BINS: usize = 48;
/// Fraction of the frame width skipped on each side (scrollbars, chrome).
const EDGE_MARGIN_NUMERATOR: u32 = 5;
const EDGE_MARGIN_DENOMINATOR: u32 = 100;
/// Ladder level thresholds on the per-bin luminance delta.
const EDGE_LEVEL_ONE: f32 = 8.0;
const EDGE_LEVEL_TWO: f32 = 26.0;
/// Rows kept for scoring. Rows with no boundary at all carry no evidence.
const EDGE_MAX_SCORING_ROWS: usize = 192;
/// Comparable boundary rows a frame must show before its cost means anything.
///
/// The gate asks for `min(this, the frame's own boundary rows)`: a frame cannot
/// be asked for more evidence than it contains. That distinction is the whole
/// point. A page whose only feature is the boundary between two flat colours
/// carries exactly one boundary row, so an absolute demand of three makes it
/// unstitchable however unambiguous that row is — while a dense frame that
/// merely overlaps the canvas by a sliver must still show three of its many
/// rows, which is what refuses a jump frame whose single comparable row happens
/// to line up with a canvas boundary.
///
/// Lowering the constant instead is not equivalent, and was measured: at 1,
/// real-1 went from 204 accepted / 0 misplaced to 201 / 6 with 20 displaced
/// bands, because the jump frames above were then accepted.
///
/// Density is not a quality judgement. Whether the rows a frame *can* show are
/// enough to trust is decided by the gates below (winner cost, runner-up ratio,
/// colour), which are normalised and do not reward or punish density.
const EDGE_MIN_COMPARABLE_ROWS: u32 = 3;
/// Absolute ceiling on the winner's cost.
///
/// The cost is already an average over the boundary cells that carried
/// evidence, so it is comparable across contrast levels and page densities: 0
/// means every boundary matched, 1 means every one disagreed. No window enters
/// it, and no typical candidate: this is the winner alone.
///
/// Measured accepted winners: real-1 max 0.0548, lowcontrast-2b max 0.0714 —
/// the low-contrast corpus is the binding case, 12% under the ceiling.
/// real-long reaches 0.168 on blurred frames and passes on the margin escape
/// ([`EDGE_CONFIDENT_SECOND_OVER_BEST`]) instead, 18 of its 97 acceptances.
const EDGE_MAX_ACCEPT_COST: f64 = 0.08;
/// The runner-up must cost at least this many times the winner.
///
/// Two different failures look like "everything scores low":
///
/// * genuinely good content, where the winner is far ahead of the runner-up;
/// * a repeating page, where several offsets tie because the content really
///   does repeat.
///
/// The absolute gate cannot tell them apart (both winners are cheap). The
/// ratio of the two best positions can: a real match stands out by a wide
/// margin, a periodic page has a runner-up at the same cost. A small ratio
/// means the content is ambiguous, and the answer is the *nearest* offset that
/// also passes the colour check — not a refusal, unless none passes it.
///
/// It doubles as the definition of "about as good": the rivals considered in
/// that tie-break are the candidates costing within this ratio of the winner.
const EDGE_MIN_SECOND_OVER_BEST: f64 = 1.5;
/// Mean per-channel colour difference (in 8-bit levels) at which the
/// structural winner counts as showing the same content as the pixel canvas.
///
/// Calibrated from both sides: real content in motion (the blurred frames of
/// real-long) reaches 40.6, while a structural fake on unrelated colourful
/// texture measures 82.2. The threshold sits between them with margin on both
/// sides.
const EDGE_MAX_COLOUR_DIFFERENCE: f64 = 30.0;
/// Margin at which a high absolute cost is forgiven: the winner is this many
/// times better than the runner-up, so the match is unmistakable even on
/// imperfect content.
const EDGE_CONFIDENT_SECOND_OVER_BEST: f64 = 2.5;
/// Positions closer than this to the winner are the same peak, not a rival.
const EDGE_ADJACENT_ROWS: i32 = 2;
/// Divisor that turns the frame height into the winner's basin width.
///
/// A rival closer than this is the winner's own shoulder, not a second answer.
/// One eighth of the viewport is wide enough to cover the peak, and narrow
/// enough that a repeated pattern one card-height away still counts as a rival.
const EDGE_BASIN_DIVISOR: i32 = 8;
/// Impulse window half-width: `max(24, 1.05 x |previous delta|)`.
///
/// The window has to cover the *next* step, and the next step is at least as
/// large as the last one when the page accelerates. Measured on real-1,
/// widening 0.6 -> 1.0 -> 1.05 moved 20 frames out of the guard stage into this
/// cheaper one with identical placements (204 accepts, 0 misplaced, 0
/// displaced bands, drift -257).
///
/// The reason to widen is that the window should contain the next step, not
/// speed: the extra positions cost about what the avoided guard rings save,
/// and the mean time does not move outside run-to-run noise. 1.10 is where it
/// breaks: 205 accepts but 2 frames with displaced bands and drift -677,
/// because a window that wide starts catching jump frames that the guard
/// places better.
const EDGE_IMPULSE_MIN_HALF_WIDTH: i32 = 24;
const EDGE_IMPULSE_SCALE_NUMERATOR: i32 = 21;
const EDGE_IMPULSE_SCALE_DENOMINATOR: i32 = 20;
/// Factor each further anchor ring grows by.
///
/// The anchor walks outward from the last confirmed position one ring at a
/// time, so the nearest explanation is tried before a distant one and the
/// common case never scores the whole canvas. The first ring is one viewport
/// (`rows`), which is derived from the frame rather than configured: it is the
/// smallest distance at which "somewhere else in the canvas" starts.
///
/// The guard needs no ladder at all — its window is `pos ± rows`, a single
/// interval — so rings exist only here.
const EDGE_RING_GROWTH: i32 = 4;

/// A frame reduced to "where does luminance change, per row".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EdgeLadder {
    rows: u32,
    bins: u32,
    /// `rows * bins` levels, 0 / 1 / 2.
    levels: Vec<u8>,
    /// Row indices that contain at least one boundary, ascending.
    scoring_rows: Vec<u32>,
}

impl EdgeLadder {
    /// The gain a session should use, from the first frame committed.
    ///
    /// Low-contrast content (peak below the first level's threshold) needs its
    /// deltas amplified or it produces no boundaries at all; everything else
    /// uses the neutral 1.0. See `from_source` for why this is decided once
    /// per session rather than per frame.
    pub(super) fn session_gain(frame: &dyn RgbSource) -> f32 {
        Self::session_gain_on(frame, SearchAxis::Vertical)
    }

    pub(super) fn session_gain_on(frame: &dyn RgbSource, axis: SearchAxis) -> f32 {
        let peak = Self::peak_delta_on(frame, axis);
        if peak > 0.0 && peak < EDGE_LEVEL_ONE {
            (EDGE_LEVEL_ONE / peak).min(64.0)
        } else {
            1.0
        }
    }

    /// Reads a frame in axis terms: `(along, across)` positions, where `along`
    /// is the scroll axis and `across` is the dimension the bins span.
    fn pixel_on(frame: &dyn RgbSource, axis: SearchAxis, along: u32, across: u32) -> [u8; 3] {
        match axis {
            SearchAxis::Vertical => frame.pixel_rgb(across, along),
            SearchAxis::Horizontal => frame.pixel_rgb(along, across),
        }
    }

    fn extent_on(frame: &dyn RgbSource, axis: SearchAxis) -> (u32, u32) {
        match axis {
            SearchAxis::Vertical => (frame.height(), frame.width()),
            SearchAxis::Horizontal => (frame.width(), frame.height()),
        }
    }

    /// The strongest bin-mean delta along the scroll axis, in luminance levels.
    fn peak_delta_on(frame: &dyn RgbSource, axis: SearchAxis) -> f32 {
        let (along, across) = Self::extent_on(frame, axis);
        if along == 0 || across == 0 {
            return 0.0;
        }
        let margin = across * EDGE_MARGIN_NUMERATOR / EDGE_MARGIN_DENOMINATOR;
        let body_start = margin.min(across);
        let body_end = across.saturating_sub(margin).max(body_start);
        let body_width = body_end - body_start;
        if body_width == 0 {
            return 0.0;
        }
        let bins = EDGE_BINS.min(body_width as usize) as u32;
        let bin_width = body_width / bins;
        let mut previous: Option<Vec<f32>> = None;
        let mut peak = 0f32;
        for position in 0..along {
            let mut row = vec![0f32; bins as usize];
            for b in 0..bins {
                let a0 = body_start + b * bin_width;
                let a1 = if b + 1 == bins { body_end } else { a0 + bin_width };
                let mut sum = 0u32;
                let mut count = 0u32;
                for a in a0..a1 {
                    let [r, g, bl] = Self::pixel_on(frame, axis, position, a);
                    sum += 77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(bl);
                    count += 1;
                }
                row[b as usize] =
                    if count == 0 { 0.0 } else { (sum >> 8) as f32 / count as f32 };
            }
            if let Some(prev) = previous.as_ref() {
                for b in 0..bins as usize {
                    peak = peak.max((row[b] - prev[b]).abs());
                }
            }
            previous = Some(row);
        }
        peak
    }

    /// Builds the ladder for a frame.
    ///
    /// The margin exists so a moving scrollbar never enters the comparison; it
    /// is a fixed fraction, not a detection step.
    pub(super) fn from_source(frame: &dyn RgbSource, gain: f32) -> Option<Self> {
        Self::from_source_on(frame, gain, SearchAxis::Vertical)
    }

    /// Builds the ladder along `axis`.
    ///
    /// The descriptor is the derivative *along the scroll axis*, binned across
    /// the other dimension: rows of `|dL/dy|` for a vertical scroll, columns of
    /// `|dL/dx|` for a horizontal one. Everything downstream — the canvas, the
    /// windows, the gates — addresses "positions along the axis" and needs no
    /// knowledge of which way that is.
    pub(super) fn from_source_on(
        frame: &dyn RgbSource,
        gain: f32,
        axis: SearchAxis,
    ) -> Option<Self> {
        let (along, across) = Self::extent_on(frame, axis);
        if along == 0 || across == 0 {
            return None;
        }

        let margin = across * EDGE_MARGIN_NUMERATOR / EDGE_MARGIN_DENOMINATOR;
        let body_start = margin.min(across);
        let body_end = across.saturating_sub(margin).max(body_start);
        let body_width = body_end - body_start;
        if body_width == 0 {
            return None;
        }
        let bins = EDGE_BINS.min(body_width as usize) as u32;
        let bin_width = body_width / bins;
        let bins_usize = bins as usize;

        // Per-position, per-bin luminance means.
        let mut means = vec![0f32; along as usize * bins_usize];
        for position in 0..along {
            for b in 0..bins {
                let a0 = body_start + b * bin_width;
                let a1 = if b + 1 == bins { body_end } else { a0 + bin_width };
                let mut sum = 0u32;
                let mut count = 0u32;
                for a in a0..a1 {
                    let [r, g, bl] = Self::pixel_on(frame, axis, position, a);
                    sum += 77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(bl);
                    count += 1;
                }
                let mean = if count == 0 { 0.0 } else { (sum >> 8) as f32 / count as f32 };
                means[position as usize * bins_usize + b as usize] = mean;
            }
        }

        // Contrast-relative quantisation: see gain below.
        // The gain comes from the *session*, not from this frame. It exists so
        // low-contrast content (a two-gray page, delta 2, below the first
        // level's threshold) still produces boundaries. Deriving it per frame
        // made the two sides of a comparison disagree about unchanged content:
        // the canvas rows were written by an earlier frame whose peak — and
        // therefore gain — differed, so the same boundary landed on different
        // levels on the two sides. One gain for the whole session keeps every
        // ladder on the scale the canvas was built with; a boundary stronger
        // than the scale simply saturates at level 2, which compares fine.

        // Difference between consecutive positions along the axis.
        let mut levels = vec![0u8; along as usize * bins_usize];
        let mut row_mass = vec![0u16; along as usize];
        for position in 0..along as usize {
            for b in 0..bins_usize {
                let index = position * bins_usize + b;
                let current = means[index];
                let previous = if position == 0 {
                    current
                } else {
                    means[index - bins_usize]
                };
                let delta = (current - previous).abs() * gain;
                let level = if delta >= EDGE_LEVEL_TWO {
                    2
                } else if delta >= EDGE_LEVEL_ONE {
                    1
                } else {
                    0
                };
                levels[index] = level;
                if level > 0 {
                    row_mass[position] += 1;
                }
            }
        }

        // Keep the rows that actually carry boundaries, strongest first, then
        // restore ascending order so scoring walks memory forwards.
        let mut order: Vec<u32> = (0..along).filter(|p| row_mass[*p as usize] > 0).collect();
        order.sort_by(|a, b| {
            row_mass[*b as usize]
                .cmp(&row_mass[*a as usize])
                .then_with(|| a.cmp(b))
        });
        order.truncate(EDGE_MAX_SCORING_ROWS);
        order.sort_unstable();

        Some(Self {
            rows: along,
            bins,
            levels,
            scoring_rows: order,
        })
    }

    pub(super) fn rows(&self) -> u32 {
        self.rows
    }

    pub(super) fn bins(&self) -> u32 {
        self.bins
    }

    /// The descriptor as a grayscale image: `rows` tall, `bins` wide, one
    /// byte per cell (0, 85 or 170 for levels 0, 1, 2).
    ///
    /// Exists so a human can look at what the matcher actually sees and compare
    /// it against the frame. When a placement is wrong, the picture answers
    /// "did the descriptor lose the evidence, or did the decision discard it?"
    pub fn as_grayscale(&self) -> (u32, u32, Vec<u8>) {
        let scale = [0u8, 85, 170];
        let data = self
            .levels
            .iter()
            .map(|level| scale[(*level as usize).min(2)])
            .collect();
        (self.bins, self.rows, data)
    }

    /// Rows that actually participate in scoring.
    pub fn scoring_rows(&self) -> &[u32] {
        &self.scoring_rows
    }

    fn row_levels(&self, row: u32) -> &[u8] {
        let start = row as usize * self.bins as usize;
        &self.levels[start..start + self.bins as usize]
    }
}

/// Canvas-side ladder storage in the same coordinate space as [`StitchedFrame`].
///
/// Only `place` reads it; `commit` writes it after the pixel canvas has already
/// accepted the frame, so the two canvases cannot disagree about which rows are
/// verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EdgeCanvas {
    bins: u32,
    levels: Vec<u8>,
    filled: Vec<bool>,
    /// Canvas row of `filled[0]`.
    origin_row: i32,
    filled_lo: i32,
    filled_hi: i32,
}

impl EdgeCanvas {
    fn new(bins: u32, origin_row: i32) -> Self {
        Self {
            bins,
            levels: Vec::new(),
            filled: Vec::new(),
            origin_row,
            filled_lo: i32::MAX,
            filled_hi: i32::MIN,
        }
    }

    fn is_empty(&self) -> bool {
        self.filled_lo > self.filled_hi
    }

    /// Shifts every stored row forward by `shift`, used when the pixel canvas
    /// prepends rows (or columns, on the horizontal axis) because a frame
    /// extended it upward (or leftward).
    fn shift_forward(&mut self, shift: i32) {
        if shift <= 0 || self.is_empty() {
            return;
        }
        let shift = shift as usize;
        let bins = self.bins as usize;
        let mut levels = vec![0u8; (self.filled.len() + shift) * bins];
        levels[shift * bins..].copy_from_slice(&self.levels);
        let mut filled = vec![false; self.filled.len() + shift];
        filled[shift..].copy_from_slice(&self.filled);
        self.levels = levels;
        self.filled = filled;
        // `origin_row` is the logical row of buffer index 0, so prepending
        // moves it up by `shift`. The *covered logical extent* does not move:
        // the content stayed where it was, only its buffer indices grew.
        // Incrementing `filled_lo`/`filled_hi` here as well would claim the
        // canvas starts `shift` rows lower than it does, and the next `write`
        // would then land inside rows it wrongly believes are already filled —
        // `write` skips those, so the new frame's content would never enter the
        // canvas at all.
        self.origin_row -= shift as i32;
    }

    /// Writes `ladder` with its top at canvas row `top`, preserving rows that
    /// were already verified by an earlier frame.
    ///
    /// The canvas is indexed in *logical* rows and only ever grows, so a frame
    /// that sits above everything stored so far grows the front here. Nothing
    /// outside the engine has to tell it about a prepend: the pixel canvas
    /// re-basing itself and the engine's logical space are two different
    /// coordinate systems, and the shift between them is not the engine's
    /// business.
    fn write(&mut self, top: i32, ladder: &EdgeLadder) {
        if ladder.bins != self.bins {
            return;
        }
        let bins = self.bins as usize;
        if top < self.origin_row {
            self.shift_forward(self.origin_row - top);
        }
        let start = top - self.origin_row;
        let needed = (start + ladder.rows as i32).max(self.filled.len() as i32).max(0) as usize;
        if needed > self.filled.len() {
            self.levels.resize(needed * bins, 0);
            self.filled.resize(needed, false);
        }
        for row in 0..ladder.rows as usize {
            let index = start + row as i32;
            if index < 0 {
                continue;
            }
            let index = index as usize;
            if self.filled[index] {
                continue;
            }
            let dst = index * bins;
            self.levels[dst..dst + bins].copy_from_slice(ladder.row_levels(row as u32));
            self.filled[index] = true;
            let logical = self.origin_row + index as i32;
            self.filled_lo = self.filled_lo.min(logical);
            self.filled_hi = self.filled_hi.max(logical);
        }
    }

    fn row_filled(&self, row: i32) -> bool {
        let index = row - self.origin_row;
        index >= 0 && (index as usize) < self.filled.len() && self.filled[index as usize]
    }

    /// Cheap geometric overlap: how many of the frame's rows land on verified
    /// canvas rows. O(1), from the filled extent.
    fn overlap_rows(&self, top: i32, frame_rows: u32) -> u32 {
        if self.is_empty() {
            return 0;
        }
        let lo = top.max(self.filled_lo);
        let hi = (top + frame_rows as i32).min(self.filled_hi + 1);
        (hi - lo).max(0) as u32
    }

    /// Cost of placing `ladder` with its top at `top`.
    ///
    /// Returns `None` when there is not enough evidence to compare: too few
    /// geometric rows, or too few rows that actually carry boundaries. Both are
    /// preconditions, not quality judgements — a sparse page that still has a
    /// handful of boundaries scores normally.
    fn cost_at(&self, ladder: &EdgeLadder, top: i32, min_overlap: u32) -> Option<Score> {
        if ladder.bins != self.bins {
            return None;
        }
        if self.overlap_rows(top, ladder.rows) < min_overlap {
            return None;
        }
        let bins = self.bins as usize;
        let mut difference = 0u32;
        let mut denominator = 0u32;
        let mut used = 0u32;
        for &row in &ladder.scoring_rows {
            let canvas_row = top + row as i32;
            if !self.row_filled(canvas_row) {
                continue;
            }
            let offset = (canvas_row - self.origin_row) as usize * bins;
            let canvas_levels = &self.levels[offset..offset + bins];
            let frame_levels = ladder.row_levels(row);
            for b in 0..bins {
                let a = canvas_levels[b];
                let f = frame_levels[b];
                difference += u32::from(a.abs_diff(f));
                if a > 0 {
                    denominator += 1;
                }
                if f > 0 {
                    denominator += 1;
                }
            }
            used += 1;
        }
        // A frame cannot be asked for more boundary rows than it has. A page
        // whose only feature is the boundary between two flat colours offers
        // exactly one, and demanding three makes it unstitchable however
        // unambiguous that row is; a dense frame that merely overlaps the canvas
        // by a sliver still has to show three of its many rows, which is what
        // refuses jump frames whose only comparable row happens to line up.
        let required = EDGE_MIN_COMPARABLE_ROWS.min(ladder.scoring_rows.len() as u32);
        if used < required || denominator == 0 {
            return None;
        }
        Some(Score {
            cost: f64::from(difference) / f64::from(denominator) / 2.0,
            used,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Score {
    pub cost: f64,
    pub used: u32,
}

/// What the engine decided about one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum EngineDecision {
    /// Place the frame at `rect` (canvas coordinates).
    Place { rect: ViewportRect, match_info: FrameMatch },
    /// Refuse. The caller must not move any accepted state.
    Reject { reason: RejectReason },
}

/// Why the engine refused. Kept separate from "no candidate" because the UI
/// needs to tell the user *what* went wrong, and the trace needs it to compare
/// engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The engine has no canvas yet; the caller owns initialisation.
    Uninitialised,
    /// The frame does not look like the canvas anywhere.
    NoCandidate,
    /// Several offsets fit equally well and none survived the colour check, so
    /// the content cannot say which one is right.
    Ambiguous,
    /// Too little comparable evidence to judge.
    InsufficientEvidence,
    /// The frame has no boundary at all (uniform content).
    NoBoundaries,
    /// The request was not well formed (size mismatch, overflow).
    Invalid,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialised => "uninitialised",
            Self::NoCandidate => "no-candidate",
            Self::Ambiguous => "ambiguous",
            Self::InsufficientEvidence => "insufficient-evidence",
            Self::NoBoundaries => "no-boundaries",
            Self::Invalid => "invalid",
        }
    }
}

/// Costs already computed during one `place` call.
///
/// The impulse window, the guard band and the anchor band overlap, and a
/// position's cost depends only on the ladder and the canvas, both of which are
/// fixed for the duration of the call. Re-checking a position is fine — each
/// stage applies the gates to its own candidate set, and that is what decides
/// the answer — but re-*computing* it is wasted work: one position costs about
/// 9k boundary-cell comparisons. Measured reuse: 9.9% of lookups on real-1,
/// 8.9% on real-long, 7.0% on lowcontrast-2b.
///
/// Keyed by `top` alone, which is sound only because the ladder and the canvas
/// are fixed for the call; the cache must not outlive `place`.
#[derive(Default)]
struct ScoreCache {
    entries: std::collections::HashMap<i32, Option<Score>>,
}

impl ScoreCache {
    fn cost(
        &mut self,
        canvas: &EdgeCanvas,
        ladder: &EdgeLadder,
        top: i32,
        min_overlap: u32,
    ) -> Option<Score> {
        if let Some(&cached) = self.entries.get(&top) {
            return cached;
        }
        let score = canvas.cost_at(ladder, top, min_overlap);
        self.entries.insert(top, score);
        score
    }
}

/// Everything an engine may look at for one frame.
///
/// `frame` is a `&dyn RgbSource` rather than a generic so the trait stays
/// object safe (engines are boxed). The one caller that needs a concrete type
/// (the legacy engine, which takes `impl RgbSource`) goes through
/// [`DynRgbSource`].
pub(super) struct PlacementRequest<'a> {
    pub stitched: &'a StitchedFrame,
    pub previous_rect: ViewportRect,
    pub frame: &'a dyn RgbSource,
    pub search_direction: Option<SearchDirection>,
    /// Consecutive refusals before this frame. The anchor stage is gated on
    /// this: it exists to recover from a red border, not to run on every frame
    /// whose immediate neighbourhood did not match.
    pub refusals: u32,
}

/// Object-safe facade over `RgbSource`, so a boxed engine can still hand the
/// frame to generic code.
pub(super) struct DynRgbSource<'a>(pub &'a dyn RgbSource);

impl RgbSource for DynRgbSource<'_> {
    fn width(&self) -> u32 {
        self.0.width()
    }

    fn height(&self) -> u32 {
        self.0.height()
    }

    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        self.0.pixel_rgb(x, y)
    }
}

/// A replaceable placement strategy.
///
/// `place` is pure: it must not mutate the canvas. `commit` is called only after
/// the caller has written the pixel canvas, so an engine can never be ahead of
/// the canvas it describes. `reset` returns the engine to its just-constructed
/// state (selection-box edits do this).
pub(super) trait PlacementEngine {
    fn name(&self) -> &'static str;
    fn place(&mut self, request: &PlacementRequest<'_>) -> EngineDecision;
    /// Records an accepted frame. `top` is the canvas row the engine asked for
    /// (possibly negative); `rect` is where the pixel canvas actually put it
    /// after any prepend.
    fn commit(&mut self, rect: ViewportRect, requested_top: i32, frame: &dyn RgbSource);
    fn reset(&mut self);

    /// Descriptor of the frame most recently passed to `place`, as a grayscale
    /// image `(width, height, data)`; `None` when the engine has none.
    ///
    /// Debug aid: it lets a human compare what the matcher sees against the
    /// frame itself.
    fn last_descriptor(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

/// A rectangular view of the pixel canvas as an [`RgbSource`].
///
/// Exists so the horizontal matcher can build a descriptor from the pixels the
/// capture already holds, instead of maintaining a second descriptor canvas
/// through the whole session. The rectangle is chosen by the caller and is
/// inside the canvas by construction.
pub(super) struct CanvasRegion<'a> {
    canvas: &'a StitchedFrame,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl<'a> CanvasRegion<'a> {
    fn new(canvas: &'a StitchedFrame, x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            canvas,
            x,
            y,
            width,
            height,
        }
    }
}

impl RgbSource for CanvasRegion<'_> {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pixel_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let canvas_x = self.x + x;
        let canvas_y = self.y + y;
        let offset =
            canvas_y as usize * self.canvas.stride as usize + canvas_x as usize * 3;
        [
            self.canvas.data[offset],
            self.canvas.data[offset + 1],
            self.canvas.data[offset + 2],
        ]
    }
}

// ---------------------------------------------------------------------------
// Production engine: boundaries + impulse windows + explicit refusal
// ---------------------------------------------------------------------------

/// The engine described in `docs/edge-stitch-design.md` §5.
#[derive(Debug, Default)]
pub(super) struct WindowedEdgeMatcher {
    /// Which way this matcher's canvas is indexed. Every position it stores,
    /// every window it searches and every delta it reports is measured along
    /// this axis; the descriptor is built along it too. Two instances (one per
    /// axis) is how "the same logic on both axes" is spelled without a
    /// transpose layer in between.
    axis: SearchAxis,
    canvas: Option<EdgeCanvas>,
    /// Canvas row of the last accepted frame; the search centre for prediction.
    pos: Option<i32>,
    /// Canvas row of the last *verified* position. Frozen while refusing, which
    /// is what lets the user scroll back and recover.
    anchor: Option<i32>,
    /// Last accepted displacement, and its trend. Both frozen while refusing.
    last_delta: Option<i32>,
    trend: f64,
    /// Rows per frame, for the overlap gate.
    frame_rows: u32,
    /// Row the last `Place` asked for, consumed by `commit`.
    pending_top: Option<i32>,
    /// Consecutive refusals since the last accepted frame.
    ///
    /// The displacement guard exists to stop a single bad match from jumping
    /// the canvas across the page. Once the engine has already refused, that
    /// guard has nothing left to protect: the canvas is frozen at the last
    /// verified row while the content keeps scrolling, so every further frame
    /// is legitimately further away. Without widening, one blurred frame
    /// (a scroll animation between two wheel steps) freezes the capture
    /// permanently.
    /// The ladder built by the most recent `place`, reused by `commit`.
    ///
    /// Building it costs about half of the per-frame time, and `place` is
    /// always followed by `commit` for the same frame, so rebuilding it there
    /// would double the descriptor cost for no information.
    pending_ladder: Option<EdgeLadder>,
    /// The ladder of the frame most recently committed, kept for the debug
    /// descriptor dump (`pending_ladder` is consumed by `commit`).
    last_committed_ladder: Option<EdgeLadder>,
    /// The one quantisation gain for the whole session, set when the canvas is
    /// initialised. Every ladder is built with it, so a frame and the canvas
    /// rows it is compared against always share one scale.
    session_gain: f32,
}

impl WindowedEdgeMatcher {
    pub(super) fn new(axis: SearchAxis) -> Self {
        Self {
            axis,
            ..Self::default()
        }
    }

    /// Scores an annulus around `centre`: positions with `inner < |top - centre|
    /// <= outer`, clipped to `bounds` and skipping `exclude`.
    ///
    /// A ring is how a ladder avoids scoring a position twice. A plain wider
    /// window re-examines everything the narrower one already rejected, which
    /// is wasted work and — because the narrower window is the better-informed
    /// one — cannot change the answer. `inner == 0` covers the centre itself.
    ///
    /// The guard calls this once with `inner = 0` and `outer = rows`; only the
    /// anchor walks a ladder.
    #[allow(clippy::too_many_arguments)]
    fn decide_ring(
        &self,
        cache: &mut ScoreCache,
        ladder: &EdgeLadder,
        centre: i32,
        inner: i32,
        outer: i32,
        bounds: (i32, i32),
        exclude: &[(i32, i32)],
        request: &PlacementRequest<'_>,
    ) -> DecideOutcome {
        let lo = (centre - outer).max(bounds.0);
        let hi = (centre + outer).min(bounds.1);
        let mut costs: Vec<(f64, i32, u32)> = Vec::new();
        let Some(canvas) = self.canvas.as_ref() else {
            return DecideOutcome::Reject(RejectReason::Uninitialised);
        };
        if lo > hi {
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }
        let min_overlap = minimum_overlap(ladder.rows(), ladder.rows()).max(1);
        for top in lo..=hi {
            // `inner == 0` means "no hole": the ring includes the centre, so
            // the first ring can answer "the content did not move".
            if inner > 0 && (top - centre).abs() <= inner {
                continue;
            }
            if exclude.iter().any(|&(a, b)| top >= a && top <= b) {
                continue;
            }
            if let Some(score) = cache.cost(canvas, ladder, top, min_overlap) {
                costs.push((score.cost, top, score.used));
            }
        }
        self.decide_from_costs(costs, centre, request)
    }

    /// Scans `lo..=hi` row by row and applies the gates.
    ///
    /// Deliberately exhaustive inside the window: the correct offset is a
    /// one-row-wide basin (a one-row shift multiplies the cost ~47x), so any
    /// coarse step can step over it.
    fn decide(
        &self,
        cache: &mut ScoreCache,
        ladder: &EdgeLadder,
        lo: i32,
        hi: i32,
        centre: i32,
        request: &PlacementRequest<'_>,
    ) -> DecideOutcome {
        let Some(canvas) = self.canvas.as_ref() else {
            return DecideOutcome::Reject(RejectReason::Uninitialised);
        };
        if lo > hi {
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }
        let min_overlap = minimum_overlap(ladder.rows(), ladder.rows()).max(1);

        let mut costs: Vec<(f64, i32, u32)> = Vec::new();
        for top in lo..=hi {
            if let Some(score) = cache.cost(canvas, ladder, top, min_overlap) {
                costs.push((score.cost, top, score.used));
            }
        }
        // `centre` is the best available "where should this be" estimate for
        // this pass: the impulse window centres on the prediction, the
        // horizontal window on the previous frame (ties then prefer the
        // smallest step, which keeps the canvas continuous).
        self.decide_from_costs(costs, centre, request)
    }

    /// The gate decision, given the scored candidates for one window.
    ///
    /// `centre` is what "nearest" means when several offsets tie: the search
    /// that produced these candidates is centred there, so the closest tied
    /// answer to it is the one that keeps the canvas continuous.
    fn decide_from_costs(
        &self,
        mut costs: Vec<(f64, i32, u32)>,
        centre: i32,
        request: &PlacementRequest<'_>,
    ) -> DecideOutcome {
        if costs.len() < 4 {
            return DecideOutcome::Reject(RejectReason::InsufficientEvidence);
        }

        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (best_cost, best_top, _) = costs[0];
        // The runner-up is the cheapest candidate outside the winner's basin.
        //
        // Skipping only `EDGE_ADJACENT_ROWS` rows finds the shoulder of the
        // winner's own peak, not a rival: on a smooth cost surface the next few
        // rows are always a little worse, which made `sep` look healthy even
        // when a second, equally good answer existed. The basin is therefore
        // widened, so a candidate one period away (a 235-row card grid) counts
        // as the rival it actually is.
        let basin = self.basin_rows();
        let second = costs
            .iter()
            .find(|(_, top, _)| (top - best_top).abs() > basin)
            .map(|(cost, _, _)| *cost);
        // The other places that match about as well as the winner.
        //
        // "About as well" is the same relative test as the runner-up ratio —
        // cost within `EDGE_MIN_SECOND_OVER_BEST` of the winner — so one
        // definition decides both whether the frame is ambiguous and which
        // positions compete for it. An absolute margin here used to count a
        // position 20x worse than a near-perfect winner as an equal answer.
        //
        // Periodic content is why this exists: a card grid repeats every 235
        // rows inside a 700-row viewport, so several offsets score the same,
        // and the ratio alone cannot say which one the scroll meant. The
        // offsets are collected here and the tie is broken on pixels below.
        let mut rival_tops: Vec<i32> = Vec::new();
        for (cost, top, _) in &costs {
            if *cost > best_cost * EDGE_MIN_SECOND_OVER_BEST {
                break;
            }
            if (top - best_top).abs() <= basin {
                continue;
            }
            if rival_tops.iter().any(|kept| (kept - top).abs() <= basin) {
                continue;
            }
            rival_tops.push(*top);
        }
        // How far ahead the winner is of the next distinct answer.
        //
        // This is what separates "the winner is genuinely the one" from
        // "several offsets fit equally well". It is a property of the two best
        // positions, so it does not move with the search window.
        // `None` means there is no candidate outside the winner's basin — in
        // the narrow impulse window that usually means the window was too
        // narrow to contain one, not that the winner is unopposed. It must not
        // read as an infinite margin, or the absolute gate is bypassed and a
        // 0.71-cost "winner" gets written.
        let second_over_best = match second {
            // A perfect winner (cost exactly 0) is as confident as it gets.
            Some(second) if second > 0.0 && best_cost <= 0.0 => f64::INFINITY,
            Some(second) if best_cost > 0.0 => second / best_cost,
            // A rival scoring exactly zero: two identical answers exist.
            Some(_) => 1.0,
            None => 0.0,
        };

        if crate::trace::edge_gate_trace_enabled() {
            crate::trace_log!(
                "stitch: edge gate window={}..={} best={best_cost:.4}@{best_top} \
                 second={second:?} rivals={} second_over_best={second_over_best:.2} \
                 max_accept={EDGE_MAX_ACCEPT_COST}",
                costs.iter().map(|c| c.1).min().unwrap_or(0),
                costs.iter().map(|c| c.1).max().unwrap_or(0),
                rival_tops.len()
            );
        }

        // The only absolute gate: how good the winner is on its own, with a
        // margin-based escape.
        //
        // `median` is gone. It is the typical cost across whatever window was
        // searched, so it moves when the window moves, and a decision that
        // flips with the search radius is not measuring the content. The
        // winner's own cost is a property of the frame and the canvas alone.
        //
        // Motion blur genuinely weakens boundaries, so a real match on moving
        // content can cost more than clean content ever does while still being
        // unmistakable: the runner-up is far worse. Refusing those drops frames
        // that are plainly placeable. The escape requires an *actual* runner-up
        // with a wide margin — `second_over_best` is 0 when there is none, so a
        // window with nothing to compare against cannot use it.
        //
        // This runs before the tie-break because it is cheap and decides about
        // the winner alone; the tie-break below would otherwise sample colour
        // for frames this gate rejects regardless.
        let confident = second_over_best >= EDGE_CONFIDENT_SECOND_OVER_BEST;
        if best_cost > EDGE_MAX_ACCEPT_COST && !confident {
            if crate::trace::edge_gate_trace_enabled() {
                crate::trace_log!(
                    "stitch: winner {best_cost:.4}@{best_top} over absolute cost \
                     {EDGE_MAX_ACCEPT_COST} and only {second_over_best:.2}x ahead"
                );
            }
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }

        // A small ratio means the content cannot say which offset is right.
        // That is not a reason to refuse: the user scrolled a little, not a
        // page, so the nearest candidate keeps the canvas continuous and is
        // exactly as well supported by the pixels as any other.
        //
        // The ladder cannot tell them apart — it is colour-blind by design —
        // so the tie is broken on the actual pixels: a candidate survives if
        // its colour difference against the pixel canvas is acceptable. If
        // more than one survives, the nearest wins; if none does, the
        // structural tie was a coincidence and the frame is refused.
        let ambiguous = second_over_best < EDGE_MIN_SECOND_OVER_BEST;
        let chosen = if ambiguous {
            let target = centre;
            let candidates: Vec<i32> =
                std::iter::once(best_top).chain(rival_tops.iter().copied()).collect();
            let survivors: Vec<i32> = candidates
                .iter()
                .copied()
                .filter(|&top| {
                    self.colour_cost_at(request, top)
                        .is_some_and(|colour| colour <= EDGE_MAX_COLOUR_DIFFERENCE)
                })
                .collect();
            let Some(&first) = survivors.first() else {
                if crate::trace::edge_gate_trace_enabled() {
                    crate::trace_log!(
                        "stitch: ambiguous (second/best={second_over_best:.2}) but no \
                         candidate passes the colour check"
                    );
                }
                return DecideOutcome::Reject(RejectReason::Ambiguous);
            };
            let pick = survivors.iter().fold(first, |best, &top| {
                if (top - target).abs() < (best - target).abs() {
                    top
                } else {
                    best
                }
            });
            if crate::trace::edge_gate_trace_enabled() {
                crate::trace_log!(
                    "stitch: ambiguous (second/best={second_over_best:.2}), {} of {} \
                     candidates pass the colour check, choosing nearest: {pick} (target {target})",
                    survivors.len(),
                    candidates.len()
                );
            }
            pick
        } else {
            best_top
        };

        // Every acceptance is verified against the real pixels, not only the
        // ties. The ladder is colour-blind, and that blindness can be exploited
        // by content whose *structure* repeats with different colours:
        // measured, a fabricated canvas produced a confident-looking
        // structural match (0.073 with a 1.82x runner-up) whose pixels matched
        // nothing. The confident fake and the genuine tie are the same failure
        // wearing two different ratios.
        // `None` means the two do not overlap in the pixel canvas, so there is
        // nothing to compare: the placement rests on structure alone. That is
        // acceptable for a confident winner, and it is not for a tie — the
        // tie-break above requires positive colour evidence, which is why it
        // drops unverifiable candidates instead of keeping them.
        match self.colour_cost_at(request, chosen) {
            Some(colour) if colour > EDGE_MAX_COLOUR_DIFFERENCE => {
                if crate::trace::edge_gate_trace_enabled() {
                    crate::trace_log!(
                        "stitch: placement {chosen} rejected on colour: mean \
                         difference {colour:.1}"
                    );
                }
                return DecideOutcome::Reject(RejectReason::NoCandidate);
            }
            None => {
                if crate::trace::edge_gate_trace_enabled() {
                    crate::trace_log!(
                        "stitch: placement {chosen} not colour-verified (no overlapping \
                         canvas rows)"
                    );
                }
            }
            Some(_) => {}
        }

        // Report the cost of the position actually accepted: the tie-break may
        // have picked a rival, and `best_cost` belongs to a different offset.
        let chosen_cost = costs
            .iter()
            .find(|(_, top, _)| *top == chosen)
            .map_or(best_cost, |(cost, _, _)| *cost);
        DecideOutcome::Accept { top: chosen, cost: chosen_cost }
    }

    /// Mean absolute colour difference between the frame and the pixel canvas
    /// at a candidate offset, sampled on a grid.
    ///
    /// The boundary ladder is deliberately blind to colour and absolute
    /// brightness — that is what makes it robust. The blindness has a cost:
    /// content whose *structure* repeats with different colours ties in the
    /// ladder, and the ladder alone cannot break the tie. This check can, by
    /// reading the actual pixels both sides are made of.
    fn colour_cost_at(
        &self,
        request: &PlacementRequest<'_>,
        top: i32,
    ) -> Option<f64> {
        let stitched = request.stitched;
        let frame = request.frame;
        // Everything below is expressed along the engine's own axis. The
        // anchor is the previous frame's pixel coordinate on that axis plus the
        // difference the engine decided on — both halves come from the same
        // `commit`, so the pair is consistent by construction. Using their
        // difference keeps the formula independent of how either coordinate
        // space is anchored (a geometry change re-seeds the engine's logical
        // space, a prepend re-bases the pixel buffer, and neither disturbs the
        // difference).
        let engine_pos = self.pos?;
        let (previous_along, previous_across) = match self.axis {
            SearchAxis::Vertical => (request.previous_rect.y, request.previous_rect.x),
            SearchAxis::Horizontal => (request.previous_rect.x, request.previous_rect.y),
        };
        let (along, across) = match self.axis {
            SearchAxis::Vertical => (frame.height(), frame.width()),
            SearchAxis::Horizontal => (frame.width(), frame.height()),
        };
        let canvas_anchor = previous_along + (top - engine_pos);
        let mut total = 0u64;
        let mut samples = 0u64;
        let step_along = (along / 12).max(1);
        let step_across = (across / 8).max(1);
        for a in (0..along).step_by(step_along as usize) {
            // `stitched` rows are addressed directly by canvas coordinate:
            // when the canvas grows upward the stitcher rebases the data *and*
            // the viewport rects together, so the anchor already lands on the
            // right row without an origin adjustment. (Subtracting an origin
            // here measurably broke real-1, whose canvas prepends throughout.)
            let canvas_along = canvas_anchor + a as i32;
            if canvas_along < 0
                || canvas_along
                    >= match self.axis {
                        SearchAxis::Vertical => stitched.height as i32,
                        SearchAxis::Horizontal => stitched.width as i32,
                    }
            {
                continue;
            }
            for c in (0..across).step_by(step_across as usize) {
                // The frame's edge on the other axis sits at the previous
                // frame's position: a vertical scroll does not move sideways.
                let canvas_across = previous_across + c as i32;
                let (frame_x, frame_y, canvas_x, canvas_y) = match self.axis {
                    SearchAxis::Vertical => (c, a, canvas_across, canvas_along),
                    SearchAxis::Horizontal => (a, c, canvas_along, canvas_across),
                };
                if canvas_x < 0
                    || canvas_x >= stitched.width as i32
                    || canvas_y < 0
                    || canvas_y >= stitched.height as i32
                {
                    continue;
                }
                // Compare only pixels the canvas actually captured. A ragged
                // canvas has never-captured regions inside its buffer — the
                // space between the legs of an arch, the strip a wider
                // selection has not reached yet — and those bytes are zero.
                // Reading them as content reports a colour difference of the
                // frame's own brightness against black and refuses a placement
                // that is in fact correct. The rule is the same one the output
                // uses to decide transparency.
                if let Some(coverage) = stitched.row_coverage.get(canvas_y as usize) {
                    if !coverage.contains(canvas_x as u32) {
                        continue;
                    }
                }
                let f = frame.pixel_rgb(frame_x, frame_y);
                let so = canvas_y as usize * stitched.stride as usize
                    + canvas_x as usize * 3;
                if so + 2 >= stitched.data.len() {
                    continue;
                }
                let c = [
                    stitched.data[so],
                    stitched.data[so + 1],
                    stitched.data[so + 2],
                ];
                total += u64::from(f[0].abs_diff(c[0]))
                    + u64::from(f[1].abs_diff(c[1]))
                    + u64::from(f[2].abs_diff(c[2]));
                samples += 3;
            }
        }
        if samples == 0 {
            return None;
        }
        Some(total as f64 / samples as f64)
    }

    fn basin_rows(&self) -> i32 {
        // A peak is a few rows wide (a one-row shift multiplies the cost ~47x),
        // so anything inside this is the same answer. Scaled with the frame so
        // a large viewport does not treat its own peak as a rival.
        (self.frame_rows as i32 / EDGE_BASIN_DIVISOR).max(EDGE_ADJACENT_ROWS)
    }

    /// How far the frame may sit from the previous frame and still *overlap* it.
    ///
    /// This is the guard window, and it is the frame's own height: at
    /// `|d| = rows` the two frames just touch, at `|d| = rows - 1` they share a
    /// single row. Both directions are included, so a capture that scrolls back
    /// up gets the same window as one that scrolls down.
    ///
    /// It is deliberately *not* `rows - minimum_overlap`. Overlap and evidence
    /// are different questions: whether the frame continues the previous one is
    /// geometry, while whether the overlap carries enough boundaries to trust is
    /// a quality gate applied later, to the candidate's actual score. Folding
    /// the second into the first made the window a constant of the gate rather
    /// than of the frames, and it hid positions that do overlap.
    fn adjacency_radius(&self, ladder: &EdgeLadder) -> i32 {
        ladder.rows() as i32
    }

    /// Every canvas row where the frame still shares `minimum_overlap` rows
    /// with content the canvas holds — anywhere, not just next to the previous
    /// frame. This is the anchor window's reach.
    fn canvas_bounds(&self, ladder: &EdgeLadder) -> Option<(i32, i32)> {
        let canvas = self.canvas.as_ref()?;
        if canvas.is_empty() {
            return None;
        }
        let rows = ladder.rows() as i32;
        let min_overlap = minimum_overlap(ladder.rows(), ladder.rows()).max(1) as i32;
        let lo = canvas.filled_lo - rows + min_overlap;
        let hi = canvas.filled_hi + 1 - min_overlap;
        (lo <= hi).then_some((lo, hi))
    }

    /// Scores rings outward from `centre` inside `bounds`, skipping the
    /// positions `exclude` already covered.
    ///
    /// Near to far and symmetric: each ring covers both directions, so a
    /// scroll-back is found as quickly as a scroll-forward and neither
    /// direction is searched exhaustively before the other. The first ring is
    /// one viewport wide, so the size comes from the frame, not from a tuned
    /// constant.
    #[allow(clippy::too_many_arguments)]
    fn search_rings(
        &self,
        cache: &mut ScoreCache,
        ladder: &EdgeLadder,
        centre: i32,
        bounds: (i32, i32),
        exclude: &[(i32, i32)],
        request: &PlacementRequest<'_>,
    ) -> Option<(i32, f64)> {
        let (lo, hi) = bounds;
        if lo > hi {
            return None;
        }
        let max_radius = (centre - lo).abs().max((hi - centre).abs());
        if max_radius <= 0 {
            return None;
        }
        let mut ring_inner = 0;
        let mut radius = (ladder.rows() as i32).min(max_radius);
        loop {
            if let DecideOutcome::Accept { top, cost } = self.decide_ring(
                cache, ladder, centre, ring_inner, radius, bounds, exclude, request,
            ) {
                return Some((top, cost));
            }
            if radius >= max_radius {
                return None;
            }
            ring_inner = radius;
            radius = radius.saturating_mul(EDGE_RING_GROWTH).min(max_radius);
        }
    }

    fn impulse_half_width(&self) -> i32 {
        let scale = self
            .last_delta
            .map(|delta| {
                (delta.abs() * EDGE_IMPULSE_SCALE_NUMERATOR) / EDGE_IMPULSE_SCALE_DENOMINATOR
            })
            .unwrap_or(0);
        scale.max(EDGE_IMPULSE_MIN_HALF_WIDTH)
    }

    fn accept(
        &mut self,
        request: &PlacementRequest<'_>,
        ladder: EdgeLadder,
        top: i32,
        cost: f64,
    ) -> EngineDecision {
        self.pending_ladder = Some(ladder);
        let pos = self.pos.unwrap_or(top);
        let delta = top - pos;
        let overlap = self.frame_rows.saturating_sub(delta.unsigned_abs());
        self.pending_top = Some(top);
        let (rect, direction, delta_x, delta_y) = match self.axis {
            SearchAxis::Vertical => (
                ViewportRect {
                    x: request.previous_rect.x,
                    y: top,
                    width: request.previous_rect.width,
                    height: request.previous_rect.height,
                },
                if delta < 0 {
                    AppendDirection::Top
                } else {
                    AppendDirection::Bottom
                },
                0,
                delta,
            ),
            SearchAxis::Horizontal => (
                ViewportRect {
                    x: top,
                    y: request.previous_rect.y,
                    width: request.previous_rect.width,
                    height: request.previous_rect.height,
                },
                if delta < 0 {
                    AppendDirection::Left
                } else {
                    AppendDirection::Right
                },
                delta,
                0,
            ),
        };
        EngineDecision::Place {
            rect,
            match_info: FrameMatch {
                direction,
                overlap,
                delta_x,
                delta_y,
                score: cost,
            },
        }
    }
}

enum DecideOutcome {
    Accept { top: i32, cost: f64 },
    Reject(RejectReason),
}

impl PlacementEngine for WindowedEdgeMatcher {
    fn name(&self) -> &'static str {
        "windowed-edge"
    }

    fn place(&mut self, request: &PlacementRequest<'_>) -> EngineDecision {
        self.pending_ladder = None;
        let Some(ladder) = EdgeLadder::from_source(request.frame, self.session_gain) else {
            return EngineDecision::Reject { reason: RejectReason::Invalid };
        };
        let Some(pos) = self.pos else {
            return EngineDecision::Reject { reason: RejectReason::Uninitialised };
        };
        if let Some(canvas) = self.canvas.as_ref() {
            if canvas.bins != ladder.bins {
                return EngineDecision::Reject { reason: RejectReason::Invalid };
            }
        }
        if ladder.scoring_rows.is_empty() {
            // Uniform content: there is nothing to compare and no honest answer.
            return EngineDecision::Reject { reason: RejectReason::NoBoundaries };
        }

        let prediction = match self.last_delta {
            Some(delta) => (f64::from(delta) + self.trend).round() as i32,
            None => 0,
        };
        let predicted_top = pos + prediction;

        // The impulse window's radius: zero on the first frame, where there is
        // no step to extrapolate.
        // One cache for the whole decision: the stages look at overlapping
        // positions, and each position's cost is fixed for this frame.
        let mut cache = ScoreCache::default();

        let impulse_reach = if self.last_delta.is_some() {
            self.impulse_half_width()
        } else {
            0
        };

        // Stage 1 — impulse: the neighbourhood of where the last step points.
        // Almost every frame is decided here, and it costs one narrow window.
        if impulse_reach > 0 {
            if let DecideOutcome::Accept { top, cost } = self.decide(
                &mut cache,
                &ladder,
                predicted_top - impulse_reach,
                predicted_top + impulse_reach,
                predicted_top,
                request,
            ) {
                return self.accept(request, ladder, top, cost);
            }
        }

        // Stage 2 — guard: is this frame a continuation of the previous one?
        //
        // The question is local. The frame can only be read as the next step of
        // the capture if it still shares `minimum_overlap` rows with the frame
        // that came before it, so the window is `pos ± adjacency_radius` — 228
        // rows on a 254-row viewport, regardless of how tall the canvas has
        // grown. Walking the whole canvas here would answer a different
        // question ("does this frame appear anywhere we have seen?"), which is
        // the anchor's job, and it would also let a far match be accepted as if
        // it were the next step.
        //
        // Rings run outward from the prediction in both directions, so the
        // nearest continuation wins and neither direction is exhausted first.
        // The guard band is centred on the previous frame, not on the
        // prediction, so it cannot exclude the impulse window the way two
        // concentric rings would: their centres differ. It searches its whole
        // band, ordered from the prediction. The rows the impulse window
        // already tried are re-*judged* (each stage applies the gates to its
        // own candidate set, and that is what decides the answer) but not
        // re-*computed*: the score cache below returns the cost already paid
        // for. Dropping them from the band instead is not an option — the
        // impulse window sees too small a candidate set to find a runner-up, so
        // a position it rejected for want of one would be lost for good.
        let radius = self.adjacency_radius(&ladder);
        let guard = (pos - radius, pos + radius);
        if let DecideOutcome::Accept { top, cost } = self.decide_ring(
            &mut cache,
            &ladder,
            predicted_top,
            0,
            radius,
            guard,
            &[],
            request,
        ) {
            return self.accept(request, ladder, top, cost);
        }

        // Stage 3 — anchor: recovery, and only recovery.
        //
        // This stage answers a different question from the guard: not "is this
        // frame the next step?" but "is this frame somewhere we have already
        // seen?". That is a much weaker claim, so it must not be tried on every
        // frame — a frame that merely failed to continue the capture would
        // otherwise be matched against unrelated content somewhere up the page
        // and appended there.
        //
        // It runs only once the border is already red, i.e. after
        // `REFUSAL_PATIENCE` consecutive refusals have shown that this is not a
        // one-frame seam but a real loss of the scroll position. The engine
        // shares that count with the UI rather than keeping its own: two
        // counters for one decision drift apart.
        //
        // Ordered from the last confirmed position, because the prediction has
        // just been shown to be wrong, and it skips the guard band, which the
        // stage above already scored.
        let recovering = request.refusals >= crate::stitch::REFUSAL_PATIENCE;
        if recovering {
            let anchor = self.anchor.unwrap_or(pos);
            if let Some(bounds) = self.canvas_bounds(&ladder) {
                if let Some((top, cost)) =
                    self.search_rings(&mut cache, &ladder, anchor, bounds, &[guard], request)
                {
                    return self.accept(request, ladder, top, cost);
                }
            }
        }

        // Stage 4 — the frame overlaps the canvas, but nothing in it matches
        // anywhere the evidence allows. Nothing moves; the caller waits for the
        // next frame.
        EngineDecision::Reject { reason: RejectReason::NoCandidate }
    }

    fn commit(&mut self, _rect: ViewportRect, requested_top: i32, frame: &dyn RgbSource) {
        // The session's one gain is decided by the frame that initialises the
        // canvas, before that frame's own ladder is built: every ladder —
        // including the first — must share the scale the canvas is written
        // with.
        if self.canvas.is_none() {
            self.session_gain = EdgeLadder::session_gain(frame);
        }
        let ladder = match self.pending_ladder.take() {
            Some(ladder) => ladder,
            None => match EdgeLadder::from_source(frame, self.session_gain) {
                Some(ladder) => ladder,
                None => return,
            },
        };
        let Some(canvas) = self.canvas.as_mut() else {
            let mut canvas = EdgeCanvas::new(ladder.bins, 0);
            canvas.write(requested_top, &ladder);
            self.frame_rows = ladder.rows();
            self.canvas = Some(canvas);
            self.pos = Some(requested_top);
            self.anchor = Some(requested_top);
            self.pending_top = None;
            return;
        };
        if ladder.bins != canvas.bins {
            return;
        }
        // `rect` is the frame's rectangle in the *pixel* canvas, which re-bases
        // itself when it grows upward. Nothing here reads it: every position
        // this engine stores (`pos`, `anchor`, the ladder) is a logical canvas
        // row, and `write` grows the front when a frame lands above what is
        // stored. Treating the pixel canvas's re-based row as a per-frame shift
        // is what made this canvas grow quadratically — `rect.y - requested_top`
        // is the *cumulative* offset between the two coordinate systems, not
        // this frame's prepend, so it re-shifted every stored row on every
        // commit (measured: 589 080 rows of descriptor for an 8 534-row
        // capture, 29.9 MB where 0.45 MB is the true extent).
        let previous = self.pos;
        canvas.write(requested_top, &ladder);
        self.frame_rows = ladder.rows();
        self.pos = Some(requested_top);
        self.anchor = Some(requested_top);
        if let Some(previous) = previous {
            let delta = requested_top - previous;
            let last = self.last_delta.unwrap_or(0);
            self.trend = 0.5 * self.trend + 0.5 * f64::from(delta - last);
            self.last_delta = Some(delta);
        }
        self.pending_top = None;
        self.last_committed_ladder = Some(ladder);
    }

    fn reset(&mut self) {
        let axis = self.axis;
        *self = Self::default();
        self.axis = axis;
    }

    fn last_descriptor(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.pending_ladder
            .as_ref()
            .or(self.last_committed_ladder.as_ref())
            .map(|ladder| ladder.as_grayscale())
    }
}

/// Places a frame on the horizontal axis, building the descriptor on demand.
///
/// The vertical matcher's canvas accumulates for the whole session because
/// every frame contributes to it. The horizontal axis gets no such state: a
/// horizontal step is rare, and a persistent second canvas would have to be
/// re-seeded on every vertical step anyway. Instead the descriptor is built for
/// the one region the comparison can use — the previous frame's rectangle,
/// which is captured by construction because that frame wrote it — and the
/// same decision code runs on it.
///
/// Two restrictions follow from what is known at that moment:
///
///  - **One direction, or both.** On the impulse axis both directions are
///    ordinary (a scroll-back is a step like any other), so `direction` is
///    `None` and the window is symmetric. As a *fallback* the caller only gets
///    here with a direction the user actually indicated, and only positions on
///    that side are considered.
///  - **Only the comparable part.** The strip is the previous frame's own
///    rectangle, and a candidate is scored only where it overlaps it. On a
///    ragged canvas the rest of the frame sits on pixels nothing has captured;
///    those are not evidence and are never compared.
///
/// The gates are the vertical ones (`decide_from_costs`), minus the anchor
/// window: recovery belongs to the axis the capture is on.
pub(super) fn place_horizontally(
    stitched: &StitchedFrame,
    previous_rect: ViewportRect,
    frame: &dyn RgbSource,
    direction: Option<SearchDirection>,
) -> EngineDecision {
    let width = frame.width();
    let height = frame.height();
    if width == 0 || height == 0 || stitched.width == 0 || stitched.height == 0 {
        return EngineDecision::Reject {
            reason: RejectReason::Invalid,
        };
    }
    let forward = match direction {
        None => None,
        Some(SearchDirection::Right) => Some(true),
        Some(SearchDirection::Left) => Some(false),
        Some(_) => {
            return EngineDecision::Reject {
                reason: RejectReason::Invalid,
            };
        }
    };
    let previous_x = previous_rect.x;
    let previous_y = previous_rect.y;

    // The previous frame's rectangle: the only pixels this comparison may use.
    let strip_x = previous_x.max(0);
    let strip_y = previous_y.max(0);
    let strip_right = previous_x
        .saturating_add(width as i32)
        .min(stitched.width as i32);
    let strip_bottom = previous_y
        .saturating_add(height as i32)
        .min(stitched.height as i32);
    if strip_right <= strip_x || strip_bottom <= strip_y {
        return EngineDecision::Reject {
            reason: RejectReason::NoCandidate,
        };
    }

    let gain = EdgeLadder::session_gain_on(frame, SearchAxis::Horizontal);
    let Some(frame_ladder) = EdgeLadder::from_source_on(frame, gain, SearchAxis::Horizontal)
    else {
        return EngineDecision::Reject {
            reason: RejectReason::Invalid,
        };
    };
    if frame_ladder.scoring_rows().is_empty() {
        // Uniform content along the axis: there is nothing to compare.
        return EngineDecision::Reject {
            reason: RejectReason::NoBoundaries,
        };
    }
    let strip = CanvasRegion::new(
        stitched,
        strip_x as u32,
        strip_y as u32,
        (strip_right - strip_x) as u32,
        (strip_bottom - strip_y) as u32,
    );
    let Some(strip_ladder) = EdgeLadder::from_source_on(&strip, gain, SearchAxis::Horizontal)
    else {
        return EngineDecision::Reject {
            reason: RejectReason::Invalid,
        };
    };
    if strip_ladder.bins() != frame_ladder.bins() {
        return EngineDecision::Reject {
            reason: RejectReason::Invalid,
        };
    }
    let mut canvas = EdgeCanvas::new(strip_ladder.bins(), strip_x);
    canvas.write(strip_x, &strip_ladder);

    // Candidates stay within one frame width of the previous position: further
    // than that the two frames share nothing, so there is no overlap to score.
    let (window_lo, window_hi) = match forward {
        Some(true) => (previous_x, previous_x.saturating_add(width as i32)),
        Some(false) => (previous_x.saturating_sub(width as i32), previous_x),
        None => (
            previous_x.saturating_sub(width as i32),
            previous_x.saturating_add(width as i32),
        ),
    };

    let mut matcher = WindowedEdgeMatcher::new(SearchAxis::Horizontal);
    matcher.canvas = Some(canvas);
    matcher.pos = Some(previous_x);
    // No `anchor`: `decide` is the only stage this matcher runs, and the anchor
    // window is not part of the horizontal axis (recovery belongs to the axis
    // the capture is on).
    matcher.frame_rows = width;
    let request = PlacementRequest {
        stitched,
        previous_rect,
        frame,
        search_direction: direction,
        refusals: 0,
    };
    let mut cache = ScoreCache::default();
    match matcher.decide(
        &mut cache,
        &frame_ladder,
        window_lo,
        window_hi,
        previous_x,
        &request,
    ) {
        DecideOutcome::Accept { top, cost } => {
            let delta = top - previous_x;
            EngineDecision::Place {
                rect: ViewportRect {
                    x: top,
                    y: previous_y,
                    width: previous_rect.width,
                    height: previous_rect.height,
                },
                match_info: FrameMatch {
                    direction: if delta < 0 {
                        AppendDirection::Left
                    } else {
                        AppendDirection::Right
                    },
                    overlap: width.saturating_sub(delta.unsigned_abs()),
                    delta_x: delta,
                    delta_y: 0,
                    score: cost,
                },
            }
        }
        DecideOutcome::Reject(reason) => EngineDecision::Reject { reason },
    }
}

// ---------------------------------------------------------------------------
// Legacy engine (feature gated): the canvas-scan matcher, for A/B only
// ---------------------------------------------------------------------------

#[cfg(feature = "stitch-legacy")]
pub(super) struct LegacyPipeline;

#[cfg(feature = "stitch-legacy")]
impl PlacementEngine for LegacyPipeline {
    fn name(&self) -> &'static str {
        "legacy"
    }

    fn place(&mut self, request: &PlacementRequest<'_>) -> EngineDecision {
        let axis = super::canvas_search_axis(request.search_direction);
        let Some(match_info) = super::find_canvas_placement_match(
            request.stitched,
            request.previous_rect,
            &DynRgbSource(request.frame),
            axis,
            request.search_direction,
        ) else {
            return EngineDecision::Reject { reason: RejectReason::NoCandidate };
        };
        let rect = ViewportRect {
            x: request.previous_rect.x + match_info.delta_x,
            y: request.previous_rect.y + match_info.delta_y,
            width: request.previous_rect.width,
            height: request.previous_rect.height,
        };
        EngineDecision::Place { rect, match_info }
    }

    fn commit(&mut self, _rect: ViewportRect, _requested_top: i32, _frame: &dyn RgbSource) {}

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// Mock engine (test only): scripted decisions, no image data
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct ScriptedEngine {
    script: VecDeque<EngineDecision>,
    committed: Vec<ViewportRect>,
    resets: u32,
}

#[cfg(test)]
impl ScriptedEngine {
    pub(super) fn new(script: Vec<EngineDecision>) -> Self {
        Self { script: script.into(), committed: Vec::new(), resets: 0 }
    }

    pub(super) fn committed(&self) -> &[ViewportRect] {
        &self.committed
    }

    pub(super) fn resets(&self) -> u32 {
        self.resets
    }
}

#[cfg(test)]
impl PlacementEngine for ScriptedEngine {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn place(&mut self, _request: &PlacementRequest<'_>) -> EngineDecision {
        self.script
            .pop_front()
            .unwrap_or(EngineDecision::Reject { reason: RejectReason::NoCandidate })
    }

    fn commit(&mut self, rect: ViewportRect, _requested_top: i32, _frame: &dyn RgbSource) {
        self.committed.push(rect);
    }

    fn reset(&mut self) {
        self.resets += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stitch::{RgbFrame, RowCoverage};

    const W: u32 = 192;

    /// A page whose rows are distinct: `row_texture` mixes the row index into
    /// every channel, so a one-row shift is visible.
    fn row_texture(row: u32) -> Vec<u8> {
        let mut data = vec![0u8; W as usize * 3];
        for x in 0..W {
            let o = x as usize * 3;
            data[o] = ((x * 37 + row * 17 + (x * row) % 29) % 251) as u8;
            data[o + 1] = ((x * 19 + row * 31 + (x + row * 3) % 23) % 251) as u8;
            data[o + 2] = ((x * 11 + row * 7 + (x * 5 + row) % 41) % 251) as u8;
        }
        data
    }

    fn frame_from(top: u32, height: u32) -> RgbFrame {
        let stride = W * 3;
        let mut data = Vec::with_capacity(stride as usize * height as usize);
        for row in top..top + height {
            data.extend_from_slice(&row_texture(row));
        }
        RgbFrame { width: W, height, stride, data }
    }

    /// Same page as [`frame_from`], but allowing rows above zero so a frame
    /// that extends the canvas upward can be built.
    fn frame_from_signed(top: i32, height: u32) -> RgbFrame {
        let stride = W * 3;
        let mut data = Vec::with_capacity(stride as usize * height as usize);
        for row in top..top + height as i32 {
            let row = ((row % 4096) + 4096) as u32 % 4096;
            data.extend_from_slice(&row_texture(row));
        }
        RgbFrame { width: W, height, stride, data }
    }

    /// Flat page: identical rows, so no boundary exists anywhere.
    fn flat_frame(height: u32) -> RgbFrame {
        let stride = W * 3;
        let row = row_texture(0);
        let mut data = Vec::with_capacity(stride as usize * height as usize);
        for _ in 0..height {
            data.extend_from_slice(&row);
        }
        RgbFrame { width: W, height, stride, data }
    }

    /// Alternating near-identical grays with pseudo-random run lengths: the
    /// `lowcontrast-2` corpus in miniature. Bounded runs keep it free of long
    /// flat blocks that would match anywhere.
    fn stripe_page_rows(len: usize, delta: u8) -> Vec<u8> {
        let mut rows = Vec::with_capacity(len);
        let mut state = 0x1234_5678u32;
        let mut dark = true;
        while rows.len() < len {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let run = 12 + (state >> 16) as usize % 37;
            let value = if dark { 128 } else { 128 + delta };
            for _ in 0..run.min(len - rows.len()) {
                rows.push(value);
            }
            dark = !dark;
        }
        rows
    }

    fn stripe_frame(page: &[u8], top: u32, height: u32) -> RgbFrame {
        let stride = W * 3;
        let mut data = Vec::with_capacity(stride as usize * height as usize);
        for y in top..top + height {
            let value = page[y as usize];
            for _ in 0..W {
                data.extend_from_slice(&[value, value, value]);
            }
        }
        RgbFrame { width: W, height, stride, data }
    }

    /// Low contrast is not supposed to change where a frame lands. With an
    /// absolute duplicate threshold the moving frames looked identical and
    /// were dropped; with absolute quantisation every boundary fell to level
    /// zero and every frame was refused.
    #[test]
    fn low_contrast_stripes_are_placed_like_high_contrast_content() {
        let page = stripe_page_rows(1024, 2);
        let height = 160;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = stripe_frame(&page, 0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        let first_frame = first.clone();
        let mut committed: Vec<(i32, RgbFrame)> = vec![(0, first_frame)];
        for (top, expected) in [(60u32, 60), (120, 120)] {
            let frame = stripe_frame(&page, top, height);
            let refs: Vec<(i32, &RgbFrame)> =
                committed.iter().map(|(y, f)| (*y, f)).collect();
            let req =
                request_on_canvas(&frame, ViewportRect { x: 0, y: 0, width: W, height }, &refs);
            match engine.place(&req) {
                EngineDecision::Place { rect, .. } => assert_eq!(rect.y, expected),
                other => panic!("frame at {top} refused: {other:?}"),
            }
            engine.commit(ViewportRect { x: 0, y: expected, width: W, height }, expected, &frame);
            committed.push((expected, frame.clone()));
        }
    }

    /// Control for the test below: same page, same scroll, no high-contrast
    /// block anywhere. Both frames share one gain, so the placement is exact.
    /// If this passes while the block test fails, the block's effect on `gain`
    /// is the cause.
    #[test]
    fn low_contrast_only_content_is_placed_without_a_block_in_view() {
        let height = 160;
        let page = stripe_page_rows(2048, 2);
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = stripe_frame(&page, 0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let second = stripe_frame(&page, 60, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&second, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => assert_eq!(rect.y, 60),
            other => panic!("low-contrast-only frame refused: {other:?}"),
        }
    }

    /// A page that is mostly two grays two levels apart, with one high-contrast
    /// block that scrolls into view. The block sets the frame's own peak, and
    /// the peak sets the quantisation gain, so the *unchanged* low-contrast rows
    /// land on different levels depending on whether the block is in view.
    ///
    /// `gain` is a session-wide constant, decided by the frame that
    /// initialises the canvas. It used to be derived per frame, which made the
    /// two sides of a comparison disagree about unchanged content: frame one
    /// (peak 2, gain 4) stored the low-contrast rows as level 1, while frame
    /// two (peak 128 because the block was in view, gain 1) quantised the same
    /// rows to level 0 and scored only the block's edges, which landed on
    /// canvas rows never committed. With one session gain, frame two quantises
    /// on frame one's scale: the block saturates at level 2 and the
    /// low-contrast rows stay level 1 on both sides.
    ///
    /// No corpus catches this because every corpus has a constant gain: all
    /// high-contrast corpora sit at 1.0, lowcontrast-2b at 4.0 throughout.
    #[test]
    fn a_high_contrast_block_does_not_erase_the_low_contrast_rows() {
        let height = 160;
        let mut page: Vec<u8> = stripe_page_rows(2048, 2);
        // A black block below the first frame's viewport, so only the second
        // frame contains it. 160..220 is outside frame one (0..160) and inside
        // frame two (60..220).
        for row in 160..220 {
            page[row] = 0;
        }

        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        // First frame: pure low contrast, peak 2, gain 4.
        let first = stripe_frame(&page, 0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // Second frame: the black block is in view, so peak 128 and gain 1.
        // The overlapping low-contrast rows are the same content as in frame
        // one; only their quantisation changed.
        let second = stripe_frame(&page, 60, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&second, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => {
                assert_eq!(rect.y, 60, "the frame's true position, got {}", rect.y)
            }
            other => panic!("frame with a high-contrast block refused: {other:?}"),
        }
    }

    /// The anchor stage searches outward from the anchor, so when the content
    /// matches at several distances the nearest one wins.
    ///
    /// Periodic content makes that observable: every period-length offset is
    /// pixel-identical, so a single wide scan returns whichever offset it
    /// happens to score first (in practice the far end, because ties resolve by
    /// sort order), while the ring ladder returns the nearest. This is the
    /// behaviour the ring loop exists for.
    #[test]
    fn anchor_stage_prefers_the_nearest_of_equivalent_offsets() {
        const PERIOD: u32 = 40;
        let height = 200;
        // A page that repeats every PERIOD rows.
        let periodic = |top: i32, height: u32| {
            let stride = W * 3;
            let mut data = Vec::with_capacity(stride as usize * height as usize);
            for row in top..top + height as i32 {
                let r = ((row.rem_euclid(PERIOD as i32)) as u32) * 5 + 40;
                let tex = row_texture(r);
                data.extend_from_slice(&tex);
            }
            RgbFrame { width: W, height, stride, data }
        };

        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = periodic(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // Push content far enough that it does not overlap the committed
        // canvas at all, so the engine refuses and the anchor freezes at 0.
        // Periodic content cannot produce a refusal on its own — it matches
        // everywhere — so this uses flat content, which has no boundaries.
        let flat = {
            let stride = W * 3;
            let row = row_texture(0);
            let mut data = Vec::new();
            for _ in 0..height {
                data.extend_from_slice(&row);
            }
            RgbFrame { width: W, height, stride, data }
        };
        let committed_flat: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&flat, ViewportRect { x: 0, y: 0, width: W, height }, &committed_flat);
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));

        // 40 rows down is exactly one period: it matches at 40, 0, -40, ...
        // The nearest answer to the anchor at 0 is 40 (one period down).
        let back = periodic(40, height);
        let committed_back: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&back, ViewportRect { x: 0, y: 0, width: W, height }, &committed_back);
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => {
                assert!(
                    rect.y.abs() <= PERIOD as i32,
                    "the nearest equivalent offset to the anchor must win, got {}",
                    rect.y
                );
                assert_eq!(rect.y % PERIOD as i32, 0, "must land on a period boundary");
            }
            other => panic!("expected recovery near the anchor, got {other:?}"),
        }
    }

    /// A request whose pixel canvas is built from the frames the test actually
    /// committed, rebased the way the stitcher does it.
    ///
    /// The colour check reads `stitched` to verify a structural match against
    /// real pixels. A fabricated canvas there (`canvas_of`, which always holds
    /// row-texture rows 0..n) makes content the test committed look unrelated,
    /// and the check rightly rejects it. This mirrors what production does:
    /// rows are addressed directly by canvas coordinate, and a frame that
    /// lands above row 0 rebases the whole canvas (data *and* rects) instead of
    /// shifting an origin.
    fn request_on_canvas<'a>(
        frame: &'a dyn RgbSource,
        rect: ViewportRect,
        committed: &[(i32, &RgbFrame)],
    ) -> PlacementRequest<'a> {
        // `committed` is in *commit order*, which is what the engine's canvas
        // reflects: the first entry initialises it at pixel row 0 and each
        // later one prepends or appends as its position requires. Sorting by
        // position would describe a canvas the engine never built.
        let mut rows: Vec<Option<Vec<u8>>> = Vec::new();
        let mut prev_rect_y = 0i32;
        let mut prev_engine_pos = 0i32;
        let mut first = true;
        for &(top, commit_frame) in committed {
            if first {
                // The first frame initialises the canvas at pixel row 0.
                rows = vec![None; commit_frame.height as usize];
                for row in 0..commit_frame.height as usize {
                    let src = row * commit_frame.stride as usize;
                    rows[row] =
                        Some(commit_frame.data[src..src + W as usize * 3].to_vec());
                }
                prev_rect_y = 0;
                prev_engine_pos = top;
                first = false;
                continue;
            }
            let mut anchor = prev_rect_y + (top - prev_engine_pos);
            if anchor < 0 {
                // Prepend: everything moves down by -anchor; rects rebase too.
                let shift = (-anchor) as usize;
                let mut rebased = vec![None; shift];
                rebased.extend(rows);
                rows = rebased;
                anchor = 0;
                prev_rect_y = 0;
            } else {
                prev_rect_y = anchor;
            }
            prev_engine_pos = top;
            if rows.len() < anchor as usize + commit_frame.height as usize {
                rows.resize(anchor as usize + commit_frame.height as usize, None);
            }
            for row in 0..commit_frame.height as usize {
                let index = anchor as usize + row;
                if rows[index].is_some() {
                    continue; // first write wins, mirroring EdgeCanvas::write
                }
                let src = row * commit_frame.stride as usize;
                rows[index] =
                    Some(commit_frame.data[src..src + W as usize * 3].to_vec());
            }
        }
        let height = rows.len().max(1) as u32;
        let mut data = vec![0u8; W as usize * 3 * height as usize];
        for (row, content) in rows.iter().enumerate() {
            if let Some(content) = content {
                let dst = row * W as usize * 3;
                data[dst..dst + content.len()].copy_from_slice(content);
            }
        }
        // `anchor_origin_y` is the logical row of pixel row 0: the last
        // commit's logical top minus its pixel row. The stitcher rebases both
        // by the same amount, so their difference is invariant and is exactly
        // this origin.
        let origin_y = if let Some(&(top, _)) = committed.last() {
            top - prev_rect_y
        } else {
            0
        };
        let stitched = StitchedFrame {
            width: W,
            height,
            stride: W * 3,
            data,
            current_origin_x: 0,
            current_origin_y: 0,
            anchor_origin_x: 0,
            anchor_origin_y: origin_y,
            compose_width: W,
            compose_height: height,
            active_crop: crate::stitch::ComposeCrop {
                x: 0,
                y: 0,
                width: W,
                height,
            },
            fixed_bands: crate::stitch::FixedBands::default(),
            // Mirror the stitcher: prepended rows carry no pixels, so the
            // colour check must not compare the frame against them.
            row_coverage: rows
                .iter()
                .map(|row| {
                    if row.is_some() {
                        RowCoverage::Full
                    } else {
                        RowCoverage::Empty
                    }
                })
                .collect(),
        };
        PlacementRequest {
            stitched: Box::leak(Box::new(stitched)),
            // The previous frame's pixel row, as the stitcher would report it.
            // Tests cannot pass a `rect.y` that disagrees with the canvas they
            // just built.
            previous_rect: ViewportRect { y: prev_rect_y, ..rect },
            frame,
            search_direction: None,
            refusals: 0,
        }
    }

    fn seed(engine: &mut WindowedEdgeMatcher, top: u32, height: u32) -> ViewportRect {
        let frame = frame_from(top, height);
        let rect = ViewportRect { x: 0, y: 0, width: W, height };
        engine.commit(rect, 0, &frame);
        rect
    }

    #[test]
    fn ladder_marks_the_rows_where_content_changes() {
        // Two blocks of identical rows: the only boundary is between them.
        let stride = W * 3;
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend_from_slice(&row_texture(0));
        }
        for _ in 0..10 {
            data.extend_from_slice(&row_texture(120));
        }
        let frame = RgbFrame { width: W, height: 20, stride, data };
        let ladder = EdgeLadder::from_source(&frame, EdgeLadder::session_gain(&frame)).unwrap();
        assert_eq!(ladder.rows(), 20);
        assert_eq!(ladder.scoring_rows, vec![10]);
    }

    #[test]
    fn uniform_frame_reports_no_boundaries_instead_of_guessing() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        seed(&mut engine, 0, 40);
        let flat = flat_frame(40);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 40 };
        let seeded = frame_from(0, 40);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &seeded)];
        let req = request_on_canvas(&flat, rect, &committed);
        assert_eq!(
            engine.place(&req),
            EngineDecision::Reject { reason: RejectReason::NoBoundaries }
        );
    }

    #[test]
    fn finds_a_small_forward_shift() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        seed(&mut engine, 0, 60);
        let frame = frame_from(17, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let seeded = frame_from(0, 60);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &seeded)];
        let req = request_on_canvas(&frame, rect, &committed);
        let EngineDecision::Place { rect: placed, match_info } = engine.place(&req) else {
            panic!("expected a placement for a 17-row shift");
        };
        assert_eq!(placed.y, 17);
        assert_eq!(match_info.delta_y, 17);
        assert!(match_info.score < 0.01);
    }

    #[test]
    fn refuses_when_the_frame_is_nowhere_in_the_canvas() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        seed(&mut engine, 0, 60);
        // 900 rows away: no overlap with a 60-row canvas.
        let frame = frame_from(900, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let seeded = frame_from(0, 60);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &seeded)];
        let req = request_on_canvas(&frame, rect, &committed);
        let decision = engine.place(&req);
        assert!(
            matches!(decision, EngineDecision::Reject { .. }),
            "a frame with no overlap must be refused, got {decision:?}"
        );
    }

    #[test]
    fn a_one_row_shift_is_distinguishable_from_the_truth() {
        // The basin is one row wide; this asserts the descriptor keeps it.
        let a = frame_from(100, 80);
        let b = frame_from(117, 80);
        let ga = EdgeLadder::session_gain(&a);
        let la = EdgeLadder::from_source(&a, ga).unwrap();
        let lb = EdgeLadder::from_source(&b, ga).unwrap();
        let mut canvas = EdgeCanvas::new(la.bins, 0);
        canvas.write(0, &la);

        let truth = canvas.cost_at(&lb, 17, 1).unwrap().cost;
        let off_by_one = canvas.cost_at(&lb, 18, 1).unwrap().cost;
        assert!(truth < 0.01, "true offset should score ~0, got {truth}");
        assert!(
            off_by_one > truth * 10.0,
            "one-row error must be clearly worse: truth={truth} off-by-one={off_by_one}"
        );
    }

    #[test]
    fn periodic_content_is_placed_at_the_nearest_of_its_equally_good_offsets() {
        // 12 identical rows repeated: many offsets score identically. The
        // content genuinely cannot say which is right, so the engine takes the
        // one closest to the predicted scroll — the user scrolled a little, not
        // a page. Every such answer is equally supported by the pixels, so
        // picking the nearest keeps the canvas continuous instead of dropping
        // content the page is willing to give us.
        let stride = W * 3;
        let mut data = Vec::new();
        for i in 0..120 {
            data.extend_from_slice(&row_texture(i % 12));
        }
        let page = RgbFrame { width: W, height: 120, stride, data };
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        engine.commit(
            ViewportRect { x: 0, y: 0, width: W, height: 60 },
            0,
            &crop(&page, 0, 60),
        );
        // The frame sits 24 rows down, but 24 = 2 periods, so 0 also matches.
        let frame = crop(&page, 24, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let committed_crop = crop(&page, 0, 60);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &committed_crop)];
        let mut req = request_on_canvas(&frame, rect, &committed);
        match engine.place(&req) {
            EngineDecision::Place { rect: placed, .. } => {
                // Nearest to the zero prediction: 0 is one period closer than
                // 24, and both are pixel-identical.
                assert!(
                    placed.y == 0 || placed.y == 24,
                    "must land on a period-equivalent offset, got {}",
                    placed.y
                );
                assert_eq!(placed.y % 12, 0, "offset must be a whole number of periods");
            }
            other => panic!("periodic content should be placed at the nearest period, got {other:?}"),
        }
    }

    #[test]
    fn rejection_does_not_move_the_anchor() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        seed(&mut engine, 0, 60);
        let anchor_before = engine.anchor;
        let pos_before = engine.pos;
        let delta_before = engine.last_delta;
        let trend_before = engine.trend;

        let frame = frame_from(2000, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let seeded = frame_from(0, 60);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &seeded)];
        let req = request_on_canvas(&frame, rect, &committed);
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));

        assert_eq!(engine.anchor, anchor_before);
        assert_eq!(engine.pos, pos_before);
        assert_eq!(engine.last_delta, delta_before);
        assert_eq!(engine.trend, trend_before);
    }

    /// The pixel canvas grows upward by prepending rows when a frame extends it
    /// past row 0, and `commit` receives both the logical row the engine asked
    /// for (`requested_top`) and the row the pixel canvas ended up using
    /// (`rect.y`). The engine's own state must stay in logical coordinates:
    /// storing the re-based pixel row made every later frame compare against
    /// content shifted by the whole prepend, and the canvas silently stopped
    /// growing.
    #[test]
    fn prepending_rows_keeps_the_engine_in_logical_coordinates() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let height = 60;

        // Canvas starts covering page rows 100..=159 at logical row 0.
        let first = frame_from_signed(100, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // Scroll up by 17: the frame now starts at page row 83, i.e. 17 rows
        // above the canvas top, so it belongs at logical row -17. The pixel
        // canvas grows upward, so it prepends 17 rows and this frame lands at
        // pixel row 0 while the engine asked for -17.
        let upward = frame_from_signed(83, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let mut req =
            request_on_canvas(&upward, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        let EngineDecision::Place { match_info, .. } = engine.place(&req) else {
            panic!("expected the upward frame to be placed");
        };
        assert_eq!(match_info.delta_y, -17);

        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, -17, &upward);

        // The engine must report the logical row it decided on, not the pixel
        // row the canvas re-based to.
        assert_eq!(engine.pos, Some(-17));

        // The third placement is deliberately not asserted here.
        //
        // It used to be, against a fabricated 60-row canvas. On a truthful
        // canvas the frame's true row is -34 while the prediction (pos -17,
        // last delta -17, trend -8.5) points at -51, and the row texture makes
        // both score alike — so the assertion could only ever have been
        // passing because the canvas was fake. What this test exists for is the
        // line above: after a prepend the engine still reports the *logical*
        // row, and that is what `commit` and the caller's delta depend on.
    }

    /// A frame placed by prepending must actually be readable at the logical
    /// row it was placed at. Before the fix the write landed inside rows that
    /// were wrongly marked as already filled, so the new frame never entered
    /// the canvas and the engine kept comparing against stale content.
    #[test]
    fn a_prepended_frame_is_written_into_the_canvas() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let height = 60;
        let first = frame_from_signed(100, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        let upward = frame_from_signed(83, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let mut req =
            request_on_canvas(&upward, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        assert!(matches!(engine.place(&req), EngineDecision::Place { .. }));
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, -17, &upward);

        let canvas = engine.canvas.as_ref().expect("canvas exists");
        let ladder =
            EdgeLadder::from_source(&upward, EdgeLadder::session_gain(&upward)).unwrap();
        let cost = canvas.cost_at(&ladder, -17, 1).expect("evidence at -17");
        assert!(
            cost.cost < 0.01,
            "the prepended frame must be readable at its logical row, got {}",
            cost.cost
        );
        // The rows it did not cover must still hold the older content.
        let old = EdgeLadder::from_source(&first, EdgeLadder::session_gain(&first)).unwrap();
        let cost = canvas.cost_at(&old, 0, 1).expect("evidence at 0");
        assert!(
            cost.cost < 0.01,
            "the original content must survive the prepend, got {}",
            cost.cost
        );
    }

    /// The engine's canvas holds one descriptor row per *logical* canvas row.
    ///
    /// The pixel canvas re-bases itself whenever a frame extends it upward, and
    /// that re-based row used to be handed to `commit` as a per-frame prepend.
    /// It is not one: it is the cumulative distance between two coordinate
    /// systems, so re-applying it every frame shifted every stored row again
    /// and again. On real-1 the 8 534-row capture ended up carrying 589 080
    /// rows of descriptor — 29.9 MB where the content needs 0.45 MB — most of
    /// them `false` in `filled`. Every delta was still right (the search reads
    /// logical rows), so only a memory measurement found it.
    #[test]
    fn the_engine_canvas_tracks_logical_rows_not_the_rebased_pixel_canvas() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let height = 60;
        let first = frame_from_signed(100, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // Three upward steps, each one prepending in the pixel canvas exactly
        // as the stitcher reports it: the pixel row stays 0 while the logical
        // row goes 0 → -20 → -40 → -60.
        let mut committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let frames: Vec<RgbFrame> = (1..=3).map(|step| frame_from_signed(100 - 20 * step, height)).collect();
        for (step, frame) in frames.iter().enumerate() {
            let logical = -20 * (step as i32 + 1);
            let request = request_on_canvas(frame, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
            assert!(
                matches!(engine.place(&request), EngineDecision::Place { .. }),
                "step {step} must be placeable"
            );
            // rect.y is where the *pixel* canvas put it: at the top, as after
            // a prepend.
            engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, logical, frame);
            committed.push((logical, frame));
        }

        let canvas = engine.canvas.as_ref().expect("canvas exists");
        assert_eq!(
            canvas.filled.len(),
            (3 * 20 + height) as usize,
            "the descriptor canvas must hold exactly the rows the capture covered"
        );
        // Every frame is still readable at its own logical row, and no other
        // row fits it better. (The frame's own top and bottom rows read
        // differently from the same content written by another frame's
        // interior — the descriptor is a vertical derivative — so this compares
        // rows against each other rather than against zero.) One gain for the
        // whole session, as the engine does it: ladders built with a per-frame
        // gain are quantised on a different scale.
        let gain = EdgeLadder::session_gain(&first);
        for (logical, frame) in committed.iter() {
            let frame: &RgbFrame = frame;
            let ladder = EdgeLadder::from_source(frame, gain).unwrap();
            let candidates: Vec<(f64, i32)> = (logical - 60..=*logical + 60)
                .filter_map(|top| canvas.cost_at(&ladder, top, 1).map(|score| (score.cost, top)))
                .collect();
            let best = candidates
                .iter()
                .copied()
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap_or_else(|| panic!("no evidence anywhere near {logical}"));
            assert_eq!(
                best.1, *logical,
                "row {logical} must hold its own frame, but row {} fits it better ({})",
                best.1, best.0
            );
        }
    }

    /// A frame that matches nothing is refused without moving anything, and
    /// the next frame that does match is placed: the search is rebuilt from the
    /// canvas every time, so one bad frame cannot stall the capture.
    #[test]
    fn a_refused_frame_does_not_stop_the_next_one() {
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let height = 60;
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];

        // No overlap with the canvas at all: nothing to compare against.
        let nowhere = frame_from(900, height);
        let req =
            request_on_canvas(&nowhere, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));
        assert_eq!(engine.pos, Some(0), "a refusal must not move the position");

        // The next frame overlaps again and is placed.
        let back = frame_from(10, height);
        let req =
            request_on_canvas(&back, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        let EngineDecision::Place { rect, .. } = engine.place(&req) else {
            panic!("the next overlapping frame must be placed");
        };
        assert_eq!(rect.y, 10);
    }

    /// The guard window is the frame height, both directions, and it does not
    /// depend on how tall the canvas has grown.
    ///
    /// `rows`, not `rows - minimum_overlap`: two frames of height `rows`
    /// overlap iff `|d| < rows`, and whether that overlap carries enough
    /// boundaries is decided later, on the candidate's score.
    #[test]
    fn the_guard_window_is_the_frame_height_in_both_directions() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let ladder = EdgeLadder::from_source(&first, engine.session_gain).unwrap();
        assert_eq!(engine.adjacency_radius(&ladder), 60);

        // A taller canvas must not change it.
        let second = frame_from(60, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 60, &second);
        let third = frame_from(120, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 120, &third);
        assert_eq!(
            engine.adjacency_radius(&ladder),
            60,
            "the window is a property of the frames, not of the canvas"
        );
    }

    /// The anchor's reach is every row where the frame still shares
    /// `minimum_overlap` rows with the canvas, and nothing beyond it.
    #[test]
    fn the_anchor_reach_is_bounded_by_the_canvas_evidence() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let ladder = EdgeLadder::from_source(&first, engine.session_gain).unwrap();

        let (lo, hi) = engine.canvas_bounds(&ladder).expect("canvas exists");
        let min_overlap = crate::stitch::minimum_overlap(height, height).max(1) as i32;
        // Canvas rows 0..60, frame height 60: the frame's top may run from
        // `-60 + min_overlap` to `60 - min_overlap`.
        assert_eq!(lo, -(height as i32) + min_overlap);
        assert_eq!(hi, height as i32 - min_overlap);

        // One row beyond either end leaves too little evidence to score.
        let canvas = engine.canvas.as_ref().unwrap();
        assert!(canvas.cost_at(&ladder, lo, min_overlap as u32).is_some());
        assert!(canvas.cost_at(&ladder, lo - 1, min_overlap as u32).is_none());
        assert!(canvas.cost_at(&ladder, hi, min_overlap as u32).is_some());
        assert!(canvas.cost_at(&ladder, hi + 1, min_overlap as u32).is_none());
    }

    /// A frame that the guard can place is placed by the guard, even when the
    /// border is red: recovery must not take precedence over continuation.
    ///
    /// Row 90 overlaps the previous frame (60..120) by 30 rows, so it is a
    /// legitimate continuation and must be placed there rather than being
    /// re-anchored to some earlier position.
    #[test]
    fn a_continuation_wins_over_recovery_even_with_a_red_border() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let second = frame_from(60, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 60, &second);
        assert_eq!(engine.pos, Some(60));

        let next = frame_from(90, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first), (60, &second)];
        let mut req =
            request_on_canvas(&next, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        req.refusals = crate::stitch::REFUSAL_PATIENCE;
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => assert_eq!(rect.y, 90),
            other => panic!("the continuation must be placed: {other:?}"),
        }
    }

    /// The colour check must read the canvas rows the frame would occupy, at
    /// every candidate offset.
    ///
    /// This is the mapping `canvas_row = top - stitched.anchor_origin_y`. It was
    /// once derived from `previous_rect.y + (top - engine_pos)`, which agrees
    /// only while the previous frame's pixel row happens to equal its engine
    /// row — true for a canvas that never prepends, false otherwise. The
    /// failure was silent: structure scored 0.0000 at the true row while the
    /// colour check read unrelated rows and refused it.
    #[test]
    fn the_colour_check_reads_the_rows_the_frame_would_occupy() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        // Canvas initialised at logical 0, then grown upward: the second commit
        // prepends rows, so pixel row 0 stops being logical row 0. That is the
        // case a mapping derived from anything but the commit pair gets wrong.
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let above = frame_from_signed(-60, height); // page rows -60..0
        // Committed the way the stitcher does it after a prepend: the engine is
        // told the *logical* row (-60) while the pixel rect is re-based to 0.
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, -60, &above);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first), (-60, &above)];
        let req = request_on_canvas(
            &above,
            ViewportRect { x: 0, y: 0, width: W, height },
            &committed,
        );
        assert_eq!(
            req.previous_rect.y, 0,
            "the prepended frame is at pixel row 0, while the engine's logical row is -60"
        );
        assert_eq!(engine.pos, Some(-60), "the engine tracks logical rows");

        // The frame is a copy of the prepended content, so at its own logical
        // row the canvas holds exactly those pixels...
        let at_truth = engine.colour_cost_at(&req, -60).expect("rows exist");
        assert!(
            at_truth < 1.0,
            "the frame's own row must read as identical, got {at_truth}"
        );
        // ...and one viewport lower it is a different page area.
        let at_wrong = engine.colour_cost_at(&req, 0).expect("rows exist");
        assert!(
            at_wrong > at_truth + 10.0,
            "an unrelated row must differ, got {at_wrong}"
        );
    }

    /// The score cache must not change any decision: it only avoids recomputing
    /// a cost that is fixed for the call.
    #[test]
    fn the_score_cache_returns_the_same_scores() {
        let height = 60;
        let first = frame_from(0, height);
        let mut cache = ScoreCache::default();
        let canvas = {
            let mut e = WindowedEdgeMatcher::new(SearchAxis::Vertical);
            e.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
            e.canvas.clone().expect("canvas")
        };
        let ladder = EdgeLadder::from_source(&first, EdgeLadder::session_gain(&first)).unwrap();
        for top in -5..5 {
            let direct = canvas.cost_at(&ladder, top, 16);
            let cached_first = cache.cost(&canvas, &ladder, top, 16);
            let cached_again = cache.cost(&canvas, &ladder, top, 16);
            assert_eq!(direct.map(|s| s.cost), cached_first.map(|s| s.cost));
            assert_eq!(cached_first.map(|s| s.cost), cached_again.map(|s| s.cost));
        }
    }

    /// The guard band is centred on the previous frame while the impulse window
    /// is centred on the prediction, so the guard must search its whole band —
    /// not just the part outside the impulse window.
    ///
    /// A position the impulse window rejected (its candidate set is too small to
    /// produce a runner-up) has to remain reachable from the guard, or it is lost
    /// for good.
    #[test]
    fn the_guard_band_is_not_cut_by_the_impulse_window() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        // A step of 44, so the prediction sits at 44 while the previous frame is
        // at 0 and the guard band is 0 ± 34.
        engine.last_delta = Some(44);
        engine.trend = 0.0;

        let moved = frame_from(44, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&moved, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        // Row 44 is inside both windows; the point is that the guard band is
        // scored as a whole, so this placement must not depend on which stage
        // finds it.
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => assert_eq!(rect.y, 44),
            other => panic!("the guard band must be searched whole: {other:?}"),
        }
    }

    /// The anchor stage is recovery, not a second opinion: it must not run
    /// until the border is already red.
    ///
    /// Two frames 60 rows apart, so the guard band (`pos ± (rows -
    /// minimum_overlap)`, 34 rows here) excludes row 10 while the canvas still
    /// covers it. With no refusal streak the frame is refused — it is not a
    /// continuation of the previous one — and the same frame is placed once the
    /// streak reaches `REFUSAL_PATIENCE`, which is the moment the UI turns the
    /// border red.
    #[test]
    fn the_anchor_stage_waits_for_the_red_border() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        // A canvas three viewports tall: rows 0..180.
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let second = frame_from(60, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 60, &second);
        let third = frame_from(120, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 120, &third);
        assert_eq!(engine.pos, Some(120));

        // Row 10: the canvas covers it, but the guard band (120 ± 60, i.e.
        // 60..180) does not — the frame is not a continuation of the previous
        // one, it is content from further up the page.
        let back = frame_from(10, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first), (60, &second), (120, &third)];
        let mut req =
            request_on_canvas(&back, ViewportRect { x: 0, y: 0, width: W, height }, &committed);

        req.refusals = 0;
        assert!(
            matches!(engine.place(&req), EngineDecision::Reject { .. }),
            "the anchor must not run before the border is red"
        );

        req.refusals = crate::stitch::REFUSAL_PATIENCE;
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => assert_eq!(rect.y, 10),
            other => panic!("recovery must place the frame: {other:?}"),
        }
    }

    /// Both directions are ordinary. A capture that scrolls back up must be
    /// predicted, guarded and placed exactly like one that scrolls down: the
    /// impulse window extrapolates a negative step, the guard band is symmetric
    /// around the previous frame, and the frame lands *above* it.
    ///
    /// real-1 is a scroll-up corpus (total displacement -8020 px, 112 negative
    /// steps), so this path is exercised by the corpora as well; this test pins
    /// the geometry so a future change cannot make the window one-sided.
    #[test]
    fn scrolling_back_up_is_placed_like_scrolling_down() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        // Start deep in the page and walk upward in half-viewport steps, so
        // consecutive frames really do overlap (a full-viewport step leaves
        // them touching, with zero rows in common).
        let start = frame_from(600, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 600, &start);
        let mut committed: Vec<(i32, RgbFrame)> = vec![(600, start)];
        for top in [570i32, 540, 510] {
            let frame = frame_from(top as u32, height);
            let refs: Vec<(i32, &RgbFrame)> = committed.iter().map(|(y, f)| (*y, f)).collect();
            let req =
                request_on_canvas(&frame, ViewportRect { x: 0, y: 0, width: W, height }, &refs);
            match engine.place(&req) {
                EngineDecision::Place { rect, .. } => {
                    assert_eq!(rect.y, top, "backward step to {top} must be placed there")
                }
                other => panic!("backward step to {top} refused: {other:?}"),
            }
            engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, top, &frame);
            committed.push((top, frame));
        }
    }

    /// A move outside the impulse window is still placed by the guard, which
    /// searches everything the canvas can still compare, nearest first.
    #[test]
    fn a_move_beyond_the_impulse_window_is_placed_by_the_guard() {
        let height = 600;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // 340 rows: beyond the first ring (320), inside the canvas overlap.
        let moved = frame_from(340, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&moved, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        match engine.place(&req) {
            EngineDecision::Place { rect, .. } => assert_eq!(rect.y, 340),
            other => panic!("the guard must place a 340-row move: {other:?}"),
        }
    }

    /// The first anchor ring has no hole: it must be able to answer "the
    /// content did not move". The code skipped the centre (`|top - centre| <=
    /// inner` with `inner == 0`), which turned the first ring into an annulus
    /// and contradicted the ring's own contract.
    #[test]
    fn the_first_anchor_ring_includes_its_centre() {
        let height = 60;
        let mut engine = WindowedEdgeMatcher::new(SearchAxis::Vertical);
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let same = frame_from(0, height);
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &first)];
        let req = request_on_canvas(&same, ViewportRect { x: 0, y: 0, width: W, height }, &committed);
        let ladder = EdgeLadder::from_source(&same, engine.session_gain).unwrap();
        let mut cache = ScoreCache::default();
        match engine.decide_ring(&mut cache, &ladder, 0, 0, 40, (-40, 40), &[], &req) {
            DecideOutcome::Accept { top, .. } => assert_eq!(top, 0, "the centre must win"),
            DecideOutcome::Reject(_) => panic!("the centre must be a candidate at all"),
        }
    }

    fn hashed_page(x: u32, y: u32) -> u8 {
        let mut hash = (x / 6).wrapping_mul(0x9E37_79B1) ^ (y / 6).wrapping_mul(0x85EB_CA77);
        hash ^= hash >> 15;
        hash = hash.wrapping_mul(0x2545_F491);
        hash ^= hash >> 13;
        (hash & 0xFF) as u8
    }

    fn page_crop(page_x: u32, page_y: u32, width: u32, height: u32) -> RgbFrame {
        let stride = width * 3;
        let mut data = Vec::with_capacity((stride * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let value = hashed_page(page_x + x, page_y + y);
                data.extend_from_slice(&[value, value, value]);
            }
        }
        RgbFrame {
            width,
            height,
            stride,
            data,
        }
    }

    #[test]
    fn horizontal_placement_reads_the_step_along_the_axis() {
        let canvas = StitchedFrame::from_first_frame(&page_crop(0, 0, 240, 60));
        let rect = ViewportRect {
            x: 0,
            y: 0,
            width: 240,
            height: 60,
        };
        let moved = page_crop(30, 0, 240, 60);
        match place_horizontally(&canvas, rect, &moved, Some(SearchDirection::Right)) {
            EngineDecision::Place { rect, match_info } => {
                assert_eq!(rect.x, 30);
                assert_eq!(match_info.delta_x, 30);
                assert_eq!(match_info.delta_y, 0);
                assert_eq!(match_info.direction, AppendDirection::Right);
            }
            other => panic!("a 30 px step must be placed, got {other:?}"),
        }
    }

    /// Only the direction the user indicated is searched. The same frame,
    /// asked the other way, has no answer and says so.
    #[test]
    fn horizontal_placement_searches_only_the_indicated_direction() {
        let canvas = StitchedFrame::from_first_frame(&page_crop(0, 0, 240, 60));
        let rect = ViewportRect {
            x: 0,
            y: 0,
            width: 240,
            height: 60,
        };
        let moved = page_crop(30, 0, 240, 60);
        assert!(matches!(
            place_horizontally(&canvas, rect, &moved, Some(SearchDirection::Left)),
            EngineDecision::Reject { .. }
        ));
    }

    /// Content that is uniform along the axis carries no evidence, and the
    /// horizontal matcher refuses it for the same reason the vertical one
    /// does — not because a pixel difference happened to be small.
    #[test]
    fn horizontal_placement_refuses_content_without_horizontal_boundaries() {
        let canvas = StitchedFrame::from_first_frame(&page_crop(0, 0, 240, 60));
        let rect = ViewportRect {
            x: 0,
            y: 0,
            width: 240,
            height: 60,
        };
        // Rows are constant across x: the page has no vertical edges at all.
        let stripe = |value: u8| {
            let stride = 240 * 3;
            RgbFrame {
                width: 240,
                height: 60,
                stride,
                data: (0..60)
                    .flat_map(|_| std::iter::repeat_n(value, (stride) as usize))
                    .collect(),
            }
        };
        assert!(matches!(
            place_horizontally(&canvas, rect, &stripe(120), Some(SearchDirection::Right)),
            EngineDecision::Reject {
                reason: RejectReason::NoBoundaries
            }
        ));
    }

    #[test]
    fn scripted_engine_satisfies_the_interface_contract() {
        let frame = frame_from(0, 40);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 40 };
        let committed: Vec<(i32, &RgbFrame)> = vec![(0, &frame)];
        let req = request_on_canvas(&frame, rect, &committed);

        let mut engine = ScriptedEngine::new(vec![
            EngineDecision::Place {
                rect,
                match_info: FrameMatch {
                    direction: AppendDirection::Bottom,
                    overlap: 40,
                    delta_x: 0,
                    delta_y: 0,
                    score: 0.0,
                },
            },
            EngineDecision::Reject { reason: RejectReason::Ambiguous },
        ]);
        assert_eq!(engine.name(), "scripted");
        assert!(matches!(engine.place(&req), EngineDecision::Place { .. }));
        engine.commit(rect, 0, &frame);
        assert_eq!(engine.committed(), &[rect]);
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));
        engine.reset();
        assert_eq!(engine.resets(), 1);
    }

    fn crop(page: &RgbFrame, top: u32, height: u32) -> RgbFrame {
        let stride = page.stride;
        let start = top as usize * stride as usize;
        let len = height as usize * stride as usize;
        RgbFrame {
            width: page.width,
            height,
            stride,
            data: page.data[start..start + len].to_vec(),
        }
    }
}
