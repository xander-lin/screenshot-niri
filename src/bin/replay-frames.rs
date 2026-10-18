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
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" | "-d" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--verbose" | "-v" => verbose = true,
            "--help" | "-h" => {
                println!("replay-frames --dir DIR [--verbose]");
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
    let mut oracle_failed = 0u32;
    let mut ambiguous_pairs = 0u32;
    let mut canvas_top = 0i32;
    let mut worst: Vec<(u32, i32)> = Vec::new();

    let first = load_image(&frames[0].path)?;
    stitcher.push_frame_with_offset(
        &ImageRgbView::new(&first)?,
        &ImageRgbView::new(&first)?,
        frames[0].direction,
        frames[0].offset,
    )?;
    let mut previous = first;

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

        let outcome = stitcher.push_frame_with_offset(
            &ImageRgbView::new(&image)?,
            &ImageRgbView::new(&image)?,
            entry.direction,
            entry.offset,
        )?;

        let mut frame_delta: Option<i32> = None;
        let (label, note, score, err) = match outcome {
            PushResult::Accepted { match_info } => {
                accepted += 1;
                canvas_top += match_info.delta_y;
                frame_delta = Some(match_info.delta_y);
                let err = if oracle_delta == i32::MIN {
                    i32::MIN
                } else {
                    // Compare displacements, not absolute positions: the oracle
                    // measures this pair only.
                    match_info.delta_y - oracle_delta
                };
                // A disagreement on content that matches at several offsets is
                // not a misplacement: the frame carries no answer.
                if err != i32::MIN && err.abs() > 1 && !oracle_ambiguous {
                    misplaced += 1;
                    worst.push((i as u32, err));
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

    println!("\nsummary");
    println!("  accepted  {accepted}");
    println!("  pending   {pending}");
    println!("  no-match  {refused}");
    println!("  duplicate {duplicate}");
    println!("  MISPLACED {misplaced}  (accepted with the wrong displacement)");
    println!("  ambiguous pairs {ambiguous_pairs}  (content matched at several offsets)");
    if oracle_failed > 0 {
        println!("  oracle could not decide {oracle_failed} pairs (no overlap)");
    }
    if !worst.is_empty() {
        println!("  worst misplacements: {worst:?}");
    }
    println!("  canvas height reached: {canvas_top}");
    Ok(())
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

    let mut scored: Vec<(f64, i32)> = Vec::new();
    for delta in -max_delta..=max_delta {
        let (cur_start, prev_start, rows) = if delta >= 0 {
            (0u32, delta as u32, height - delta as u32)
        } else {
            ((-delta) as u32, 0u32, height - (-delta) as u32)
        };
        if rows < min_overlap {
            continue;
        }
        let mut sum = 0u64;
        let mut count = 0u64;
        let mut y = 0u32;
        while y < rows {
            let a = ((prev_start + y) * width) as usize;
            let b = ((cur_start + y) * width) as usize;
            let mut x = 0u32;
            while x < width {
                let pa = prev[a + x as usize];
                let pb = cur[b + x as usize];
                sum += u64::from(pa.abs_diff(pb));
                count += 1;
                x += ORACLE_STEP;
            }
            y += ORACLE_STEP;
        }
        if count == 0 {
            continue;
        }
        scored.push((sum as f64 / count as f64, delta));
    }
    if scored.is_empty() {
        return None;
    }

    let best_cost = scored.iter().map(|(c, _)| *c).fold(f64::INFINITY, f64::min);
    // Two offsets are the same answer when their costs differ by less than the
    // sampling noise of the metric itself.
    let tolerance = best_cost * 0.02 + 0.5;
    let mut tied: Vec<i32> = scored
        .iter()
        .filter(|(c, _)| *c <= best_cost + tolerance)
        .map(|(_, d)| *d)
        .collect();
    tied.sort_unstable();
    // Adjacent offsets are the same peak, not a second answer.
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
    Some(Oracle {
        delta: chosen,
        ambiguous: distinct.len() > 1,
        cost: best_cost,
    })
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
