//! Strict, slow ground truth for a captured corpus.
//!
//! `replay-frames` judges the stitcher while it replays. This tool answers the
//! question that check cannot: **where should each frame have gone, given every
//! other frame in the session, and does the pixels' own answer even exist?**
//!
//! It is deliberately independent of the stitcher and deliberately slow:
//!
//! 1. every adjacent pair is matched at **every integer displacement, at full
//!    resolution** — no sampling, no early exit — and the whole *tie set* is
//!    kept, so "the pixels cannot tell these apart" stays a fact instead of
//!    collapsing to whichever offset happened to be smallest;
//! 2. the pairs are chained into one page (a tie is broken towards the
//!    neighbouring step, never by magnitude);
//! 3. each frame is re-checked against a canvas built from **all the other
//!    frames** (leave-one-out), so a misplaced frame cannot vote for its own
//!    mistake, and every tie-set alternative is scored too.
//!
//! Verdicts:
//!
//! * `determinate` — one position fits and no alternative is close: a stitcher
//!   must place the frame there (or report a duplicate when the step is zero);
//! * `ambiguous`   — several positions fit equally well: any of them is
//!   defensible, and so is a refusal;
//! * `no-consensus` — no position explains the pixels: motion blur, content that
//!   changed between two visits, or two renditions of the same page (a brightness
//!   jump) that the consensus averages together. The strict criterion abstains
//!   here; it is not a defect by itself;
//! * `edge`        — too little of the frame is covered by *other* frames to
//!   judge it (the first/last screenfuls).
//!
//! Usage: truth-frames --dir DIR [--window ROWS] [--tsv OUT] [--quiet]

use std::error::Error;
use std::path::PathBuf;

use screenshot_rs_rebuild::corpus::{load_frames, load_png, luminance};

/// Overlap (as a fraction of the frame's height) below which a displacement is
/// not considered: a 25% overlap is the least that still says something, and
/// the mosaic check is what keeps a weak pair from deciding alone.
const MIN_OVERLAP_DENOMINATOR: u32 = 4;
/// Costs within this of the best one are the same answer (u8 luminance units).
/// Content re-encoded through a screenshot lands within rounding of itself, so
/// this is quantisation noise, not a quality threshold.
const TIE_EPSILON: f32 = 0.25;
/// A second position this close to the chosen one is the same peak.
const PEAK_SEPARATION: i32 = 2;
/// Leave-one-out residual above which no position explains the pixels.
const UNPLACEABLE_COST: f32 = 2.0;
/// How much better a position must fit before it overrides the chain's own.
const ALT_MARGIN: f32 = 0.5;
/// A frame is only judged when at least this share of its pixels is covered by
/// other frames. 5% is a scroll step of nearly a whole viewport — the CDP
/// corpora step 600 rows on 700-row frames — and still a hundred rows of
/// evidence; at the first and last screenful there is none.
const MIN_COVERAGE_NUMERATOR: u32 = 1;
const MIN_COVERAGE_DENOMINATOR: u32 = 20;

struct Frame {
    lum: Vec<u8>,
    width: u32,
    height: u32,
    /// Page column of the frame's left edge.
    x: i32,
    /// Screen offset of the capture region, used to carry the page across a
    /// selection edit (where no pairwise cost exists).
    screen_y: i32,
}

/// Costs of one adjacent pair, at every displacement that keeps the minimum
/// overlap. `lo` is the smallest displacement in `costs`.
struct Pair {
    lo: i32,
    costs: Vec<f32>,
}

impl Pair {
    /// Every distinct displacement the pixels cannot tell apart.
    fn ties(&self) -> Vec<i32> {
        let Some(best) = self.costs.iter().copied().reduce(f32::min) else {
            return Vec::new();
        };
        let mut ties = Vec::new();
        for (index, &cost) in self.costs.iter().enumerate() {
            if cost > best + TIE_EPSILON {
                continue;
            }
            // Local minimum only: a plateau edge must not masquerade as a
            // second answer.
            let left = index.checked_sub(1).map(|i| self.costs[i]);
            let right = self.costs.get(index + 1).copied();
            let is_min = left.is_none_or(|c| c >= cost) && right.is_none_or(|c| c >= cost);
            if !is_min {
                continue;
            }
            let d = self.lo + index as i32;
            if ties.last().is_none_or(|last: &i32| (d - last).abs() > PEAK_SEPARATION) {
                ties.push(d);
            }
        }
        ties
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("truth-frames: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut dir: Option<PathBuf> = None;
    let mut window = 0i32;
    let mut tsv: Option<PathBuf> = None;
    let mut quiet = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" | "-d" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--window" => {
                window = args.next().ok_or("--window needs rows")?.parse()?;
            }
            "--tsv" => tsv = Some(PathBuf::from(args.next().ok_or("--tsv needs a path")?)),
            "--quiet" | "-q" => quiet = true,
            "--help" | "-h" => {
                println!("truth-frames --dir DIR [--window ROWS] [--tsv OUT] [--quiet]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let dir = dir.ok_or("--dir is required")?;
    let entries = load_frames(&dir)?;
    if entries.len() < 2 {
        return Err("a corpus needs at least two frames".into());
    }

    // 1. Decode once, as luminance.
    let base_x = entries.iter().map(|entry| entry.offset.x).min().unwrap_or(0);
    let mut frames = Vec::with_capacity(entries.len());
    for entry in &entries {
        let image = load_png(&entry.path)?;
        let lum = luminance(&image);
        frames.push(Frame {
            lum,
            width: image.width,
            height: image.height,
            x: entry.offset.x - base_x,
            screen_y: entry.offset.y,
        });
    }

    // 2. Exact pairwise costs, all displacements, in parallel over pairs.
    let pairs = pairwise_costs(&frames);
    if !quiet {
        let decided = pairs.iter().filter(|pair| pair.is_some()).count();
        println!("pairs: {decided} with a common geometry, {} geometry changes", pairs.len() - 1 - decided);
    }

    // 3. Chain into a page. A tie resolves towards the previous step: on a
    //    repeated page every member is equally consistent, and the capture did
    //    not teleport, so the near one is the honest reading. Magnitude is never
    //    the tie-breaker — it prefers "0" on any periodic page.
    let mut page = vec![0i32; frames.len()];
    let mut step = vec![0i32; frames.len()];
    let mut loose = vec![false; frames.len()];
    for (index, pair) in pairs.iter().enumerate().skip(1) {
        match pair {
            Some(pair) => {
                let ties = pair.ties();
                if ties.len() == 1 {
                    step[index] = ties[0];
                } else {
                    // The pair does not say how far the page moved: several
                    // displacements fit equally well (blank stretches, repeated
                    // rows). Advancing on a guess is what turns an ambiguity into
                    // thousands of rows of invented page, so the chain stays put
                    // and the frame is reported as undetermined — its own step is
                    // one of `ties`, and that is the honest answer.
                    loose[index] = true;
                    step[index] = 0;
                }
            }
            None => {
                step[index] = frames[index].screen_y - frames[index - 1].screen_y;
            }
        }
        page[index] = page[index - 1] + step[index];
    }

    // 4. Grounded runs. A pair whose tie set holds more than one displacement
    //    does not say how far the page moved, so the absolute page position is
    //    unknown from there on: chaining through it on a guess drifts by
    //    thousands of rows (measured on `real-2`: 9 276 invented page rows for a
    //    ~3 500-row page, and every later frame judged against a wrong offset).
    //    Each run of frames connected by *determined* steps is therefore its own
    //    coordinate system: inside it every relative position is exact, and the
    //    consensus is built from that run alone.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut run_start = 0;
    for (index, &is_loose) in loose.iter().enumerate().skip(1) {
        if is_loose {
            runs.push((run_start, index));
            run_start = index;
        }
    }
    runs.push((run_start, frames.len()));

    // 5. Verdicts, one run at a time.
    let mut rows = Vec::with_capacity(frames.len());
    let mut counts = [0u32; 5]; // determinate, ambiguous, no-consensus, inconsistent, edge
    let mut uncovered = 0u32;
    let mut largest_run = 0usize;
    for (run_begin, run_end) in runs {
        largest_run = largest_run.max(run_end - run_begin);
        let origin = page[run_begin];
        let local: Vec<i32> = page[run_begin..run_end].iter().map(|top| top - origin).collect();
        let run_frames = &frames[run_begin..run_end];
        let mut mosaic = Mosaic::build(run_frames, &local);
        // One robust round: a frame nothing can explain must not vote for its own
        // mistake. It is then judged against the consensus that excludes it.
        let first_pass: Vec<Option<f32>> = (0..run_frames.len())
            .map(|index| mosaic.loo_cost(run_frames, index, local[index]))
            .collect();
        let excluded: Vec<bool> = first_pass
            .iter()
            .map(|cost| cost.is_some_and(|value| value > UNPLACEABLE_COST))
            .collect();
        if excluded.iter().any(|flag| *flag) {
            mosaic = Mosaic::build_excluding(run_frames, &local, &excluded);
        }
        uncovered += mosaic.uncovered_rows();

        for index in 0..run_frames.len() {
            let frame_index = run_begin + index;
            let here = mosaic.loo_cost(run_frames, index, local[index]);
            let mut candidates: Vec<(i32, f32)> = vec![(local[index], here.unwrap_or(f32::INFINITY))];
            // Alternatives: this frame reached from its neighbours through the
            // tie sets, so a repeated patch the chain resolved one way is still
            // scored the other way.
            if let Some(pair) = pairs[frame_index].as_ref() {
                let previous = local[index] - step[frame_index];
                for tie in pair.ties() {
                    let position = previous + tie;
                    if (position - local[index]).abs() > PEAK_SEPARATION {
                        candidates.push((position, mosaic.loo_cost(run_frames, index, position).unwrap_or(f32::INFINITY)));
                    }
                }
            }
            if frame_index + 1 < frames.len() && !loose[frame_index + 1] {
                if let Some(pair) = pairs[frame_index + 1].as_ref() {
                    let next = local[index] + step[frame_index + 1];
                    for tie in pair.ties() {
                        let position = next - tie;
                        if (position - local[index]).abs() > PEAK_SEPARATION {
                            candidates.push((position, mosaic.loo_cost(run_frames, index, position).unwrap_or(f32::INFINITY)));
                        }
                    }
                }
            }
            for offset in 1..=window {
                for position in [local[index] - offset, local[index] + offset] {
                    if let Some(cost) = mosaic.loo_cost(run_frames, index, position) {
                        candidates.push((position, cost));
                    }
                }
            }

            let coverage = mosaic.coverage(&run_frames[index], local[index]);
            let judged = coverage * MIN_COVERAGE_DENOMINATOR as f32 >= MIN_COVERAGE_NUMERATOR as f32;
            // The frame's own step is undetermined whatever the mosaic says:
            // report that first, so a short run (where the consensus has nothing
            // to compare with) does not bury it under `edge`.
            if loose[frame_index] {
                let alt = pairs[frame_index]
                    .as_ref()
                    .and_then(|pair| pair.ties().into_iter().find(|tie| *tie != 0));
                rows.push(Row { index: frame_index, page: page[frame_index], delta: step[frame_index], verdict: Verdict::Ambiguous, cost: here, alt, coverage });
                counts[1] += 1;
                continue;
            }
            let Some(best) = candidates
                .iter()
                .filter(|(_, cost)| cost.is_finite())
                .min_by(|a, b| a.1.total_cmp(&b.1))
            else {
                rows.push(Row { index: frame_index, page: page[frame_index], delta: step[frame_index], verdict: Verdict::Edge, cost: None, alt: None, coverage });
                counts[4] += 1;
                continue;
            };

            let (verdict, alt) = if !judged {
                (Verdict::Edge, None)
            } else if best.1 > UNPLACEABLE_COST {
                (Verdict::NoConsensus, None)
            } else {
                let mut fitting: Vec<i32> = candidates
                    .iter()
                    .filter(|(position, cost)| {
                        cost.is_finite()
                            && *cost <= best.1 + ALT_MARGIN
                            && (*position - best.0).abs() > PEAK_SEPARATION
                    })
                    .map(|(position, _)| *position)
                    .collect();
                fitting.sort_unstable();
                fitting.dedup();
                if fitting.is_empty() {
                    let moved = (best.0 - local[index]).abs() > PEAK_SEPARATION;
                    if moved && best.1 + ALT_MARGIN < here.unwrap_or(f32::INFINITY) {
                        (Verdict::Inconsistent, Some(best.0 + origin))
                    } else {
                        (Verdict::Determinate, None)
                    }
                } else if (best.0 - local[index]).abs() > PEAK_SEPARATION {
                    (Verdict::Inconsistent, Some(best.0 + origin))
                } else {
                    (Verdict::Ambiguous, Some(fitting[0] + origin))
                }
            };

            match verdict {
                Verdict::Determinate => counts[0] += 1,
                Verdict::Ambiguous => counts[1] += 1,
                Verdict::NoConsensus => counts[2] += 1,
                Verdict::Inconsistent => counts[3] += 1,
                Verdict::Edge => counts[4] += 1,
            }
            rows.push(Row { index: frame_index, page: page[frame_index], delta: step[frame_index], verdict, cost: Some(best.1), alt, coverage });
        }
    }
    rows.sort_by_key(|row| row.index);
    // 6. Report.
    if !quiet {
        println!(
            "runs: {} (largest {largest_run} frames), {uncovered} uncovered page rows (content no frame ever captured)",
            runs_count(&loose)
        );
        println!(
            "verdicts: {} determinate, {} ambiguous, {} no-consensus, {} inconsistent, {} edge",
            counts[0], counts[1], counts[2], counts[3], counts[4]
        );

    }
    if let Some(path) = tsv {
        let mut text = String::from("index\tpage\tdelta\tverdict\tloo_cost\talternative\tcoverage\n");
        for row in &rows {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\n",
                row.index,
                row.page,
                row.delta,
                row.verdict.as_str(),
                row.cost.map(|cost| format!("{cost:.4}")).unwrap_or_else(|| "-".into()),
                row.alt.map(|alt| alt.to_string()).unwrap_or_else(|| "-".into()),
                row.coverage,
            ));
        }
        std::fs::write(&path, text)?;
        if !quiet {
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

/// Consensus canvas: per page pixel, the sum of the luminance written there and
/// how many frames wrote it. Leave-one-out reads `(sum - own) / (count - 1)`.
struct Mosaic {
    width: u32,
    height: u32,
    origin_y: i32,
    sum: Vec<u32>,
    count: Vec<u16>,
}

impl Mosaic {
    fn build(frames: &[Frame], page: &[i32]) -> Self {
        Self::build_excluding(frames, page, &[])
    }

    fn build_excluding(frames: &[Frame], page: &[i32], excluded: &[bool]) -> Self {
        let origin_y = page.iter().copied().min().unwrap_or(0);
        let max_y = page.iter().zip(frames).map(|(top, frame)| top + frame.height as i32).max().unwrap_or(0);
        let width = frames
            .iter()
            .map(|frame| frame.x + frame.width as i32)
            .max()
            .unwrap_or(0)
            .max(1) as u32;
        let height = (max_y - origin_y).max(1) as u32;
        let mut mosaic = Self {
            width,
            height,
            origin_y,
            sum: vec![0; width as usize * height as usize],
            count: vec![0; width as usize * height as usize],
        };
        for (index, frame) in frames.iter().enumerate() {
            if excluded.get(index).copied().unwrap_or(false) {
                continue;
            }
            let x0 = frame.x;
            for row in 0..frame.height {
                let y = page[index] + row as i32 - origin_y;
                if y < 0 || y >= height as i32 {
                    continue;
                }
                let dst = y as usize * width as usize + x0 as usize;
                let src = row as usize * frame.width as usize;
                for column in 0..frame.width as usize {
                    mosaic.sum[dst + column] += u32::from(frame.lum[src + column]);
                    mosaic.count[dst + column] = mosaic.count[dst + column].saturating_add(1);
                }
            }
        }
        mosaic
    }

    /// Calls `visit(luminance, mean_of_others)` for every pixel of the frame at
    /// `top` that at least one *other* frame also covers.
    fn walk(&self, frame: &Frame, top: i32, mut visit: impl FnMut(u8, f64)) {
        for row in 0..frame.height {
            let y = top + row as i32 - self.origin_y;
            if y < 0 || y >= self.height as i32 {
                continue;
            }
            let src = row as usize * frame.width as usize;
            let dst = y as usize * self.width as usize + frame.x as usize;
            for column in 0..frame.width as usize {
                let cell = dst + column;
                let count = self.count[cell];
                if count < 2 {
                    continue;
                }
                let own = u32::from(frame.lum[src + column]);
                let others = self.sum[cell].saturating_sub(own);
                visit(frame.lum[src + column], others as f64 / f64::from(count - 1));
            }
        }
    }

    /// Difference between the frame at `top` and the consensus of the *other*
    /// frames: the **median** residual after a per-frame brightness/contrast fit.
    ///
    /// Three properties of real captures decide this shape:
    ///
    /// * a page with a sticky header or footer shows that band at the same
    ///   screen row in every frame, so it lands at a different page row in each
    ///   one — it disagrees by construction, and a mean lets that band alone mark
    ///   every frame unplaceable (measured on `real-long`: 2.4–3.4 luminance mean
    ///   over the whole frame against a perfect alignment of the rest);
    /// * content that changed between two visits (a chat log that streamed text,
    ///   a blinking caret) disagrees over part of the frame while the rest still
    ///   pins the position — a median ignores the changed minority;
    /// * the display's brightness and gamma are not properties of the content: a
    ///   dimmed screen or a mid-capture brightness jump changes every pixel at
    ///   once, and two renditions of the same page would otherwise accuse each
    ///   other (measured on `real-3`, frames 37–63: identical structure, +3
    ///   luminance, all 27 marked unplaceable by a mean). Fitting `gain` and
    ///   `offset` per frame removes both the jump and a gamma change.
    ///
    /// A *wrong* position still moves most of the pixels: text against text at
    /// the wrong row differs almost everywhere, and no gain/offset fixes it.
    fn loo_cost(&self, frames: &[Frame], index: usize, top: i32) -> Option<f32> {
        let frame = &frames[index];
        let mut count = 0f64;
        let mut sum_frame = 0f64;
        let mut sum_other = 0f64;
        let mut sum_frame_sq = 0f64;
        let mut sum_cross = 0f64;
        self.walk(frame, top, |luminance, mean| {
            let y = f64::from(luminance);
            count += 1.0;
            sum_frame += y;
            sum_other += mean;
            sum_frame_sq += y * y;
            sum_cross += y * mean;
        });
        if count == 0.0 {
            return None;
        }
        let variance = count * sum_frame_sq - sum_frame * sum_frame;
        let (gain, offset) = if variance.abs() < 1e-6 * count {
            (1.0, (sum_frame - sum_other) / count)
        } else {
            let gain = ((count * sum_cross - sum_frame * sum_other) / variance).clamp(0.25, 4.0);
            let offset = (sum_frame - gain * sum_other) / count;
            (gain, offset)
        };
        let mut histogram = [0u32; 256];
        let mut compared = 0u32;
        self.walk(frame, top, |luminance, mean| {
            let predicted = gain * mean + offset;
            let difference = (f64::from(luminance) - predicted).abs();
            let bucket = (difference.round() as usize).min(255);
            histogram[bucket] += 1;
            compared += 1;
        });
        let target = compared / 2;
        let mut seen = 0u32;
        for (value, bucket) in histogram.iter().enumerate() {
            seen += *bucket;
            if seen > target {
                return Some(value as f32);
            }
        }
        Some(255.0)
    }

    /// Share of the frame's pixels that another frame also covers. Below a
    /// quarter there is not enough consensus to judge the frame at all (the
    /// first and last screenfuls of a capture are like this).
    fn coverage(&self, frame: &Frame, top: i32) -> f32 {
        let mut covered = 0u64;
        self.walk(frame, top, |_, _| covered += 1);
        covered as f32 / (frame.width as f32 * frame.height as f32)
    }

    fn uncovered_rows(&self) -> u32 {
        let mut uncovered = 0;
        let mut seen_content = false;
        for row in 0..self.height as usize {
            let start = row * self.width as usize;
            let slice = &self.count[start..start + self.width as usize];
            let any = slice.iter().any(|count| *count > 0);
            if any {
                seen_content = true;
            } else if seen_content {
                uncovered += 1;
            }
        }
        uncovered
    }

}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Determinate,
    Ambiguous,
    NoConsensus,
    Inconsistent,
    Edge,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Determinate => "determinate",
            Verdict::Ambiguous => "ambiguous",
            Verdict::NoConsensus => "no-consensus",
            Verdict::Inconsistent => "inconsistent",
            Verdict::Edge => "edge",
        }
    }
}

struct Row {
    index: usize,
    page: i32,
    delta: i32,
    verdict: Verdict,
    cost: Option<f32>,
    alt: Option<i32>,
    coverage: f32,
}

/// How many grounded runs the chain split into (a run starts at every
/// undetermined step).
fn runs_count(loose: &[bool]) -> usize {
    1 + loose.iter().skip(1).filter(|flag| **flag).count()
}

/// Exact mean absolute luminance difference between two frames at one
/// displacement, over every pixel in the overlap.
fn pair_cost(previous: &Frame, current: &Frame, delta: i32) -> Option<f32> {
    let height = previous.height as i32;
    let (current_start, previous_start) = if delta >= 0 { (0, delta) } else { (-delta, 0) };
    let rows = height - delta.abs();
    if rows <= 0 {
        return None;
    }
    let width = previous.width;
    let mut total = 0u64;
    for row in 0..rows as u32 {
        let previous_row = (previous_start as u32 + row) as usize * width as usize;
        let current_row = (current_start as u32 + row) as usize * width as usize;
        let a = &previous.lum[previous_row..previous_row + width as usize];
        let b = &current.lum[current_row..current_row + width as usize];
        total += a
            .iter()
            .zip(b)
            .map(|(left, right)| u64::from(left.abs_diff(*right)))
            .sum::<u64>();
    }
    Some(total as f32 / (rows as f32 * width as f32))
}

fn pairwise_costs(frames: &[Frame]) -> Vec<Option<Pair>> {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 32);
    let mut out: Vec<Option<Pair>> = (0..frames.len()).map(|_| None).collect();
    let indices: Vec<usize> = (1..frames.len())
        .filter(|index| {
            frames[*index].width == frames[*index - 1].width
                && frames[*index].height == frames[*index - 1].height
        })
        .collect();
    if indices.is_empty() {
        return out;
    }
    let chunk = indices.len().div_ceil(workers).max(1);
    let computed: Vec<(usize, Pair)> = std::thread::scope(|scope| {
        let handles: Vec<_> = indices
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    for &index in slice {
                        let previous = &frames[index - 1];
                        let current = &frames[index];
                        let height = previous.height as i32;
                        let min_overlap = (height / MIN_OVERLAP_DENOMINATOR as i32).max(1);
                        let max_delta = height - min_overlap;
                        if max_delta <= 0 {
                            continue;
                        }
                        let mut costs = Vec::with_capacity((2 * max_delta + 1) as usize);
                        for delta in -max_delta..=max_delta {
                            costs.push(pair_cost(previous, current, delta).unwrap_or(f32::INFINITY));
                        }
                        local.push((index, Pair { lo: -max_delta, costs }));
                    }
                    local
                })
            })
            .collect();
        handles.into_iter().flat_map(|handle| handle.join().expect("truth worker panicked")).collect()
    });
    for (index, pair) in computed {
        out[index] = Some(pair);
    }
    out
}
