//! Captures the frames a long screenshot would consume, and nothing else.
//!
//! This exists so the stitcher can be debugged against the frames that were
//! actually on screen, captured through the exact same path production uses
//! (`zwlr_screencopy_manager_v1` on the composited output, with the overlay
//! rendered clean for the capture). It performs no stitching, draws no border,
//! and never decides whether a frame is good — it writes every frame to disk
//! and lets the caller scroll.
//!
//! Usage:
//!
//!     capture-frames --out DIR [--fps N] [--max-frames N] [--timeout SECS]
//!
//! Drag a region, then scroll however you normally would. Press Enter (or
//! Space) in the overlay to finish. Each frame lands in `DIR` as
//! `frame-NNNN.png` next to a `session.json` describing the capture region,
//! the output geometry, and the per-frame timing.

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use screenshot_rs_rebuild::geometry::{self, LogicalRect, OutputInfo};
use screenshot_rs_rebuild::image;
use screenshot_rs_rebuild::wayland::screencopy::{CaptureOutputRegion, CaptureSession};
use screenshot_rs_rebuild::wayland::selection::{
    LongSessionStatus, SelectionOutcome, SelectionSession,
};

const PACE_SLEEP: Duration = Duration::from_millis(2);

struct Options {
    out: PathBuf,
    fps: u32,
    max_frames: Option<u32>,
    timeout: Option<Duration>,
}

fn parse_args() -> Result<Options, Box<dyn Error>> {
    let mut out: Option<PathBuf> = None;
    let mut fps = 30u32;
    let mut max_frames = None;
    let mut timeout = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                out = Some(PathBuf::from(args.next().ok_or("--out needs a path")?));
            }
            "--fps" => {
                fps = args
                    .next()
                    .ok_or("--fps needs a value")?
                    .parse::<u32>()
                    .map_err(|_| "--fps must be a number")?
                    .clamp(1, 240);
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
                     Drag a region, scroll, then press Enter/Space to finish.\n\
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

fn main() {
    if let Err(err) = run() {
        eprintln!("capture-frames: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let opts = parse_args()?;
    std::fs::create_dir_all(&opts.out)?;

    let mut session = SelectionSession::new_long(3)?;
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

    let mut manifest = Manifest::new(viewport.rect, region, &outputs, opts.fps);
    manifest.write(&opts.out)?;

    eprintln!(
        "capturing {}x{} from output {} at {} fps into {}",
        region.region.width,
        region.region.height,
        region.output_name,
        opts.fps,
        opts.out.display()
    );
    eprintln!("scroll now; press Enter or Space in the overlay when done");

    let started = Instant::now();
    let mut index = 0u32;
    let mut last_capture: Option<Instant> = None;
    loop {
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
        if let Some(prev) = last_capture {
            if prev.elapsed() < frame_interval(opts.fps) {
                std::thread::sleep(PACE_SLEEP);
                continue;
            }
        }

        // The overlay is rendered clean for the capture and the region is
        // derived from the same rectangle at the same instant — the same
        // handshake production uses, so no border or preview pixels leak in.
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
        let captured =
            capture.capture_region_frame_without_cursor(capture_region, false, &stop);
        let valid = session.complete_capture();

        match captured {
            Ok(Some(frame)) => {
                if !valid {
                    // A rectangle that moved during the capture: the frame is
                    // exposed to the old geometry. Dropping it here keeps the
                    // corpus honest about what production would have stitched.
                    eprintln!("frame {index}: dropped (geometry changed during capture)");
                } else {
                    let path = opts.out.join(format!("frame-{index:04}.png"));
                    image::write_png(&path, &frame)?;
                    let direction = match session.long_direction() {
                        Some(screenshot_rs_rebuild::wayland::selection::SearchDirection::Down) => "down",
                        Some(screenshot_rs_rebuild::wayland::selection::SearchDirection::Up) => "up",
                        Some(screenshot_rs_rebuild::wayland::selection::SearchDirection::Left) => "left",
                        Some(screenshot_rs_rebuild::wayland::selection::SearchDirection::Right) => "right",
                        Some(screenshot_rs_rebuild::wayland::selection::SearchDirection::Vertical)
                        | None => "vertical",
                    };
                    manifest.frames.push(FrameRecord {
                        file: format!("frame-{index:04}.png"),
                        width: frame.width,
                        height: frame.height,
                        at_ms: started.elapsed().as_millis() as u64,
                        capture_ms: frame_started.elapsed().as_millis() as u64,
                        direction: direction.to_string(),
                        screen_offset: geometry::frame_screen_offset(capture_region, &frame),
                    });
                    manifest.write(&opts.out)?;
                    index += 1;
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("frame capture failed: {err}");
                break;
            }
        }
        last_capture = Some(Instant::now());
    }

    manifest.write(&opts.out)?;
    session.close()?;
    eprintln!(
        "wrote {} frames to {}",
        manifest.frames.len(),
        opts.out.display()
    );
    Ok(())
}

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

    fn write(&self, dir: &std::path::Path) -> Result<(), Box<dyn Error>> {
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
        file.sync_all()?;
        Ok(())
    }
}
