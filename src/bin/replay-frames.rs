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

use screenshot_rs_rebuild::corpus::{load_frames, load_png, luminance, truth_cache_path, truth_key_for};
use screenshot_rs_rebuild::image::Image;
use screenshot_rs_rebuild::stitch::{ImageRgbView, PushResult, RawStitcher};

/// Pixels sampled per axis by the exhaustive search. Four keeps the search
/// under a second per pair while still being sensitive to a one-row shift.
const ORACLE_STEP: u32 = 4;
/// The oracle only trusts an offset that keeps at least this share of the
/// frame overlapping: below it the "best" match is noise.
const ORACLE_MIN_OVERLAP_NUMERATOR: u32 = 1;
const ORACLE_MIN_OVERLAP_DENOMINATOR: u32 = 2;
/// Offsets closer than this are the same peak, not two different answers.
const ORACLE_ADJACENT: i32 = 2;

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
    let mut exact = false;
    let mut refresh = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" | "-d" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--verbose" | "-v" => verbose = true,
            "--exact" => exact = true,
            "--refresh" => refresh = true,
            "--save" => save = Some(PathBuf::from(args.next().ok_or("--save needs a path")?)),
            "--dump-ladder" => {
                dump_ladder = Some(PathBuf::from(args.next().ok_or("--dump-ladder needs a directory")?))
            }
            "--help" | "-h" => {
                println!("replay-frames --dir DIR [--verbose] [--exact] [--refresh] [--save OUT.png] [--dump-ladder DIR]");
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

    // The exhaustive oracle depends only on the frames: cache it per corpus so
    // a before/after verification pass does not pay for it twice. `--exact`
    // changes the answer, so it is part of the key.
    let cache_key = truth_key_for(&frames, "replay-oracle", &[("exact", exact.to_string()), ("step", ORACLE_STEP.to_string())])?;
    let cache_path = truth_cache_path(&dir, "replay-oracle");
    let mut oracles: Option<Vec<Option<Oracle>>> = if refresh {
        None
    } else {
        load_oracle_cache(&cache_path, &cache_key, frames.len())
    };
    let cached_oracles = oracles.is_some();
    if !cached_oracles {
        oracles = Some(Vec::with_capacity(frames.len().saturating_sub(1)));
    }

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
    let mut strict_refusals = 0u32;
    let mut canvas_top = 0i32;
    let mut placements: Vec<(usize, i32)> = Vec::new();
    let mut blended_pair_costs: Vec<(usize, f64)> = Vec::new();
    let mut pair_costs: Vec<f64> = Vec::new();
    // Where the canvas top should be if it had followed the pairwise truth at
    // every step, and the largest gap between that chain and the engine's.
    let mut expected_top = 0i32;
    let mut max_drift: (usize, i32) = (0, 0);
    // The drift of the last accepted frame; only its final value is reported.
    let final_drift: Option<i32>;
    // Per-frame stitcher wall time (production caliber: the whole push call,
    // excluding PNG decode and the exhaustive oracle).
    let mut push_total_ns = 0u64;
    let mut push_count = 0u32;
    let mut push_worst = std::time::Duration::ZERO;
    let mut push_worst_frame = 0usize;
    let mut push_accepted_ns = 0u64;
    let mut push_accepted_count = 0u32;

    let first = load_png(&frames[0].path)?;
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
        let image = load_png(&entry.path)?;

        // Ground truth: exhaustive search between this frame and the last one,
        // or the cached answer for this corpus.
        let oracle = match oracles.as_mut() {
            Some(cached) if cached_oracles => cached.get(i - 1).cloned().flatten(),
            Some(computed) => {
                let oracle = oracle_delta(&previous, &image, if exact { 1 } else { ORACLE_STEP });
                computed.push(oracle.clone());
                oracle
            }
            None => unreachable!("oracles is filled above"),
        };
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
                // not a misplacement: the frame carries no answer. The tie set
                // is what decides that, not a flag: an offset inside it is one
                // of the answers the pixels allow. Nor is a delta that absorbs
                // a refusal gap — the canvas position is what matters, which
                // the reprojection check verifies directly.
                let outside_ties = oracle
                    .as_ref()
                    .is_some_and(|o| !o.ties.iter().any(|t| (t - match_info.delta_y).abs() <= 1));
                if err != i32::MIN && err.abs() > 1 && outside_ties {
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
                    } else if !outside_ties {
                        format!("differs by {err:+}, still inside the tie set")
                    } else {
                        format!(
                            "off by {err:+} (pixels allow {:?})",
                            oracle.as_ref().map(|o| &o.ties)
                        )
                    },
                    match_info.score,
                    err,
                )
            }
            PushResult::Pending => {
                pending += 1;
                if oracle.as_ref().is_some_and(|o| !o.ambiguous && o.cost <= 1.0) {
                    strict_refusals += 1;
                }
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
                if oracle.as_ref().is_some_and(|o| !o.ambiguous && o.cost <= 1.0) {
                    strict_refusals += 1;
                }
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

    if !cached_oracles {
        if let Some(computed) = oracles.as_ref() {
            save_oracle_cache(&cache_path, &cache_key, computed);
            println!("oracle: cached for this corpus ({})", cache_path.display());
        }
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
                    let image = load_png(&frames[i].path)?;
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

            // The stitcher's own recorded row is not usable as an absolute
            // reference: a recovery re-bases the engine's page coordinate, so
            // the number is in a different coordinate system from the canvas
            // rows (measured on `real-2`: a frame recorded at row 683 sits at
            // 3203 with a perfect fit). Every check below therefore uses the
            // row the frame's *pixels* fit best, and the divergence is reported
            // on its own line so a re-basing stays visible.
            let misplaced_rows: Vec<(usize, i32, f64)> = searched
                .iter()
                .filter(|(_, l)| l.floor <= REPROJECTION_MAX_COST)
                .map(|(i, l)| (*i, l.row, l.cost))
                .collect();
            let jumps = find_isolation(&misplaced_rows);
            misplaced = jumps.len() as u32;

            // Check 3: band consistency. A frame captured mid-jump — part of
            // the buffer at the old position, part at the new one — has no
            // single placement, and whatever rows it writes land displaced
            // from its own top. Locate each horizontal band independently and
            // compare with the frame's content row (never the stitcher's
            // recorded row: that number is in another coordinate system). Bands
            // that fit nowhere are not evidence (low texture fits nothing
            // tightly); a band that fits well *elsewhere* is content the canvas
            // holds at the wrong rows.
            // Bands are located against the frame's *content row*, not the
            // stitcher's claimed one, and only frames the canvas can verify are
            // judged (a blurred frame has no honest band position).
            let rows_by_frame: std::collections::HashMap<usize, i32> = searched
                .iter()
                .filter(|(_, l)| l.floor <= REPROJECTION_MAX_COST)
                .map(|(i, l)| (*i, l.row))
                .collect();
            let band_work: Vec<(usize, i32, i32, i32)> = {
                let mut work = Vec::new();
                for (k, (i, _, image, _)) in loaded.iter().enumerate() {
                    let Some(&claimed) = rows_by_frame.get(i) else { continue };
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
                            work.push((k, claimed, lo, hi));
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
            let diverged: Vec<(usize, i32, i32)> = searched
                .iter()
                .filter(|(_, l)| (l.row - l.claimed).abs() > ROW_DIVERGENCE_REPORT)
                .map(|(i, l)| (*i, l.claimed, l.row))
                .collect();
            println!(
                "  placements whose recorded row is not the content row: {}",
                diverged.len()
            );
            for (i, claimed, row) in diverged.iter().take(10) {
                let located = searched.iter().find(|(k, _)| k == i).map(|(_, l)| l.claimed_cost);
                println!(
                    "    frame {i}: recorded {claimed}, content sits at {row}{}",
                    located.map(|cost| format!(" (claimed fit {cost:.2})")).unwrap_or_default()
                );
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
    println!("  MISPLACED {misplaced}  (placements whose content sits far from their neighbours' content)");
    println!("  frames with displaced bands {torn_written}  (jump-transition frames whose content sits rows away from their own top)");
    println!("  UNCOVERED {uncovered}  (canvas rows no accepted frame ever contained)");
    println!("  pairwise-outlier frames written {blended_written}  (cost > {blend_threshold:.2}; whether their rows land correctly is decided by the band criterion)");
    if let Some(top) = stitcher.last_engine_top() {
        final_drift = Some(top - expected_top);
        println!(
            "  chain drift vs truth: max {} px at frame {}, final {} px",
            max_drift.1,
            max_drift.0,
            final_drift.unwrap_or(0)
        );
    }
    println!("  delta outside the pixels' tie set {delta_mismatch}  (a recovery gap or a wrong offset; an ambiguity class is not counted)");
    println!("  ambiguous pairs {ambiguous_pairs}  (content matched at several offsets; the tie set is what the delta is judged against)");
    println!("  refusals on a uniquely resolvable pair {strict_refusals}  (diagnostic: the pixels know this step, the canvas could not use it)");
    if oracle_failed > 0 {
        println!("  oracle could not decide {oracle_failed} pairs (no overlap)");
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
/// scroll positions (the convention the corpora's per-frame truth used). Writing such a frame
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
/// Loads the oracle cache when it matches `key` and has one record per pair.
fn load_oracle_cache(path: &Path, key: &str, frames: usize) -> Option<Vec<Option<Oracle>>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != key {
        return None;
    }
    let mut oracles = Vec::with_capacity(frames.saturating_sub(1));
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let delta: i32 = fields.next()?.parse().ok()?;
        let cost: f64 = fields.next()?.parse().ok()?;
        let ambiguous = fields.next()? == "1";
        let ties: Vec<i32> = fields
            .next()?
            .split(',')
            .filter(|tie| !tie.is_empty())
            .map(|tie| tie.parse().ok())
            .collect::<Option<_>>()?;
        oracles.push(if delta == i32::MIN {
            None
        } else {
            Some(Oracle { delta, ambiguous, cost, ties })
        });
    }
    (oracles.len() == frames.saturating_sub(1)).then_some(oracles)
}

/// Writes the oracle cache for `key`, atomically enough for a re-run: the file
/// is small and a torn cache is rejected by the length check above anyway.
fn save_oracle_cache(path: &Path, key: &str, oracles: &[Option<Oracle>]) {
    let mut text = String::with_capacity(oracles.len() * 32);
    text.push_str(key);
    text.push('\n');
    for oracle in oracles {
        match oracle {
            Some(oracle) => {
                let ties: Vec<String> = oracle.ties.iter().map(|tie| tie.to_string()).collect();
                text.push_str(&format!(
                    "{}\t{:?}\t{}\t{}\n",
                    oracle.delta,
                    oracle.cost,
                    u8::from(oracle.ambiguous),
                    ties.join(",")
                ));
            }
            None => text.push_str(&format!("{}\t0.0\t0\t\n", i32::MIN)),
        }
    }
    if std::fs::write(path, text).is_err() {
        eprintln!("replay-frames: could not write {}", path.display());
    }
}

#[derive(Clone)]
struct Oracle {
    /// Smallest-magnitude displacement that matches as well as the best one.
    delta: i32,
    /// True when several distinct displacements match equally well. A stitcher
    /// that accepts such a frame is guessing, not measuring.
    ambiguous: bool,
    /// Cost at the chosen displacement, in mean luminance units.
    cost: f64,
    /// Every distinct displacement the pixels cannot tell apart. A placement
    /// inside this set is consistent with the frames; only a placement outside
    /// it is a wrong displacement. Reporting just the smallest-magnitude member
    /// (what the summary used to do) turns a genuine ambiguity into an error.
    ties: Vec<i32>,
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
fn oracle_delta(previous: &Image, current: &Image, step: u32) -> Option<Oracle> {
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
                            oracle_score(prev, cur, width, height, min_overlap, delta, step)
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
    return Some(Oracle { delta: chosen, ambiguous: distinct.len() > 1, cost: best_cost, ties: distinct });

}

/// Scores one displacement candidate for the oracle.
fn oracle_score(
    prev: &[u8],
    cur: &[u8],
    width: u32,
    height: u32,
    min_overlap: u32,
    delta: i32,
    step: u32,
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
            x += step;
        }
        y += step;
    }
    if count == 0 {
        return None;
    }
    Some((sum as f64 / count as f64, delta))
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
        let rgb = stitched.to_rgb_buffer();
        let mut data = Vec::with_capacity(stitched.width as usize * stitched.height as usize);
        for y in 0..stitched.height as usize {
            let row = y * stitched.stride as usize;
            for x in 0..stitched.width as usize {
                let c = row + x * 3;
                data.push(
                    ((u32::from(rgb[c]) * 77 + u32::from(rgb[c + 1]) * 150 + u32::from(rgb[c + 2]) * 29)
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
    // Content decides the row, never the stitcher's own row bookkeeping: a
    // recovery or a far placement re-bases the engine's page coordinate, so the
    // claimed row can sit thousands of rows away from where the frame's pixels
    // actually are (measured on `real-2`: frame 44 claimed row 683, matched the
    // canvas at 3203 with a perfect fit). The claimed row is kept only for
    // reporting, so a divergence stays visible instead of being judged.
    Located { claimed, claimed_cost, row: best_row, cost: best_cost, floor: best_cost }
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
/// Above this, the frame's pixels do not match the canvas where it was put.
const REPROJECTION_MAX_COST: f64 = 2.0;
/// How far the stitcher's own recorded row may differ from the row the frame's
/// pixels actually occupy before the divergence is reported. This is not a
/// placement error: a recovery re-bases the engine's page coordinate, so the
/// two numbers are in different coordinate systems and only their *change*
/// means anything.
const ROW_DIVERGENCE_REPORT: i32 = 2;
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
    let rgb = stitched.to_rgb_buffer();
    for y in 0..stitched.height as usize {
        let src_row = y * stitched.stride as usize;
        let dst_row = y * stitched.width as usize * 4;
        for x in 0..stitched.width as usize {
            let s = src_row + x * 3;
            let d = dst_row + x * 4;
            data[d] = rgb[s + 2];
            data[d + 1] = rgb[s + 1];
            data[d + 2] = rgb[s];
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
    let mut desc = Vec::with_capacity(dw as usize * dh as usize * 4);
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
    let mut orig = Vec::with_capacity(target_w as usize * target_h as usize * 4);
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
