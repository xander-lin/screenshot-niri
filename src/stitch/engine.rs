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

use super::rgb::RgbSource;
use super::{minimum_overlap, AppendDirection, FrameMatch, SearchDirection, StitchedFrame, ViewportRect};

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
/// Minimum comparable boundary rows: a statistical precondition, not a quality
/// judgement. Sparse pages are not a problem; too little evidence is.
const EDGE_MIN_COMPARABLE_ROWS: u32 = 8;
/// Runner-up separation floor, `(second - best) / second`.
/// 0.30 leaves 13 silently misplaced frames on periodic content, 0.80 none.
const EDGE_MIN_SEPARATION: f64 = 0.80;
/// Self-normalised "no evidence" gate: the winner must be much better than a
/// typical candidate. Relative, so it does not punish sparse pages.
const EDGE_MAX_BEST_OVER_MEDIAN: f64 = 0.10;
/// Positions closer than this to the winner are the same peak, not a rival.
const EDGE_ADJACENT_ROWS: i32 = 2;
/// Impulse window half-width, and its scale with the previous delta.
const EDGE_IMPULSE_MIN_HALF_WIDTH: i32 = 24;
const EDGE_IMPULSE_SCALE_NUMERATOR: i32 = 6;
const EDGE_IMPULSE_SCALE_DENOMINATOR: i32 = 10;
/// Wide-window half-width around the impulse prediction.
const EDGE_WIDE_HALF_WIDTH: i32 = 1400;
/// Anchor-recovery half-width around the last verified position.
const EDGE_ANCHOR_HALF_WIDTH: i32 = 2800;

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
    /// Builds the ladder for a frame.
    ///
    /// The margin exists so a moving scrollbar never enters the comparison; it
    /// is a fixed fraction, not a detection step.
    pub(super) fn from_source(frame: &dyn RgbSource) -> Option<Self> {
        let width = frame.width();
        let height = frame.height();
        if width == 0 || height == 0 {
            return None;
        }

        let margin = width * EDGE_MARGIN_NUMERATOR / EDGE_MARGIN_DENOMINATOR;
        let body_start = margin.min(width);
        let body_end = width.saturating_sub(margin).max(body_start);
        let body_width = body_end - body_start;
        if body_width == 0 {
            return None;
        }
        let bins = EDGE_BINS.min(body_width as usize) as u32;
        let bin_width = body_width / bins;
        let bins_usize = bins as usize;

        // Per-row, per-bin luminance means.
        let mut means = vec![0f32; height as usize * bins_usize];
        for y in 0..height {
            for b in 0..bins {
                let x0 = body_start + b * bin_width;
                let x1 = if b + 1 == bins { body_end } else { x0 + bin_width };
                let mut sum = 0u32;
                let mut count = 0u32;
                for x in x0..x1 {
                    let [r, g, bl] = frame.pixel_rgb(x, y);
                    sum += 77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(bl);
                    count += 1;
                }
                let mean = if count == 0 { 0.0 } else { (sum >> 8) as f32 / count as f32 };
                means[y as usize * bins_usize + b as usize] = mean;
            }
        }

        // Vertical difference: the boundary between row y-1 and row y.
        let mut levels = vec![0u8; height as usize * bins_usize];
        let mut row_mass = vec![0u16; height as usize];
        for y in 0..height as usize {
            for b in 0..bins_usize {
                let index = y * bins_usize + b;
                let current = means[index];
                let previous = if y == 0 { current } else { means[index - bins_usize] };
                let delta = (current - previous).abs();
                let level = if delta >= EDGE_LEVEL_TWO {
                    2
                } else if delta >= EDGE_LEVEL_ONE {
                    1
                } else {
                    0
                };
                levels[index] = level;
                if level > 0 {
                    row_mass[y] += 1;
                }
            }
        }

        // Keep the rows that actually carry boundaries, strongest first, then
        // restore ascending order so scoring walks memory forwards.
        let mut order: Vec<u32> = (0..height).filter(|y| row_mass[*y as usize] > 0).collect();
        order.sort_by(|a, b| {
            row_mass[*b as usize]
                .cmp(&row_mass[*a as usize])
                .then_with(|| a.cmp(b))
        });
        order.truncate(EDGE_MAX_SCORING_ROWS);
        order.sort_unstable();

        Some(Self { rows: height, bins, levels, scoring_rows: order })
    }

    pub(super) fn rows(&self) -> u32 {
        self.rows
    }

    pub(super) fn bins(&self) -> u32 {
        self.bins
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

    /// Shifts every stored row down by `shift`, used when the pixel canvas
    /// prepends rows because a frame extended it upward.
    fn shift_down(&mut self, shift: i32) {
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
        self.origin_row -= shift as i32;
        self.filled_lo += shift as i32;
        self.filled_hi += shift as i32;
    }

    /// Writes `ladder` with its top at canvas row `top`, preserving rows that
    /// were already verified by an earlier frame.
    fn write(&mut self, top: i32, ladder: &EdgeLadder) {
        if ladder.bins != self.bins {
            return;
        }
        let bins = self.bins as usize;
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
        if used < EDGE_MIN_COMPARABLE_ROWS || denominator == 0 {
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
    /// Two positions scored almost the same; refusing beats guessing.
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
}

// ---------------------------------------------------------------------------
// Production engine: boundaries + impulse windows + explicit refusal
// ---------------------------------------------------------------------------

/// The engine described in `docs/edge-stitch-design.md` §5.
#[derive(Debug, Default)]
pub(super) struct WindowedEdgeMatcher {
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
    /// The ladder built by the most recent `place`, reused by `commit`.
    ///
    /// Building it costs about half of the per-frame time, and `place` is
    /// always followed by `commit` for the same frame, so rebuilding it there
    /// would double the descriptor cost for no information.
    pending_ladder: Option<EdgeLadder>,
}

impl WindowedEdgeMatcher {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Scans `lo..=hi` row by row and applies the three gates.
    ///
    /// Deliberately exhaustive inside the window: the correct offset is a
    /// one-row-wide basin (a one-row shift multiplies the cost ~47x), so any
    /// coarse step can step over it.
    fn decide(&self, ladder: &EdgeLadder, lo: i32, hi: i32) -> DecideOutcome {
        let Some(canvas) = self.canvas.as_ref() else {
            return DecideOutcome::Reject(RejectReason::Uninitialised);
        };
        if lo > hi {
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }
        let min_overlap = minimum_overlap(ladder.rows(), ladder.rows()).max(1);

        let mut costs: Vec<(f64, i32)> = Vec::new();
        for top in lo..=hi {
            if let Some(score) = canvas.cost_at(ladder, top, min_overlap) {
                costs.push((score.cost, top));
            }
        }
        if costs.len() < 4 {
            return DecideOutcome::Reject(RejectReason::InsufficientEvidence);
        }

        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (best_cost, best_top) = costs[0];
        let second = costs
            .iter()
            .find(|(_, top)| (top - best_top).abs() > EDGE_ADJACENT_ROWS)
            .map(|(cost, _)| *cost);
        let mut sorted_costs: Vec<f64> = costs.iter().map(|(cost, _)| *cost).collect();
        sorted_costs.sort_by(|a, b| a.total_cmp(b));
        let median = sorted_costs[sorted_costs.len() / 2];

        let separation = match second {
            Some(second) if second > 0.0 => (second - best_cost) / second,
            // A rival scoring exactly zero means two identical answers exist.
            Some(_) => 0.0,
            None => 0.0,
        };
        if separation < EDGE_MIN_SEPARATION {
            return DecideOutcome::Reject(RejectReason::Ambiguous);
        }
        if median <= 0.0 || best_cost / median > EDGE_MAX_BEST_OVER_MEDIAN {
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }
        DecideOutcome::Accept { top: best_top, cost: best_cost }
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
        let delta_y = top - pos;
        let overlap = self.frame_rows.saturating_sub(delta_y.unsigned_abs());
        self.pending_top = Some(top);
        EngineDecision::Place {
            rect: ViewportRect {
                x: request.previous_rect.x,
                y: top,
                width: request.previous_rect.width,
                height: request.previous_rect.height,
            },
            match_info: FrameMatch {
                direction: if delta_y < 0 {
                    AppendDirection::Top
                } else {
                    AppendDirection::Bottom
                },
                overlap,
                delta_x: 0,
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
        let Some(ladder) = EdgeLadder::from_source(request.frame) else {
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

        // Stage 1: impulse window. Cheap, and almost always enough.
        if self.last_delta.is_some() {
            let half = self.impulse_half_width();
            if let DecideOutcome::Accept { top, cost } =
                self.decide(&ladder, predicted_top - half, predicted_top + half)
            {
                return self.accept(request, ladder.clone(), top, cost);
            }
        }

        // Stage 2: wide window around the same prediction.
        if let DecideOutcome::Accept { top, cost } = self.decide(
            &ladder,
            predicted_top - EDGE_WIDE_HALF_WIDTH,
            predicted_top + EDGE_WIDE_HALF_WIDTH,
        ) {
            return self.accept(request, ladder.clone(), top, cost);
        }

        // Stage 3: anchor recovery. The anchor is frozen across refusals, so a
        // user who scrolls back finds the content again.
        if let Some(anchor) = self.anchor {
            if let DecideOutcome::Accept { top, cost } = self.decide(
                &ladder,
                anchor - EDGE_ANCHOR_HALF_WIDTH,
                anchor + EDGE_ANCHOR_HALF_WIDTH,
            ) {
                return self.accept(request, ladder.clone(), top, cost);
            }
        }

        // Stage 4: refuse. Nothing moves.
        EngineDecision::Reject { reason: RejectReason::Ambiguous }
    }

    fn commit(&mut self, rect: ViewportRect, requested_top: i32, frame: &dyn RgbSource) {
        let ladder = match self.pending_ladder.take() {
            Some(ladder) => ladder,
            None => match EdgeLadder::from_source(frame) {
                Some(ladder) => ladder,
                None => return,
            },
        };
        let Some(canvas) = self.canvas.as_mut() else {
            let mut canvas = EdgeCanvas::new(ladder.bins, 0);
            canvas.write(rect.y, &ladder);
            self.frame_rows = ladder.rows();
            self.canvas = Some(canvas);
            self.pos = Some(rect.y);
            self.anchor = Some(rect.y);
            self.pending_top = None;
            return;
        };
        if ladder.bins != canvas.bins {
            return;
        }
        // A prepend shifts every stored row down by `rect.y - requested_top`.
        let shift = rect.y - requested_top;
        if shift > 0 {
            canvas.shift_down(shift);
        }

        let previous = self.pos;
        canvas.write(rect.y, &ladder);
        self.frame_rows = ladder.rows();
        self.pos = Some(rect.y);
        self.anchor = Some(rect.y);
        if let Some(previous) = previous {
            let delta = rect.y - previous;
            let last = self.last_delta.unwrap_or(0);
            self.trend = 0.5 * self.trend + 0.5 * f64::from(delta - last);
            self.last_delta = Some(delta);
        }
        self.pending_top = None;
    }

    fn reset(&mut self) {
        *self = Self::default();
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
    use crate::stitch::RgbFrame;

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

    fn canvas_of(rows: u32) -> StitchedFrame {
        let frame = frame_from(0, rows);
        StitchedFrame::from_first_frame_with_layout(
            &frame,
            crate::stitch::ComposeCrop { x: 0, y: 0, width: W, height: rows },
            crate::stitch::FixedBands::default(),
        )
    }

    fn request<'a>(frame: &'a dyn RgbSource, rect: ViewportRect) -> PlacementRequest<'a> {
        PlacementRequest {
            stitched: Box::leak(Box::new(canvas_of(frame.height()))),
            previous_rect: rect,
            frame,
            search_direction: None,
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
        let ladder = EdgeLadder::from_source(&frame).unwrap();
        assert_eq!(ladder.rows(), 20);
        assert_eq!(ladder.scoring_rows, vec![10]);
    }

    #[test]
    fn uniform_frame_reports_no_boundaries_instead_of_guessing() {
        let mut engine = WindowedEdgeMatcher::new();
        seed(&mut engine, 0, 40);
        let flat = flat_frame(40);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 40 };
        let mut req = request(&flat, rect);
        req.stitched = Box::leak(Box::new(canvas_of(40)));
        assert_eq!(
            engine.place(&req),
            EngineDecision::Reject { reason: RejectReason::NoBoundaries }
        );
    }

    #[test]
    fn finds_a_small_forward_shift() {
        let mut engine = WindowedEdgeMatcher::new();
        seed(&mut engine, 0, 60);
        let frame = frame_from(17, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let mut req = request(&frame, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
        let EngineDecision::Place { rect: placed, match_info } = engine.place(&req) else {
            panic!("expected a placement for a 17-row shift");
        };
        assert_eq!(placed.y, 17);
        assert_eq!(match_info.delta_y, 17);
        assert!(match_info.score < 0.01);
    }

    #[test]
    fn refuses_when_the_frame_is_nowhere_in_the_canvas() {
        let mut engine = WindowedEdgeMatcher::new();
        seed(&mut engine, 0, 60);
        // 900 rows away: no overlap with a 60-row canvas.
        let frame = frame_from(900, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let mut req = request(&frame, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
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
        let la = EdgeLadder::from_source(&a).unwrap();
        let lb = EdgeLadder::from_source(&b).unwrap();
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
    fn periodic_content_is_refused_rather_than_matched_to_a_decoy() {
        // 12 identical rows repeated: many offsets score identically.
        let stride = W * 3;
        let mut data = Vec::new();
        for i in 0..120 {
            data.extend_from_slice(&row_texture(i % 12));
        }
        let page = RgbFrame { width: W, height: 120, stride, data };
        let mut engine = WindowedEdgeMatcher::new();
        engine.commit(
            ViewportRect { x: 0, y: 0, width: W, height: 60 },
            0,
            &crop(&page, 0, 60),
        );
        let frame = crop(&page, 24, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let mut req = request(&frame, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
        let decision = engine.place(&req);
        assert!(
            matches!(
                decision,
                EngineDecision::Reject { reason: RejectReason::Ambiguous }
                    | EngineDecision::Reject { reason: RejectReason::NoCandidate }
            ),
            "periodic content must not be accepted on a guess, got {decision:?}"
        );
    }

    #[test]
    fn rejection_does_not_move_the_anchor() {
        let mut engine = WindowedEdgeMatcher::new();
        seed(&mut engine, 0, 60);
        let anchor_before = engine.anchor;
        let pos_before = engine.pos;
        let delta_before = engine.last_delta;
        let trend_before = engine.trend;

        let frame = frame_from(2000, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };
        let mut req = request(&frame, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));

        assert_eq!(engine.anchor, anchor_before);
        assert_eq!(engine.pos, pos_before);
        assert_eq!(engine.last_delta, delta_before);
        assert_eq!(engine.trend, trend_before);
    }

    #[test]
    fn recovers_when_the_user_scrolls_back() {
        let mut engine = WindowedEdgeMatcher::new();
        seed(&mut engine, 0, 60);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 60 };

        let far = frame_from(2000, 60);
        let mut req = request(&far, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));

        // Scroll back into the committed content.
        let back = frame_from(30, 60);
        let mut req = request(&back, rect);
        req.stitched = Box::leak(Box::new(canvas_of(60)));
        let EngineDecision::Place { rect: placed, .. } = engine.place(&req) else {
            panic!("expected recovery once content overlaps the canvas again");
        };
        assert_eq!(placed.y, 30);
    }

    #[test]
    fn scripted_engine_satisfies_the_interface_contract() {
        let frame = frame_from(0, 40);
        let rect = ViewportRect { x: 0, y: 0, width: W, height: 40 };
        let mut req = request(&frame, rect);
        req.stitched = Box::leak(Box::new(canvas_of(40)));

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
