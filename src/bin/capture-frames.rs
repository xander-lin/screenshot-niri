//! Captures the frames a long screenshot would consume, and nothing else.
//!
//! This exists so the stitcher can be debugged against the frames that were
//! actually on screen, captured through the exact same path production uses
//! (`zwlr_screencopy_manager_v1` on the composited output, with the overlay
//! rendered clean for the capture). It performs no stitching and never shows
//! the stitched preview — but everything else is the production flow:
//!
//! * the same selection overlay, `L`/`E`/arrow keys/`Enter`/`Space`/`Esc`,
//! * edit mode (`E`) with edge resize and box move, including the pause while a
//!   drag is in progress and the geometry handshake on release,
//! * the same `prepare_capture_clean` → capture → `complete_capture` handshake,
//!   so a frame is never captured against stale geometry and the border is
//!   never part of it,
//! * the same cursor policy (captured without the pointer),
//! * the same frame-rate cap (`--fps`, else `max_fps` from the config file).
//!
//! Frames are written as PNG by a writer thread, so encoding a 4K frame never
//! becomes the capture cadence: the corpus has to look like what production
//! consumes, not like what this tool can encode in time.
//!
//! Usage:
//!
//!     capture-frames --out DIR [--fps N] [--max-frames N] [--timeout SECS]
//!
//! Drag a region (or press `L`), then scroll however you normally would. `E`
//! edits the box, the arrow keys hint the scroll direction, `Enter`/`Space`
//! finishes, `Esc` cancels. Each frame lands in `DIR` as `frame-NNNN.png` next
//! to a `session.json` describing the selection, the output geometry, and the
//! per-frame timing, direction and screen offset.

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use screenshot_rs_rebuild::config::Config;
use screenshot_rs_rebuild::geometry::{self, LogicalRect, OutputInfo};
use screenshot_rs_rebuild::image::{self, Image};
use screenshot_rs_rebuild::wayland::screencopy::{CaptureOutputRegion, CaptureSession};
use screenshot_rs_rebuild::wayland::selection::{
    LongSessionStatus, SearchDirection, SelectionOutcome, SelectionSession,
};

const PACE_SLEEP: Duration = Duration::from_millis(2);
/// Frames handed to the writer but not yet encoded.
///
/// Bounded, so a slow disk throttles the capture instead of growing memory
/// without limit (a 4K frame is 33 MB). Three is enough to cover one encode
/// while the next capture is in flight.
const WRITE_QUEUE: usize = 3;

struct Options {
    out: PathBuf,
    fps: Option<u32>,
    max_frames: Option<u32>,
    timeout: Option<Duration>,
}

fn parse_args() -> Result<Options, Box<dyn Error>> {
    let mut out: Option<PathBuf> = None;
    let mut fps = None;
    let mut max_frames = None;
    let mut timeout = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                out = Some(PathBuf::from(args.next().ok_or("--out needs a path")?));
            }
            "--fps" => {
                fps = Some(
                    args.next()
                        .ok_or("--fps needs a value")?
                        .parse::<u32>()
                        .map_err(|_| "--fps must be a number")?
                        .clamp(1, 240),
                );
            }
            "--max-frames" => {
                max_frames = Some(
                    args.next()
                        .ok_or("--max-frames needs a value")?
                        .parse::<u32>()
                        .map_err(|_| "--max-frames must be a number")?,
                );
            }
            "--timeout" => {
                let secs = args
                    .next()
                    .ok_or("--timeout needs seconds")?
                    .parse::<f64>()
                    .map_err(|_| "--timeout must be a number")?;
                timeout = Some(Duration::from_secs_f64(secs.max(0.1)));
            }
            "--help" | "-h" => {
                println!(
                    "capture-frames --out DIR [--fps N] [--max-frames N] [--timeout SECS]\n\n\
                     Drag a region (or press L), scroll, then press Enter/Space to finish.\n\
                     E edits the box, arrows hint the direction, Esc cancels.\n\
                     Writes frame-NNNN.png plus session.json into DIR."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(Options {
        out: out.ok_or("--out DIR is required")?,
        fps,
        max_frames,
        timeout,
    })
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

/// Refuses to mix two captures in one directory.
///
/// The manifest lists only this run's frames, but `stitch-bench` reads every
/// PNG in the directory — stale frames from an earlier run would silently
/// become part of the corpus.
fn ensure_fresh_output(dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut frames = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("frame-") && name.ends_with(".png") {
            frames += 1;
        }
    }
    if frames > 0 {
        return Err(format!(
            "{} already contains {frames} frame-*.png; use a fresh directory so two captures cannot mix",
            dir.display()
        )
        .into());
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("capture-frames: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let opts = parse_args()?;
    let config = Config::load();
    let fps = opts.fps.unwrap_or(config.max_fps).clamp(1, 240);
    std::fs::create_dir_all(&opts.out)?;
    ensure_fresh_output(&opts.out)?;

    let mut session = SelectionSession::new_long(config.border_width)?;
    let viewport = match session.run_selection()? {
        SelectionOutcome::Selected(viewport) | SelectionOutcome::LongModeRequested(viewport) => {
            viewport
        }
        SelectionOutcome::Cancelled => {
            session.close()?;
            return Err("selection cancelled".into());
        }
    };

    // Long capture is single-screen. Clamp exactly the way production does so
    // the frames here are the frames production would have consumed.
    let outputs = session.output_infos();
    let mut viewport = viewport;
    if viewport.regions.len() != 1 {
        let clamped = geometry::clamp_rect_to_largest_output(viewport.rect, &outputs);
        viewport = geometry::SelectedViewport::from_outputs(clamped, &outputs)?;
    }
    session.set_selected_viewport_passthrough(&viewport)?;

    let region = geometry::prepare_capture_region(viewport.rect, &outputs)
        .ok_or("long capture requires a selection on one output")?;

    // One connection for the whole session, like production's dedicated
    // capture worker: a fresh connection per frame would add latency between
    // frames and skew the timing this corpus is supposed to record.
    let mut capture = CaptureSession::new(region.output_name)?;

    let manifest = Manifest::new(viewport.rect, region, &outputs, fps);
    let (writer, writer_thread) = spawn_writer(opts.out.clone(), manifest)?;

    eprintln!(
        "capturing {}x{} from output {} at {} fps into {}",
        region.region.width,
        region.region.height,
        region.output_name,
        fps,
        opts.out.display()
    );
    eprintln!("scroll now; E edits the box, Enter or Space finishes, Esc cancels");

    let started = Instant::now();
    let mut index = 0u32;
    let mut last_capture: Option<Instant> = None;
    let mut writer_failed: Option<String> = None;
    let interval = frame_interval(fps);
    loop {
        // Input first, every iteration: during the pace wait this is what keeps
        // `E`, the arrow hints and `Enter` responsive at the capture cadence.
        session.dispatch_poll(0)?;
        match session.long_status() {
            LongSessionStatus::Running => {}
            LongSessionStatus::FinishRequested => break,
            LongSessionStatus::Cancelled => {
                eprintln!("cancelled");
                break;
            }
        }
        if let Some(limit) = opts.timeout {
            if started.elapsed() >= limit {
                eprintln!("timeout reached after {limit:?}");
                break;
            }
        }
        if let Some(limit) = opts.max_frames {
            if index >= limit {
                eprintln!("frame limit reached ({limit})");
                break;
            }
        }
        // Same cadence as production's worker: the interval runs from the
        // previous capture request, not from the end of its encode.
        if let Some(prev) = last_capture {
            if prev.elapsed() < interval {
                std::thread::sleep(PACE_SLEEP);
                continue;
            }
        }
        last_capture = Some(Instant::now());

        // The overlay is rendered clean for the capture and the region is
        // derived from the same rectangle at the same instant — the same
        // handshake production uses, so no border or preview pixels leak in.
        // An edit-mode drag returns `None` here, which is exactly production's
        // pause: intermediate geometry is never captured.
        let Some(rect) = session.prepare_capture_clean()? else {
            session.complete_capture();
            continue;
        };
        let Some(capture_region) = geometry::prepare_capture_region(rect, &session.output_infos())
        else {
            session.complete_capture();
            continue;
        };

        let frame_started = Instant::now();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let captured = capture.capture_region_frame_without_cursor(capture_region, false, &stop);
        let valid = session.complete_capture();

        match captured {
            Ok(Some(frame)) => {
                if !valid {
                    // A rectangle that moved during the capture: the frame is
                    // exposed to the old geometry. Dropping it here keeps the
                    // corpus honest about what production would have stitched.
                    eprintln!("frame {index}: dropped (geometry changed during capture)");
                } else {
                    let record = FrameRecord {
                        file: format!("frame-{index:04}.png"),
                        width: frame.width,
                        height: frame.height,
                        at_ms: started.elapsed().as_millis() as u64,
                        capture_ms: frame_started.elapsed().as_millis() as u64,
                        direction: direction_name(session.long_direction()).to_string(),
                        screen_offset: geometry::frame_screen_offset(capture_region, &frame),
                    };
                    if writer.send(WriteJob { image: frame, record }).is_err() {
                        writer_failed = Some("writer thread stopped".into());
                        break;
                    }
                    index += 1;
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("frame capture failed: {err}");
                break;
            }
        }
    }

    // Restore the desktop before waiting for the last encodes: the overlay is
    // the user's screen, and the writer has nothing to do with it.
    session.close()?;
    drop(writer);
    let written = match writer_thread.join() {
        Ok(Ok(written)) => written,
        Ok(Err(err)) => return Err(format!("writing frames failed: {err}").into()),
        Err(_) => return Err("writer thread panicked".into()),
    };
    if let Some(err) = writer_failed {
        return Err(err.into());
    }
    eprintln!("wrote {written} frames to {}", opts.out.display());
    Ok(())
}

fn direction_name(direction: Option<SearchDirection>) -> &'static str {
    match direction {
        Some(SearchDirection::Down) => "down",
        Some(SearchDirection::Up) => "up",
        Some(SearchDirection::Left) => "left",
        Some(SearchDirection::Right) => "right",
        Some(SearchDirection::Vertical) | None => "vertical",
    }
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

struct WriteJob {
    image: Image,
    record: FrameRecord,
}

/// Starts the encoder thread and returns the sending half plus its handle.
///
/// The handle resolves to the number of frames written, so the caller can
/// report a count that includes the frames still in the queue when the user
/// pressed `Enter`.
type Writer = (mpsc::SyncSender<WriteJob>, JoinHandle<Result<usize, String>>);

fn spawn_writer(dir: PathBuf, manifest: Manifest) -> Result<Writer, Box<dyn Error>> {
    let (sender, receiver) = mpsc::sync_channel::<WriteJob>(WRITE_QUEUE);
    let mut manifest = manifest;
    manifest.write(&dir, true)?;
    let handle = std::thread::spawn(move || -> Result<usize, String> {
        while let Ok(job) = receiver.recv() {
            let path = dir.join(&job.record.file);
            write_png_fast(&path, &job.image).map_err(|err| format!("{}: {err}", path.display()))?;
            manifest.frames.push(job.record);
            // No fsync per frame: the manifest is a few kilobytes and an fsync
            // in the capture path is exactly the kind of stall this thread
            // exists to keep out of it. The final write is synced.
            manifest
                .write(&dir, false)
                .map_err(|err| format!("session.json: {err}"))?;
        }
        manifest
            .write(&dir, true)
            .map_err(|err| format!("session.json: {err}"))?;
        Ok(manifest.frames.len())
    });
    Ok((sender, handle))
}

/// Encodes one frame for a corpus, not for delivery.
///
/// The corpus is read by this repository's own tools, so encode time matters
/// and file size does not: a 4K frame must fit inside a 30 fps slot
/// (`Compression::Fast` + `Filter::Sub` measured 27 ms against 84 ms for the
/// delivery settings used by `image::write_png`).
fn write_png_fast(path: &Path, image: &Image) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_filter(png::Filter::Sub);
    encoder
        .write_header()?
        .write_image_data(&image::to_rgba(image)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

struct Manifest {
    rect: LogicalRect,
    region: CaptureOutputRegion,
    outputs: Vec<OutputManifest>,
    fps: u32,
    frames: Vec<FrameRecord>,
}

struct OutputManifest {
    global_name: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: i32,
}

struct FrameRecord {
    file: String,
    width: u32,
    height: u32,
    at_ms: u64,
    capture_ms: u64,
    direction: String,
    screen_offset: screenshot_rs_rebuild::stitch::FrameScreenOffset,
}

impl Manifest {
    fn new(
        rect: LogicalRect,
        region: CaptureOutputRegion,
        outputs: &[OutputInfo],
        fps: u32,
    ) -> Self {
        Self {
            rect,
            region,
            outputs: outputs
                .iter()
                .map(|o| OutputManifest {
                    global_name: o.global_name,
                    x: o.logical.x,
                    y: o.logical.y,
                    width: o.logical.width,
                    height: o.logical.height,
                    scale: o.scale,
                })
                .collect(),
            fps,
            frames: Vec::new(),
        }
    }

    fn write(&self, dir: &Path, durable: bool) -> Result<(), Box<dyn Error>> {
        let mut json = String::new();
        json.push_str("{\n");
        json.push_str(&format!(
            "  \"selection\": {{\"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}}},\n",
            self.rect.x, self.rect.y, self.rect.width, self.rect.height
        ));
        json.push_str(&format!(
            "  \"region\": {{\"output\": {}, \"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}}},\n",
            self.region.output_name,
            self.region.region.x,
            self.region.region.y,
            self.region.region.width,
            self.region.region.height
        ));
        json.push_str(&format!("  \"fps\": {},\n", self.fps));
        json.push_str("  \"outputs\": [\n");
        for (i, o) in self.outputs.iter().enumerate() {
            json.push_str(&format!(
                "    {{\"name\": {}, \"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}, \"scale\": {}}}{}\n",
                o.global_name,
                o.x,
                o.y,
                o.width,
                o.height,
                o.scale,
                if i + 1 == self.outputs.len() { "" } else { "," }
            ));
        }
        json.push_str("  ],\n");
        json.push_str("  \"frames\": [\n");
        for (i, f) in self.frames.iter().enumerate() {
            json.push_str(&format!(
                "    {{\"file\": \"{}\", \"width\": {}, \"height\": {}, \"at_ms\": {}, \"capture_ms\": {}, \"direction\": \"{}\", \"offset_x\": {}, \"offset_y\": {}}}{}\n",
                f.file,
                f.width,
                f.height,
                f.at_ms,
                f.capture_ms,
                f.direction,
                f.screen_offset.x,
                f.screen_offset.y,
                if i + 1 == self.frames.len() { "" } else { "," }
            ));
        }
        json.push_str("  ]\n");
        json.push_str("}\n");

        let path = dir.join("session.json");
        let mut file = std::fs::File::create(&path)?;
        file.write_all(json.as_bytes())?;
        if durable {
            file.sync_all()?;
        }
        Ok(())
    }
}
