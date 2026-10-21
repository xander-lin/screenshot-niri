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
/// Minimum comparable boundary rows.
///
/// One: a single row of evidence is enough to compare. This is deliberately not
/// a quality precondition — horizontal stripe content has boundaries only at
/// run edges, so a 160-row viewport may carry two or three, and refusing those
/// frames because the page is not text-dense would be a judgement about content
/// the stitcher has no business making. Whether one row is *enough* to trust is
/// decided by the gates below (runner-up separation, rival count, winner vs
/// median), which are evidence-normalised and do not reward or punish density.
const EDGE_MIN_COMPARABLE_ROWS: u32 = 3;
/// Runner-up separation floor, `(second - best) / second`.
/// 0.30 leaves 13 silently misplaced frames on periodic content, 0.80 none.
const EDGE_MIN_SEPARATION: f64 = 0.45;
/// Self-normalised "no evidence" gate: the winner must be much better than a
/// typical candidate. Relative, so it does not punish sparse pages.
const EDGE_MAX_BEST_OVER_MEDIAN: f64 = 0.28;
/// A rival must rest on at least this fraction of the winner's evidence.
///
/// Periodic content repeats over the viewport, so a decoy compares as many
/// boundary rows as the truth: it passes and the frame is refused. A one-row
/// coincidence on stripe content compares far fewer and is discarded, so
/// correct placements are not refused for it.
const EDGE_RIVAL_EVIDENCE_NUMERATOR: u32 = 3;
const EDGE_RIVAL_EVIDENCE_DENOMINATOR: u32 = 4;
/// Positions closer than this to the winner are the same peak, not a rival.
const EDGE_ADJACENT_ROWS: i32 = 2;
/// A candidate whose cost is within this of the winner is "as good".
///
/// Used to count distinct equally-good answers: a periodic page has several,
/// and choosing one of them would be a silent misplacement.
const EDGE_RIVAL_COST_MARGIN: f64 = 0.02;
/// Divisor that turns the frame height into the winner's basin width.
///
/// A rival closer than this is the winner's own shoulder, not a second answer.
/// One eighth of the viewport is wide enough to cover the peak, and narrow
/// enough that a repeated pattern one card-height away still counts as a rival.
const EDGE_BASIN_DIVISOR: i32 = 8;
/// Impulse window half-width, and its scale with the previous delta.
const EDGE_IMPULSE_MIN_HALF_WIDTH: i32 = 24;
const EDGE_IMPULSE_SCALE_NUMERATOR: i32 = 6;
const EDGE_IMPULSE_SCALE_DENOMINATOR: i32 = 10;
/// Wide-window half-width around the impulse prediction.
const EDGE_WIDE_HALF_WIDTH: i32 = 1400;
/// Maximum single-frame displacement, as a fraction of the viewport height.
///
/// The generic overlap floor (10% of the viewport) is far too permissive for
/// this matcher: a 700-row viewport could accept a 630-row jump on 70 rows of
/// evidence, and on periodic content those 70 rows are trivially "matched" at
/// the wrong place. Measured on the synthetic corpus, allowing 630-row jumps
/// produced 51 silent misplacements on `table` and 97 on `cards2`, all of them
/// at long offsets after a refusal had stalled the canvas.
///
/// A real scroll advances by a fraction of a viewport between frames. 60%
/// keeps at least 40% of the frame as overlap, which is what makes the
/// separation gate meaningful in the first place.
const EDGE_MAX_DISPLACEMENT_NUMERATOR: i32 = 6;
const EDGE_MAX_DISPLACEMENT_DENOMINATOR: i32 = 10;

/// Anchor-recovery half-width around the last verified position.
const EDGE_ANCHOR_HALF_WIDTH: i32 = 2800;
/// How many refusals keep widening the displacement guard, in viewports.
///
/// Three viewports covers the gap a scroll animation can open before the next
/// clean frame arrives, and stays far below the anchor window so a stale
/// canvas cannot be mistaken for a match far away.
const EDGE_REFUSAL_WIDEN_STEPS: i32 = 3;

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

        // Contrast-relative quantisation: see gain below.
        let mut peak = 0f32;
        for y in 0..height as usize {
            for b in 0..bins_usize {
                let index = y * bins_usize + b;
                let current = means[index];
                let previous = if y == 0 { current } else { means[index - bins_usize] };
                peak = peak.max((current - previous).abs());
            }
        }
        // Levels are absolute so ladders from different frames stay
        // comparable with the calibrated gates. Content whose strongest
        // boundary never reaches EDGE_LEVEL_ONE (a two-gray page, delta 2)
        // would quantise to all-zero and every frame would be refused; for
        // those frames only, scale deltas so the strongest boundary lands on
        // the first level. The gain comes from the frame's own content, and
        // the overlapping canvas rows hold the same content, so both sides
        // scale alike.
        let gain = if peak > 0.0 && peak < EDGE_LEVEL_ONE {
            (EDGE_LEVEL_ONE / peak).min(64.0)
        } else {
            1.0
        };

        // Vertical difference: the boundary between row y-1 and row y.
        let mut levels = vec![0u8; height as usize * bins_usize];
        let mut row_mass = vec![0u16; height as usize];
        for y in 0..height as usize {
            for b in 0..bins_usize {
                let index = y * bins_usize + b;
                let current = means[index];
                let previous = if y == 0 { current } else { means[index - bins_usize] };
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
    /// Consecutive refusals since the last accepted frame.
    ///
    /// The displacement guard exists to stop a single bad match from jumping
    /// the canvas across the page. Once the engine has already refused, that
    /// guard has nothing left to protect: the canvas is frozen at the last
    /// verified row while the content keeps scrolling, so every further frame
    /// is legitimately further away. Without widening, one blurred frame
    /// (a scroll animation between two wheel steps) freezes the capture
    /// permanently.
    refusals_since_accept: u32,
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

        let mut costs: Vec<(f64, i32, u32)> = Vec::new();
        for top in lo..=hi {
            if let Some(score) = canvas.cost_at(ladder, top, min_overlap) {
                costs.push((score.cost, top, score.used));
            }
        }
        if costs.len() < 4 {
            return DecideOutcome::Reject(RejectReason::InsufficientEvidence);
        }

        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (best_cost, best_top, best_used) = costs[0];
        // A candidate further than this from the previous accepted position
        // leaves too little overlap for the separation gate to mean anything.
        let limit = self.max_displacement();
        if (best_top - self.pos.unwrap_or(best_top)).abs() > limit {
            if crate::trace::edge_gate_trace_enabled() {
                crate::trace_log!(
                    "stitch: edge candidate {best_cost:.4}@{best_top} refused, displacement over {limit}"
                );
            }
            return DecideOutcome::Reject(RejectReason::Ambiguous);
        }
        // The runner-up is the cheapest candidate outside the winner's basin.
        //
        // Skipping only `EDGE_ADJACENT_ROWS` rows finds the shoulder of the
        // winner's own peak, not a rival: on a smooth cost surface the next few
        // rows are always a little worse, which made `sep` look healthy even
        // when a second, equally good answer existed. The basin is therefore
        // widened, so a candidate one period away (a 235-row card grid) counts
        // as the rival it actually is.
        let basin = self.basin_rows();
        // The runner-up must rest on comparable evidence, for the same reason
        // as the rival count below: a one-row coincidence is not a competing
        // answer, and letting it define `second` makes `sep` read 0 on content
        // where the winner is in fact the only candidate with evidence.
        let second = costs
            .iter()
            .find(|(_, top, _)| (top - best_top).abs() > basin)
            .map(|(cost, _, _)| *cost);
        // How many distinct places match about as well as the winner.
        //
        // This is the periodic-content guard. A card grid repeats every 235
        // rows inside a 700-row viewport, so three offsets match at cost 0.000;
        // the separation gate cannot see it because every other offset is much
        // worse, so `sep` reads 1.000 while the answer is genuinely ambiguous.
        // Counting equally-good places refuses those frames instead of picking
        // one at random. A rival only counts when it rests on comparable
        // evidence: periodic content repeats over the whole viewport, so the
        // offset one period away compares as many boundary rows as the truth
        // and is a genuine second answer. Stripe content has boundaries only at
        // run edges, so an unrelated offset can tie on cost by agreeing on a
        // single row while the truth agrees on four — that tie is noise, and
        // counting it refuses every frame.
        let mut rival_tops: Vec<i32> = Vec::new();
        for (cost, top, _) in &costs {
            if *cost > best_cost + EDGE_RIVAL_COST_MARGIN {
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
        let equally_good_rivals = rival_tops.len();
        let mut sorted_costs: Vec<f64> = costs.iter().map(|(cost, _, _)| *cost).collect();
        sorted_costs.sort_by(|a, b| a.total_cmp(b));
        let median = sorted_costs[sorted_costs.len() / 2];

        let separation = match second {
            Some(second) if second > 0.0 => (second - best_cost) / second,
            // A rival scoring exactly zero means two identical answers exist.
            Some(_) => 0.0,
            None => 0.0,
        };
        if crate::trace::edge_gate_trace_enabled() {
            crate::trace_log!(
                "stitch: edge gate window={lo}..={hi} best={best_cost:.4}@{best_top} \
                 second={second:?} rivals={equally_good_rivals} sep={separation:.3} \
                 median={median:.4} best_over_median={:.3}",
                if median > 0.0 { best_cost / median } else { f64::INFINITY }
            );
        }
        if equally_good_rivals > 0 {
            return DecideOutcome::Reject(RejectReason::Ambiguous);
        }
        if separation < EDGE_MIN_SEPARATION {
            return DecideOutcome::Reject(RejectReason::Ambiguous);
        }
        if median <= 0.0 || best_cost / median > EDGE_MAX_BEST_OVER_MEDIAN {
            return DecideOutcome::Reject(RejectReason::NoCandidate);
        }
        DecideOutcome::Accept { top: best_top, cost: best_cost }
    }

    fn basin_rows(&self) -> i32 {
        // A peak is a few rows wide (a one-row shift multiplies the cost ~47x),
        // so anything inside this is the same answer. Scaled with the frame so
        // a large viewport does not treat its own peak as a rival.
        (self.frame_rows as i32 / EDGE_BASIN_DIVISOR).max(EDGE_ADJACENT_ROWS)
    }

    fn max_displacement(&self) -> i32 {
        let rows = self.frame_rows as i32;
        let base = (rows * EDGE_MAX_DISPLACEMENT_NUMERATOR / EDGE_MAX_DISPLACEMENT_DENOMINATOR)
            .max(EDGE_IMPULSE_MIN_HALF_WIDTH);
        // While refusing, the content keeps moving away from the frozen canvas.
        // Each refusal buys one more viewport of reach, up to the anchor
        // window, so recovery is possible without ever letting a *first*
        // match jump the page.
        let extra = self
            .refusals_since_accept
            .min(EDGE_REFUSAL_WIDEN_STEPS as u32) as i32
            * rows;
        base.saturating_add(extra)
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
        self.refusals_since_accept = 0;
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

        // Stage 4: refuse. Nothing moves, but the displacement guard has to
        // widen: the canvas stays frozen while the content keeps scrolling, so
        // each refusal leaves the next frame legitimately further away.
        self.refusals_since_accept = self.refusals_since_accept.saturating_add(1);
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
        // A prepend shifts every stored row down by `rect.y - requested_top`.
        //
        // `rect.y` is the frame's row in the *pixel canvas*, which grows
        // upward; `requested_top` is where the engine decided the frame
        // belongs in logical canvas coordinates. Everything the engine stores
        // (`pos`, `anchor`, the ladder) lives in logical coordinates, so the
        // ladder must be written at `requested_top` and the prepend only
        // re-bases the buffer.
        let shift = rect.y - requested_top;
        if shift > 0 {
            canvas.shift_down(shift);
        }

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
        let mut engine = WindowedEdgeMatcher::new();
        let first = stripe_frame(&page, 0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        for (top, expected) in [(60u32, 60), (120, 120)] {
            let frame = stripe_frame(&page, top, height);
            let req = request(&frame, ViewportRect { x: 0, y: 0, width: W, height });
            match engine.place(&req) {
                EngineDecision::Place { rect, .. } => assert_eq!(rect.y, expected),
                other => panic!("frame at {top} refused: {other:?}"),
            }
            engine.commit(ViewportRect { x: 0, y: expected, width: W, height }, expected, &frame);
        }
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

    /// The pixel canvas grows upward by prepending rows when a frame extends it
    /// past row 0, and `commit` receives both the logical row the engine asked
    /// for (`requested_top`) and the row the pixel canvas ended up using
    /// (`rect.y`). The engine's own state must stay in logical coordinates:
    /// storing the re-based pixel row made every later frame compare against
    /// content shifted by the whole prepend, and the canvas silently stopped
    /// growing.
    #[test]
    fn prepending_rows_keeps_the_engine_in_logical_coordinates() {
        let mut engine = WindowedEdgeMatcher::new();
        let height = 60;

        // Canvas starts covering page rows 100..=159 at logical row 0.
        let first = frame_from_signed(100, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        // Scroll up by 17: the frame now starts at page row 83, i.e. 17 rows
        // above the canvas top, so it belongs at logical row -17. The pixel
        // canvas grows upward, so it prepends 17 rows and this frame lands at
        // pixel row 0 while the engine asked for -17.
        let upward = frame_from_signed(83, height);
        let mut req = request(&upward, ViewportRect { x: 0, y: 0, width: W, height });
        req.stitched = Box::leak(Box::new(canvas_of(height)));
        let EngineDecision::Place { match_info, .. } = engine.place(&req) else {
            panic!("expected the upward frame to be placed");
        };
        assert_eq!(match_info.delta_y, -17);

        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, -17, &upward);

        // The engine must report the logical row it decided on, not the pixel
        // row the canvas re-based to.
        assert_eq!(engine.pos, Some(-17));

        // A further upward frame must be found 17 rows above that, at -34.
        let further = frame_from_signed(66, height);
        let mut req = request(&further, ViewportRect { x: 0, y: 0, width: W, height });
        req.stitched = Box::leak(Box::new(canvas_of(height)));
        let EngineDecision::Place { match_info, .. } = engine.place(&req) else {
            panic!("expected the second upward frame to be placed");
        };
        assert_eq!(match_info.delta_y, -17);
    }

    /// A frame placed by prepending must actually be readable at the logical
    /// row it was placed at. Before the fix the write landed inside rows that
    /// were wrongly marked as already filled, so the new frame never entered
    /// the canvas and the engine kept comparing against stale content.
    #[test]
    fn a_prepended_frame_is_written_into_the_canvas() {
        let mut engine = WindowedEdgeMatcher::new();
        let height = 60;
        let first = frame_from_signed(100, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);

        let upward = frame_from_signed(83, height);
        let mut req = request(&upward, ViewportRect { x: 0, y: 0, width: W, height });
        req.stitched = Box::leak(Box::new(canvas_of(height)));
        assert!(matches!(engine.place(&req), EngineDecision::Place { .. }));
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, -17, &upward);

        let canvas = engine.canvas.as_ref().expect("canvas exists");
        let ladder = EdgeLadder::from_source(&upward).unwrap();
        let cost = canvas.cost_at(&ladder, -17, 1).expect("evidence at -17");
        assert!(
            cost.cost < 0.01,
            "the prepended frame must be readable at its logical row, got {}",
            cost.cost
        );
        // The rows it did not cover must still hold the older content.
        let old = EdgeLadder::from_source(&first).unwrap();
        let cost = canvas.cost_at(&old, 0, 1).expect("evidence at 0");
        assert!(
            cost.cost < 0.01,
            "the original content must survive the prepend, got {}",
            cost.cost
        );
    }

    /// A single blurred frame (a scroll animation caught mid-flight) must not
    /// freeze the capture permanently. While refusing, the content keeps moving
    /// away from the frozen canvas, so the displacement guard has to widen —
    /// otherwise every later frame is refused for being too far away and the
    /// canvas never grows again.
    #[test]
    fn refusals_widen_the_displacement_guard_so_capture_recovers() {
        let mut engine = WindowedEdgeMatcher::new();
        let height = 60;
        let first = frame_from(0, height);
        engine.commit(ViewportRect { x: 0, y: 0, width: W, height }, 0, &first);
        let base = engine.max_displacement();
        assert_eq!(engine.refusals_since_accept, 0);

        // A frame that matches nowhere: the engine refuses and counts it.
        let nowhere = frame_from(900, height);
        let mut req = request(&nowhere, ViewportRect { x: 0, y: 0, width: W, height });
        req.stitched = Box::leak(Box::new(canvas_of(height)));
        assert!(matches!(engine.place(&req), EngineDecision::Reject { .. }));
        assert_eq!(engine.refusals_since_accept, 1);
        assert!(
            engine.max_displacement() > base,
            "the guard must widen after a refusal, base={base} now={}",
            engine.max_displacement()
        );

        // Accepting again restores the strict guard.
        let back = frame_from(10, height);
        let mut req = request(&back, ViewportRect { x: 0, y: 0, width: W, height });
        req.stitched = Box::leak(Box::new(canvas_of(height)));
        assert!(matches!(engine.place(&req), EngineDecision::Place { .. }));
        assert_eq!(engine.refusals_since_accept, 0);
        assert_eq!(engine.max_displacement(), base);
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
