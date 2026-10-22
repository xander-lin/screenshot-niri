//! Replays a captured frame directory through the stitcher and reports, frame
//! by frame, what the stitcher decided and what the true displacement was.
//!
//! The true displacement is not taken from the stitcher: it is recovered by an
//! exhaustive pixel search between each pair of consecutive frames. That search
//! is far too slow to run live, but it is reliable, and having it means every
//! disagreement can be attributed instead of guessed at.
//!
//! Usage:
//!
//!     replay-frames --dir DIR [--verbose]

use std::error::Error;
use std::path::{Path, PathBuf};

use screenshot_rs_rebuild::image::Image;
use screenshot_rs_rebuild::stitch::{
    FrameScreenOffset, ImageRgbView, PushResult, RawStitcher,
};
use screenshot_rs_rebuild::wayland::selection::LongDirection;

/// Pixels sampled per axis by the exhaustive search. Four keeps the search
/// under a second per pair while still being sensitive to a one-row shift.
const ORACLE_STEP: u32 = 4;
/// The oracle only trusts an offset that keeps at least this share of the
/// frame overlapping: below it the "best" match is noise.
const ORACLE_MIN_OVERLAP_NUMERATOR: u32 = 1;
const ORACLE_MIN_OVERLAP_DENOMINATOR: u32 = 2;
/// Offsets closer than this are the same peak, not two different answers.
const ORACLE_ADJACENT: i32 = 2;

struct FrameEntry {
    path: PathBuf,
    offset: FrameScreenOffset,
    direction: Option<LongDirection>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("replay-frames: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut dir: Option<PathBuf> = None;
    let mut verbose = false;
    let mut save: Option<PathBuf> = None;
    let mut dump_ladder: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" | "-d" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--verbose" | "-v" => verbose = true,
            "--save" => save = Some(PathBuf::from(args.next().ok_or("--save needs a path")?)),
            "--dump-ladder" => {
                dump_ladder = Some(PathBuf::from(args.next().ok_or("--dump-ladder needs a directory")?))
            }
            "--help" | "-h" => {
                println!("replay-frames --dir DIR [--verbose] [--save OUT.png] [--dump-ladder DIR]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let dir = dir.ok_or("--dir DIR is required")?;

    let frames = load_frames(&dir)?;
    if frames.len() < 2 {
        return Err(format!("need at least two frames, found {}", frames.len()).into());
    }
    println!("frames: {}", frames.len());

    let mut stitcher = RawStitcher::new();
    let mut accepted = 0u32;
    let mut pending = 0u32;
    let mut refused = 0u32;
    let mut duplicate = 0u32;
    let mut misplaced = 0u32;
    let mut uncovered = 0u32;
    let mut torn_written = 0u32;
    let mut oracle_failed = 0u32;
    let mut ambiguous_pairs = 0u32;
    let mut delta_mismatch = 0u32;
    let mut canvas_top = 0i32;
    let mut worst: Vec<(u32, i32)> = Vec::new();
    let mut placements: Vec<(usize, i32)> = Vec::new();
    let mut blended_pair_costs: Vec<(usize, f64)> = Vec::new();
    let mut pair_costs: Vec<f64> = Vec::new();
    // Where the canvas top should be if it had followed the pairwise truth at
    // every step, and the largest gap between that chain and the engine's.
    let mut expected_top = 0i32;
    let mut max_drift: (usize, i32) = (0, 0);
    let mut final_drift: Option<i32> = None;
    // Per-frame stitcher wall time (production caliber: the whole push call,
    // excluding PNG decode and the exhaustive oracle).
    let mut push_total_ns = 0u64;
    let mut push_count = 0u32;
    let mut push_worst = std::time::Duration::ZERO;
    let mut push_worst_frame = 0usize;
    let mut push_accepted_ns = 0u64;
    let mut push_accepted_count = 0u32;

    let first = load_image(&frames[0].path)?;
    stitcher.push_frame_with_offset(
        &ImageRgbView::new(&first)?,
        &ImageRgbView::new(&first)?,
        frames[0].direction,
        frames[0].offset,
    )?;
    let mut previous = first;
    if let Some(top) = stitcher.last_engine_top() {
        expected_top = top;
    }

    println!(
        "\n{:>4} {:>9} {:>9} {:>7} {:>7} {:>8}  {}",
        "idx", "oracle", "stitcher", "err", "score", "outcome", "note"
    );

    for (i, entry) in frames.iter().enumerate().skip(1) {
        let image = load_image(&entry.path)?;

        // Ground truth: exhaustive search between this frame and the last one.
        let oracle = oracle_delta(&previous, &image);
        let oracle_delta = match &oracle {
            Some(o) => o.delta,
            None => {
                oracle_failed += 1;
                i32::MIN
            }
        };
        let oracle_ambiguous = oracle.as_ref().map(|o| o.ambiguous).unwrap_or(false);
        if oracle_ambiguous {
            ambiguous_pairs += 1;
        }
        // The truth chain advances on every pair, accepted or not: content the
        // engine skipped still scrolled, and the drift below has to account
        // for it or a recovery jump would look like steady-state error.
        if oracle_delta != i32::MIN {
            expected_top += oracle_delta;
        }
        if let Some(o) = &oracle {
            pair_costs.push(o.cost);
        }

        let push_started = std::time::Instant::now();
        let outcome = stitcher.push_frame_with_offset(
            &ImageRgbView::new(&image)?,
            &ImageRgbView::new(&image)?,
            entry.direction,
            entry.offset,
        )?;
        // Production-caliber timing: everything the stitcher does with a
        // captured frame, excluding PNG decode and the oracle below.
        let push_elapsed = push_started.elapsed();
        push_total_ns += push_elapsed.as_nanos() as u64;
        push_count += 1;
        if push_elapsed > push_worst {
            push_worst = push_elapsed;
            push_worst_frame = i;
        }

        // Descriptor dump: write the abstract shape the matcher saw next to a
        // side-by-side of the frame, so a wrong placement can be traced to the
        // descriptor losing evidence or to the decision discarding it.
        if let Some(out_dir) = dump_ladder.as_ref() {
            if let Some((w, h, data)) = stitcher.last_descriptor() {
                write_descriptor_dump(out_dir, i, w, h, &data, &image)?;
            }
        }

        let mut frame_delta: Option<i32> = None;
        let (label, note, score, err) = match outcome {
            PushResult::Accepted { match_info } => {
                accepted += 1;
                push_accepted_ns += push_elapsed.as_nanos() as u64;
                push_accepted_count += 1;
                canvas_top += match_info.delta_y;
                frame_delta = Some(match_info.delta_y);
                if let Some(top) = stitcher.last_engine_top() {
                    placements.push((i, top));
                    let drift = top - expected_top;
                    if drift.abs() > max_drift.1.abs() {
                        max_drift = (i, drift);
                    }
                }
                // Candidate blend frame that was written anyway: the position
                // may be right, but smeared pixels baked into the output are
                // not. Judged against the corpus-adaptive threshold below.
                if let Some(o) = oracle.as_ref() {
                    blended_pair_costs.push((i, o.cost));
                }
                let err = if oracle_delta == i32::MIN {
                    i32::MIN
                } else {
                    // Compare displacements, not absolute positions: the oracle
                    // measures this pair only.
                    match_info.delta_y - oracle_delta
                };
                // A disagreement on content that matches at several offsets is
                // not a misplacement: the frame carries no answer. Nor is a
                // delta that absorbs a refusal gap: the canvas position is what
                // matters, and `reprojection check` verifies that directly.
                if err != i32::MIN && err.abs() > 1 && !oracle_ambiguous {
                    delta_mismatch += 1;
                }
                (
                    "accepted",
                    if err == i32::MIN {
                        "no oracle".to_string()
                    } else if err.abs() <= 1 {
                        if oracle_ambiguous {
                            "ambiguous content".to_string()
                        } else {
                            String::new()
                        }
                    } else if oracle_ambiguous {
                        format!("differs by {err:+} on ambiguous content")
                    } else {
                        format!("off by {err:+}")
                    },
                    match_info.score,
                    err,
                )
            }
            PushResult::Pending => {
                pending += 1;
                (
                    "pending",
                    stitcher
                        .last_reject_reason()
                        .map(|r| r.as_str().to_string())
                        .unwrap_or_default(),
                    f64::NAN,
                    i32::MIN,
                )
            }
            PushResult::NoMatch => {
                refused += 1;
                (
                    "no-match",
                    stitcher
                        .last_reject_reason()
                        .map(|r| r.as_str().to_string())
                        .unwrap_or_default(),
                    f64::NAN,
                    i32::MIN,
                )
            }
            PushResult::Duplicate => {
                duplicate += 1;
                ("duplicate", String::new(), f64::NAN, i32::MIN)
            }
            PushResult::Initialized => ("init", String::new(), f64::NAN, i32::MIN),
        };

        if verbose || !note.is_empty() || matches!(outcome, PushResult::Pending | PushResult::NoMatch)
        {
            println!(
                "{:>4} {:>9} {:>9} {:>7} {:>7.4} {:>8}  {}",
                i,
                if oracle_delta == i32::MIN {
                    "-".to_string()
                } else {
                    oracle_delta.to_string()
                },
                frame_delta
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                if err == i32::MIN { "-".to_string() } else { err.to_string() },
                score,
                label,
                note
            );
        }

        previous = image;
    }

    // Blend threshold. On the screencopy corpus sharp pairs cost ~0.05 and
    // blends stand out above 1.0, but corpora with larger scroll steps (the
    // CDP sets) cost 1.2-2.5 on every pair — a constant there would brand
    // every frame a blend. Calibrate against the corpus median so the counter
    // means "an outlier for this corpus", never "above an absolute bar".
    let mut sorted_costs = pair_costs.clone();
    sorted_costs.sort_by(|a, b| a.total_cmp(b));
    let median_cost = sorted_costs.get(sorted_costs.len() / 2).copied().unwrap_or(0.0);
    let blend_threshold = (4.0 * median_cost).max(ORACLE_BLEND_COST);
    let blended_written = blended_pair_costs
        .iter()
        .filter(|(_, cost)| *cost > blend_threshold)
        .count() as u32;

    // Verify every accepted placement against the pixels: re-project the frame
    // onto the canvas it was placed in and measure how well it fits.
    if !placements.is_empty() {
        if let Some(stitched) = stitcher.stitched() {
            let started = std::time::Instant::now();
            let canvas = CanvasLuma::new(stitched);
            let origin = stitcher.canvas_origin();

            // Load every accepted frame once and convert it to luminance once;
            // both checks below reuse the same buffers.
            let loaded: Vec<(usize, i32, Image, Vec<u8>)> = placements
                .iter()
                .map(|&(i, logical)| {
                    let image = load_image(&frames[i].path)?;
                    let lum = luminance(&image);
                    Ok((i, logical - origin, image, lum))
                })
                .collect::<Result<Vec<_>, Box<dyn Error>>>()?;

            // One search per frame answers both questions: where the stitcher
            // put it, and where it best fits anywhere.
            let searched: Vec<(usize, Located)> = loaded
                .iter()
                .map(|(i, claimed, image, lum)| {
                    (*i, locate_in_canvas(&canvas, image, lum, *claimed))
                })
                .collect();

            // Check 1: does the frame's pixels match the canvas where the
            // stitcher put it? Blurred transition frames fit poorly *everywhere*
            // (their content is smeared), so a placement is only judged against
            // the frame's own floor — otherwise every blurred frame is a false
            // accusation.
            let mut bad: Vec<(usize, i32, f64)> = Vec::new();
            for (i, located) in &searched {
                if located.claimed_cost > REPROJECTION_MAX_COST
                    && located.claimed_cost > located.floor + REPROJECTION_MARGIN
                {
                    bad.push((*i, located.claimed, located.claimed_cost));
                }
            }
            misplaced = bad.len() as u32;

            // Check 2: where did each frame *actually* end up? A blurred frame
            // fits badly everywhere, so its own fit cannot judge it — but its
            // neighbours can. A frame whose found row sits far from the rows
            // around it was placed on a repeated-looking patch of canvas.
            let located: Vec<(usize, i32, f64)> = searched
                .iter()
                .map(|(i, l)| (*i, l.row, l.cost))
                .collect();
            let jumps = find_isolation(&located);

            // Check 3: band consistency. A frame captured mid-jump — part of
            // the buffer at the old position, part at the new one — has no
            // single placement, and whatever rows it writes land displaced
            // from its own top. Locate each horizontal band independently and
            // compare with the claimed row. Bands that fit nowhere are not
            // evidence (low texture fits nothing tightly); a band that fits
            // well *elsewhere* is content the canvas holds at the wrong rows.
            let band_work: Vec<(usize, i32, i32, i32)> = {
                let mut work = Vec::new();
                for (k, (_, claimed, image, _)) in loaded.iter().enumerate() {
                    let height = image.height as i32;
                    let band = (height + BAND_COUNT - 1) / BAND_COUNT;
                    for b in 0..BAND_COUNT {
                        let lo = (b * band).min(height);
                        let hi = ((b + 1) * band).min(height);
                        // Only bands that sit fully inside the canvas can be
                        // judged: a band straddling the top or bottom edge has
                        // its at-claimed cost computed on a sliver, and any
                        // "better fit elsewhere" is an artifact of comparing a
                        // full band against a fragment.
                        if hi - lo > 8
                            && claimed + lo >= 0
                            && claimed + hi <= canvas.height as i32
                        {
                            work.push((k, *claimed, lo, hi));
                        }
                    }
                }
                work
            };
            let parts = split_range(0, band_work.len() as i32 - 1, worker_count());
            let displaced: Vec<(usize, i32, i32)> = std::thread::scope(|scope| {
                let handles: Vec<_> = parts
                    .iter()
                    .map(|&(lo, hi)| {
                        let band_work = &band_work;
                        let loaded = &loaded;
                        let canvas = &canvas;
                        scope.spawn(move || {
                            let mut local = Vec::new();
                            for idx in lo..=hi {
                                let (k, claimed, blo, bhi) = band_work[idx as usize];
                                let (_, _, image, lum) = &loaded[k];
                                let at_claimed =
                                    reprojection_band_cost(&canvas, image, lum, claimed, blo, bhi);
                                let mut best: Option<(i32, f64)> = None;
                                for offset in -BAND_WINDOW..=BAND_WINDOW {
                                    let top = claimed + offset;
                                    // Only candidates keeping the band fully on
                                    // the canvas are comparable: outside it the
                                    // cost function would see no pixels at all
                                    // and report a perfect fit.
                                    if top + blo < 0 || top + bhi > canvas.height as i32 {
                                        continue;
                                    }
                                    let cost = reprojection_band_cost(
                                        &canvas, image, lum, top, blo, bhi,
                                    );
                                    if best.map_or(true, |(_, c)| cost < c) {
                                        best = Some((offset, cost));
                                    }
                                }
                                if let Some((offset, cost)) = best {
                                    if cost < BAND_FOUND_COST
                                        && cost + BAND_MARGIN < at_claimed
                                        && offset.abs() > BAND_TOLERANCE
                                    {
                                        local.push((k, blo, offset));
                                    }
                                }
                            }
                            local
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().expect("band worker panicked"))
                    .collect::<Vec<_>>()
            });
            let mut by_frame: Vec<(usize, Vec<(i32, i32)>)> = Vec::new();
            for (k, blo, offset) in displaced {
                let frame = loaded[k].0;
                match by_frame.last_mut() {
                    Some((f, list)) if *f == frame => list.push((blo, offset)),
                    _ => by_frame.push((frame, vec![(blo, offset)])),
                }
            }
            torn_written = by_frame.len() as u32;

            println!("\nreprojection check: {} accepted frames ({:?})", placements.len(), started.elapsed());
            println!("  placements whose pixels do not fit the canvas: {}", bad.len());
            for (i, top, cost) in bad.iter().take(20) {
                println!("    frame {i} at logical row {top}: mean |diff| = {cost:.2}");
            }
            println!("  placements far from their neighbours: {}", jumps.len());
            for (i, row, claimed, gap) in jumps.iter().take(20) {
                println!(
                    "    frame {i} at canvas row {row}, but its neighbours are around {claimed}: gap {gap}"
                );
            }
            println!(
                "  frames with bands displaced from their own top: {}  (jump-transition content written at wrong rows)",
                by_frame.len()
            );
            for (frame, list) in by_frame.iter().take(20) {
                let offsets: Vec<String> = list
                    .iter()
                    .map(|(blo, off)| format!("row {blo}: {off:+}"))
                    .collect();
                println!("    frame {frame}: {}", offsets.join(", "));
            }
            misplaced = misplaced.saturating_add(jumps.len() as u32);
        }
    }

    // Coverage: within the stitched extent, every row must be fully covered.
    // A recovery jump that outruns every later frame leaves rows that no
    // accepted frame ever contained — permanent transparent strips in the
    // final PNG. `row_coverage` records the filled extent per canvas row.
    if let Some(stitched) = stitcher.stitched() {
        if stitched.row_coverage.is_empty() {
            println!("\ncoverage check: all rows fully covered");
        } else {
            let width = stitched.width;
            let mut holes = 0u32;
            let mut hole_ranges: Vec<(u32, u32)> = Vec::new();
            let mut partial = 0u32;
            for (y, coverage) in stitched.row_coverage.iter().enumerate() {
                let covered = coverage.captured_width(width);
                if covered == 0 {
                    holes += 1;
                    match hole_ranges.last_mut() {
                        Some((_, last)) if *last + 1 == y as u32 => *last = y as u32,
                        _ => hole_ranges.push((y as u32, y as u32)),
                    }
                } else if covered < width {
                    partial += 1;
                }
            }
            println!(
                "\ncoverage check: {} canvas rows",
                stitched.row_coverage.len()
            );
            println!("  UNCOVERED rows inside the canvas: {holes}");
            for (lo, hi) in hole_ranges.iter().take(10) {
                println!("    rows {lo}..={hi}");
            }
            println!(
                "  partially covered rows: {partial}  (expected only for T/L/arch captures)"
            );
            uncovered = holes;
        }
    }

    if let Some(path) = save.as_ref() {
        if let Some(stitched) = stitcher.stitched() {
            let image = Image {
                width: stitched.width,
                height: stitched.height,
                stride: stitched.width * 4,
                format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
                data: rgba_from_rgb(stitched),
            };
            screenshot_rs_rebuild::image::write_png(path, &image)?;
            println!("canvas written to {}", path.display());
        }
    }

    println!("\nsummary");
    println!("  accepted  {accepted}");
    println!("  pending   {pending}");
    println!("  no-match  {refused}");
    println!("  duplicate {duplicate}");
    println!("  MISPLACED {misplaced}  (frame pixels do not match the canvas where they were put)");
    println!("  frames with displaced bands {torn_written}  (jump-transition frames whose content sits rows away from their own top)");
    println!("  UNCOVERED {uncovered}  (canvas rows no accepted frame ever contained)");
    println!("  pairwise-outlier frames written {blended_written}  (cost > {blend_threshold:.2}; whether their rows land correctly is decided by the band criterion)");
    if let Some(top) = stitcher.last_engine_top() {
        final_drift = Some(top - expected_top);
        println!(
            "  chain drift vs truth: max {} px at frame {}, final {} px",
            max_drift.1, max_drift.0, final_drift.unwrap_or(0)
        );
    }
    println!("  delta differs from the pairwise truth {delta_mismatch}  (a recovery gap, not a misplacement)");
    println!("  ambiguous pairs {ambiguous_pairs}  (content matched at several offsets)");
    if oracle_failed > 0 {
        println!("  oracle could not decide {oracle_failed} pairs (no overlap)");
    }
    if !worst.is_empty() {
        println!("  worst misplacements: {worst:?}");
    }
    println!("  canvas height reached: {canvas_top}");
    if push_count > 0 {
        let mean = push_total_ns as f64 / push_count as f64 / 1e6;
        let worst = push_worst.as_secs_f64() * 1e3;
        let acc = if push_accepted_count > 0 {
            push_accepted_ns as f64 / push_accepted_count as f64 / 1e6
        } else {
            0.0
        };
        println!(
            "  push timing: {push_count} frames, mean {mean:.2} ms (accepted {acc:.2} ms), worst {worst:.2} ms at frame {push_worst_frame}"
        );
    }
    Ok(())
}

/// Pairwise cost above which the oracle calls a frame a blurred blend of two
/// scroll positions (the same convention as truth.tsv). Writing such a frame
/// bakes the smear into the output even when its position is right.
const ORACLE_BLEND_COST: f64 = 1.0;
/// Number of worker threads used by the exhaustive searches.
///
/// These are the only two places in this tool that do real work per candidate
/// row, and both are embarrassingly parallel: every candidate is scored
/// independently and the results are combined at the end. The searches are
/// deterministic regardless of the thread count, so the output does not depend
/// on how many cores happen to be available.
fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 32)
}

/// Splits `lo..=hi` into one contiguous range per worker.
fn split_range(lo: i32, hi: i32, parts: usize) -> Vec<(i32, i32)> {
    if hi < lo {
        return Vec::new();
    }
    let total = (hi - lo + 1) as usize;
    let parts = parts.min(total).max(1);
    let chunk = total.div_ceil(parts);
    let mut out = Vec::new();
    let mut start = lo;
    while start <= hi {
        let end = (start + chunk as i32 - 1).min(hi);
        out.push((start, end));
        start = end + 1;
    }
    out
}

/// What the exhaustive search concluded about one frame pair.
struct Oracle {
    /// Smallest-magnitude displacement that matches as well as the best one.
    delta: i32,
    /// True when several distinct displacements match equally well. A stitcher
    /// that accepts such a frame is guessing, not measuring.
    ambiguous: bool,
    /// Cost at the chosen displacement, in mean luminance units.
    cost: f64,
}

/// Exhaustive search for the vertical displacement between two frames.
///
/// Positive means the second frame shows content further down the page (the
/// view scrolled down). Only offsets keeping half the frame overlapping are
/// considered, because a smaller overlap can match almost anything.
///
/// On periodic content several offsets match equally well. Reporting the
/// smallest-magnitude one and flagging the tie keeps the oracle from blaming
/// the stitcher for a position that is genuinely indistinguishable.
fn oracle_delta(previous: &Image, current: &Image) -> Option<Oracle> {
    if previous.width != current.width || previous.height != current.height {
        return None;
    }
    let width = previous.width;
    let height = previous.height;
    if width == 0 || height == 0 {
        return None;
    }
    let prev = luminance(previous);
    let cur = luminance(current);
    let min_overlap = height * ORACLE_MIN_OVERLAP_NUMERATOR / ORACLE_MIN_OVERLAP_DENOMINATOR;
    let max_delta = height as i32 - min_overlap as i32;
    if max_delta <= 0 {
        return None;
    }

    let parts = split_range(-max_delta, max_delta, worker_count());
    let mut scored: Vec<(f64, i32)> = std::thread::scope(|scope| {
        let handles: Vec<_> = parts
            .iter()
            .map(|&(lo, hi)| {
                let prev = &prev;
                let cur = &cur;
                scope.spawn(move || {
                    let mut local: Vec<(f64, i32)> = Vec::new();
                    for delta in lo..=hi {
                        if let Some(entry) =
                            oracle_score(prev, cur, width, height, min_overlap, delta)
                        {
                            local.push(entry);
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("oracle worker panicked"))
            .collect()
    });
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    if scored.is_empty() {
        return None;
    }
    let best_cost = scored[0].0;
    let tolerance = best_cost * 0.02 + 0.5;
    let mut tied: Vec<i32> = scored
        .iter()
        .filter(|(c, _)| *c <= best_cost + tolerance)
        .map(|(_, d)| *d)
        .collect();
    tied.sort_unstable();
    let mut distinct: Vec<i32> = Vec::new();
    for delta in tied {
        if distinct.last().map_or(true, |last| (delta - last).abs() > ORACLE_ADJACENT) {
            distinct.push(delta);
        }
    }
    let chosen = *distinct
        .iter()
        .min_by_key(|delta| delta.abs())
        .expect("at least one candidate survived the filter");
    return Some(Oracle { delta: chosen, ambiguous: distinct.len() > 1, cost: best_cost });

}

/// Scores one displacement candidate for the oracle.
fn oracle_score(
    prev: &[u8],
    cur: &[u8],
    width: u32,
    height: u32,
    min_overlap: u32,
    delta: i32,
) -> Option<(f64, i32)> {
    let (cur_start, prev_start, rows) = if delta >= 0 {
        (0u32, delta as u32, height.checked_sub(delta as u32)?)
    } else {
        ((-delta) as u32, 0u32, height.checked_sub((-delta) as u32)?)
    };
    if rows < min_overlap {
        return None;
    }
    let mut sum = 0u64;
    let mut count = 0u64;
    let mut y = 0u32;
    while y < rows {
        let a = ((prev_start + y) * width) as usize;
        let b = ((cur_start + y) * width) as usize;
        let mut x = 0u32;
        while x < width {
            sum += u64::from(prev[a + x as usize].abs_diff(cur[b + x as usize]));
            count += 1;
            x += ORACLE_STEP;
        }
        y += ORACLE_STEP;
    }
    if count == 0 {
        return None;
    }
    Some((sum as f64 / count as f64, delta))
}

/// One luminance byte per pixel, so the oracle search touches a quarter of the
/// memory the BGRA buffer would.
fn luminance(image: &Image) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.width as usize * image.height as usize);
    for y in 0..image.height as usize {
        let row = y * image.stride as usize;
        for x in 0..image.width as usize {
            let o = row + x * 4;
            if o + 2 >= image.data.len() {
                out.push(0);
                continue;
            }
            // Stored as B, G, R, X.
            let b = u32::from(image.data[o]);
            let g = u32::from(image.data[o + 1]);
            let r = u32::from(image.data[o + 2]);
            out.push(((r * 77 + g * 150 + b * 29) >> 8) as u8);
        }
    }
    out
}

fn load_frames(dir: &Path) -> Result<Vec<FrameEntry>, Box<dyn Error>> {
    let manifest = dir.join("session.json");
    if manifest.exists() {
        let text = std::fs::read_to_string(&manifest)?;
        return parse_manifest(dir, &text);
    }
    // No manifest: take every PNG in name order, no offsets, no direction.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "png").unwrap_or(false))
        .collect();
    entries.sort();
    Ok(entries
        .into_iter()
        .map(|path| FrameEntry {
            path,
            offset: FrameScreenOffset { x: 0, y: 0 },
            direction: None,
        })
        .collect())
}

/// Minimal reader for the manifest `capture-frames` writes. Deliberately not a
/// general JSON parser: the file is produced by this repository and the fields
/// are flat.
fn parse_manifest(dir: &Path, text: &str) -> Result<Vec<FrameEntry>, Box<dyn Error>> {
    let mut frames = Vec::new();
    // Each frame is one object containing a `file` key. Slicing from that key to
    // the closing brace keeps the reader independent of indentation, so both the
    // compact writer in `capture-frames` and a pretty-printed manifest parse the
    // same way.
    let mut rest = text;
    while let Some(start) = rest.find("\"file\"") {
        let tail = &rest[start..];
        let Some(end) = tail.find('}') else { break };
        let object = &tail[..end];
        rest = &tail[end..];

        let file = field(object, "file").ok_or("frame entry without a file")?;
        let ox = field(object, "offset_x")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        let oy = field(object, "offset_y")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        let direction = match field(object, "direction").as_deref() {
            Some("down") => Some(LongDirection::Down),
            Some("up") => Some(LongDirection::Up),
            Some("left") => Some(LongDirection::Left),
            Some("right") => Some(LongDirection::Right),
            _ => None,
        };
        frames.push(FrameEntry {
            path: dir.join(file),
            offset: FrameScreenOffset { x: ox, y: oy },
            direction,
        });
    }
    Ok(frames)
}

fn field(line: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\": ");
    let start = line.find(&key)? + key.len();
    let rest = &line[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Mean absolute luminance difference between a frame and the canvas rows it
/// was placed on.
///
/// This is the check that does not depend on the oracle's notion of a "single
/// frame displacement": if the frame's pixels do not match the canvas where the
/// stitcher put them, the placement is wrong regardless of how the delta was
/// derived.
fn reprojection_cost(
    canvas: &CanvasLuma,
    frame: &Image,
    frame_lum: &[u8],
    top: i32,
) -> f64 {
    reprojection_band_cost(canvas, frame, frame_lum, top, 0, frame.height as i32)
}

/// Same as `reprojection_cost`, restricted to frame rows `[lo, hi)`. Band-level
/// comparison is what exposes a torn write: a frame captured mid-scroll whose
/// top and bottom halves sit at different scroll positions.
#[allow(clippy::too_many_arguments)]
fn reprojection_band_cost(
    canvas: &CanvasLuma,
    frame: &Image,
    frame_lum: &[u8],
    top: i32,
    lo: i32,
    hi: i32,
) -> f64 {
    let width = frame.width.min(canvas.width);
    let mut sum = 0u64;
    let mut count = 0u64;
    for row in lo.max(0)..hi.min(frame.height as i32) {
        let canvas_row = top + row;
        if canvas_row < 0 || canvas_row >= canvas.height as i32 {
            continue;
        }
        let canvas_off = canvas_row as usize * canvas.width as usize;
        let frame_off = row as usize * frame.width as usize;
        let mut x = 0u32;
        while x < width {
            sum += u64::from(
                canvas.data[canvas_off + x as usize].abs_diff(frame_lum[frame_off + x as usize]),
            );
            count += 1;
            x += 4;
        }
    }
    if count == 0 {
        return 0.0;
    }
    sum as f64 / count as f64
}

/// Canvas luminance, converted once instead of once per candidate row.
struct CanvasLuma {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl CanvasLuma {
    fn new(stitched: &screenshot_rs_rebuild::stitch::StitchedFrame) -> Self {
        let mut data = Vec::with_capacity(stitched.width as usize * stitched.height as usize);
        for y in 0..stitched.height as usize {
            let row = y * stitched.stride as usize;
            for x in 0..stitched.width as usize {
                let c = row + x * 3;
                data.push(
                    ((u32::from(stitched.data[c]) * 77
                        + u32::from(stitched.data[c + 1]) * 150
                        + u32::from(stitched.data[c + 2]) * 29)
                        >> 8) as u8,
                );
            }
        }
        Self { width: stitched.width, height: stitched.height, data }
    }
}

/// Where the frame really sits in the canvas, and how well it fits there.
///
/// Searches the whole canvas rather than trusting the claimed row, so a frame
/// placed on the wrong copy of repeated content is visible.
/// Result of searching the whole canvas for one frame.
struct Located {
    /// Row the stitcher claimed, in canvas pixels.
    claimed: i32,
    /// Fit at that row.
    claimed_cost: f64,
    /// Row this frame is taken to sit at: the claimed row, unless another row
    /// fits clearly better. A blurred frame's argmin wanders by a few hundred
    /// rows on noise alone, so only a decisive improvement overrides.
    row: i32,
    /// Fit at `row`.
    cost: f64,
    /// Best fit anywhere, used as the frame's own floor. Blurred frames fit
    /// poorly everywhere, so this is what distinguishes "blurry" from
    /// "misplaced".
    floor: f64,
}

fn locate_in_canvas(
    canvas: &CanvasLuma,
    frame: &Image,
    frame_lum: &[u8],
    claimed: i32,
) -> Located {
    let claimed_cost = reprojection_cost(canvas, frame, frame_lum, claimed);
    let last = canvas.height as i32 - frame.height as i32;
    if last < 0 {
        return Located {
            claimed,
            claimed_cost,
            row: claimed,
            cost: claimed_cost,
            floor: claimed_cost,
        };
    }
    let parts = split_range(0, last, worker_count());
    let found = std::thread::scope(|scope| {
        let handles: Vec<_> = parts
            .iter()
            .map(|&(lo, hi)| {
                scope.spawn(move || {
                    let mut local: Option<(i32, f64)> = None;
                    for top in lo..=hi {
                        let cost = reprojection_cost(canvas, frame, frame_lum, top);
                        if local.map_or(true, |(_, c)| cost < c) {
                            local = Some((top, cost));
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("locate worker panicked"))
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
    });
    let (best_row, best_cost) = found.unwrap_or((claimed, claimed_cost));
    let (row, cost) = if best_cost + LOCATE_MARGIN < claimed_cost {
        (best_row, best_cost)
    } else {
        (claimed, claimed_cost)
    };
    Located { claimed, claimed_cost, row, cost, floor: best_cost }
}

/// Placements that sit far from the rows their neighbours occupy.
///
/// The page scrolls continuously, so consecutive accepted frames must land in a
/// monotone run. A frame whose best fit is a whole pattern-height away from its
/// neighbours was matched against repeated content, and its own fit cannot tell
/// — that is exactly why this check uses neighbours instead.
fn find_isolation(located: &[(usize, i32, f64)]) -> Vec<(usize, i32, i32, i32)> {
    let mut out = Vec::new();
    for (index, &(i, row, _)) in located.iter().enumerate() {
        // Compare against the nearest neighbours that are themselves consistent.
        let before = located[..index].iter().rev().find(|(_, r, _)| *r != row);
        let after = located[index + 1..].iter().find(|(_, r, _)| *r != row);
        let expected = match (before, after) {
            (Some((_, b, _)), Some((_, a, _))) => {
                // Interpolate by frame index so a large gap does not count.
                let b_idx = before.unwrap().0 as i32;
                let a_idx = after.unwrap().0 as i32;
                if a_idx == b_idx {
                    *b
                } else {
                    let t = (i as i32 - b_idx) as f64 / (a_idx - b_idx) as f64;
                    (*b as f64 + (f64::from(*a - *b)) * t).round() as i32
                }
            }
            (Some((_, b, _)), None) => *b,
            (None, Some((_, a, _))) => *a,
            (None, None) => continue,
        };
        let gap = row - expected;
        if gap.abs() > ISOLATION_MAX_GAP {
            out.push((i, row, expected, gap));
        }
    }
    out
}

/// A placement further than this from where its neighbours put the page is
/// treated as misplaced. Real scrolling is continuous; a whole viewport of
/// disagreement between adjacent frames is not scrolling.
const ISOLATION_MAX_GAP: i32 = 300;
/// How much better a different row must fit before it overrides the claimed
/// one when locating a frame.
const LOCATE_MARGIN: f64 = 1.0;
/// Above this, the frame's pixels do not match the canvas where it was put.
const REPROJECTION_MAX_COST: f64 = 2.0;
/// A placement is only wrong when it is this much worse than the frame's own
/// best fit anywhere. Blurred frames never fit tightly; comparing against their
/// own floor keeps the check about *where* the frame went, not how sharp it is.
const REPROJECTION_MARGIN: f64 = 2.0;
/// Frames are split into this many horizontal bands for the band check.
const BAND_COUNT: i32 = 4;
/// A band found this far from its frame's own top is displaced, not jitter.
const BAND_TOLERANCE: i32 = 24;
/// How far around the claimed row a band is searched. Jump-transition frames
/// in the corpora displace bands by up to ~250 rows.
const BAND_WINDOW: i32 = 300;
/// Below this a band counts as found; above it the band fits nothing tightly
/// (low texture) and is not evidence of anything.
const BAND_FOUND_COST: f64 = 1.0;
/// How much better a band must fit elsewhere before displacement counts.
const BAND_MARGIN: f64 = 1.0;

/// Converts the stitched RGB canvas into the BGRA layout `write_png` expects.
fn rgba_from_rgb(stitched: &screenshot_rs_rebuild::stitch::StitchedFrame) -> Vec<u8> {
    let mut data = vec![255u8; stitched.width as usize * stitched.height as usize * 4];
    for y in 0..stitched.height as usize {
        let src_row = y * stitched.stride as usize;
        let dst_row = y * stitched.width as usize * 4;
        for x in 0..stitched.width as usize {
            let s = src_row + x * 3;
            let d = dst_row + x * 4;
            data[d] = stitched.data[s + 2];
            data[d + 1] = stitched.data[s + 1];
            data[d + 2] = stitched.data[s];
            data[d + 3] = 255;
        }
    }
    data
}

/// Writes the descriptor as a grayscale image, plus a side-by-side of the
/// original frame scaled to the same height.
///
/// The descriptor is `bins` wide and `rows` tall — deliberately coarse, because
/// that is what the matcher compares. Looking at it next to the frame answers
/// the only question that matters when a placement is wrong: did the descriptor
/// lose the evidence, or did the decision throw it away?
fn write_descriptor_dump(
    out_dir: &Path,
    index: usize,
    width: u32,
    height: u32,
    data: &[u8],
    frame: &Image,
) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(out_dir)?;
    if width == 0 || height == 0 || data.len() < (width as usize * height as usize) {
        return Ok(());
    }
    // Descriptor as BGRA, nearest-neighbour scaled so each cell is visible.
    let scale: u32 = 4;
    let dw = width * scale;
    let dh = height;
    let mut desc = Vec::with_capacity((dw as usize * dh as usize * 4));
    for y in 0..height {
        for x in 0..dw {
            let cell = data[(y * width + x / scale) as usize];
            desc.extend_from_slice(&[cell, cell, cell, 255]);
        }
    }
    let desc_image = Image {
        width: dw,
        height: dh,
        stride: dw * 4,
        format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
        data: desc,
    };
    screenshot_rs_rebuild::image::write_png(
        &out_dir.join(format!("desc-{index:04}.png")),
        &desc_image,
    )?;

    // Original frame, nearest-neighbour scaled to the descriptor's height so the
    // two can be compared row by row.
    let fw = frame.width.max(1);
    let fh = frame.height.max(1);
    let target_h = height;
    let target_w = (fw * target_h / fh).max(1);
    let mut orig = Vec::with_capacity((target_w as usize * target_h as usize * 4));
    for y in 0..target_h {
        let sy = (y as u64 * fh as u64 / target_h as u64) as u32;
        for x in 0..target_w {
            let sx = (x as u64 * fw as u64 / target_w as u64) as u32;
            let o = (sy * frame.stride as u32 + sx * 4) as usize;
            if o + 3 < frame.data.len() {
                orig.extend_from_slice(&frame.data[o..o + 4]);
            } else {
                orig.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    let orig_image = Image {
        width: target_w,
        height: target_h,
        stride: target_w * 4,
        format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
        data: orig,
    };
    screenshot_rs_rebuild::image::write_png(
        &out_dir.join(format!("orig-{index:04}.png")),
        &orig_image,
    )?;
    Ok(())
}

fn load_image(path: &Path) -> Result<Image, Box<dyn Error>> {
    let file = std::fs::File::open(path)?;
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let (width, height) = (info.width, info.height);
    let color = info.color_type;
    let channels = match color {
        png::ColorType::Rgba => 4usize,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        other => return Err(format!("unsupported PNG colour type {other:?}").into()),
    };
    let stride = width * 4;
    let mut data = vec![0u8; stride as usize * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let s = (y * width as usize + x) * channels;
            let d = y * stride as usize + x * 4;
            let (r, g, b) = match channels {
                4 => (buf[s], buf[s + 1], buf[s + 2]),
                3 => (buf[s], buf[s + 1], buf[s + 2]),
                _ => (buf[s], buf[s], buf[s]),
            };
            // Match the in-memory layout the rest of the tool expects: B,G,R,X.
            data[d] = b;
            data[d + 1] = g;
            data[d + 2] = r;
            data[d + 3] = 255;
        }
    }
    Ok(Image {
        width,
        height,
        stride,
        format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
        data,
    })
}
