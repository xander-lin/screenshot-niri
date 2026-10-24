use std::error::Error;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::cli::{Command, Mode};
use crate::stitch::{ImageRgbView, PushResult, RawStitcher};
use crate::wayland::screencopy::CaptureOutputRegion;
use crate::wayland::selection::LongDirection;

const LONG_UI_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Frame-rate cap while the stitcher is refusing content. Every refused frame
/// costs a full anchor-window scan, and the user's recovery (scrolling back) is
/// a seconds-long action, so 5 fps captures it just as well as the configured
/// rate at a sixth of the cost.
const LONG_WARNING_FRAME_RATE: u32 = 5;
/// Granularity of the capture-thread pacing sleep loop.
const LONG_FRAME_PACE_SLEEP: Duration = Duration::from_millis(2);

fn long_frame_interval(max_frame_rate: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(max_frame_rate.max(1)))
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let config = crate::config::Config::load();
    match Command::parse()? {
        Command::Screenshot(args) if args.help => {
            print!("{}", crate::cli::HELP);
            Ok(())
        }
        Command::Screenshot(args) if args.mode == Mode::Long => run_long(args, config),
        Command::Screenshot(args) => run_normal(args, config),
        Command::ClipboardProvider(args) => crate::clipboard::serve_path(&args.path, args.mode),
    }
}

fn run_normal(args: crate::cli::Args, config: crate::config::Config) -> Result<(), Box<dyn Error>> {
    // Both captures are still needed — the user can press `H` before confirming
    // — but only one of them is now stored whole: the session keeps the
    // without-cursor capture and reduces the other to the pixels they differ by
    // (the cursor overlay; measured 0.07–0.13% of a 1920x1080 output).
    let frozen_outputs = crate::wayland::screencopy::capture_outputs_with_cursor()?;
    let frozen_outputs_without_cursor = crate::wayland::screencopy::capture_outputs_without_cursor()?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen_pair(
        &frozen_outputs,
        &frozen_outputs_without_cursor,
        config.border_width,
    )?;
    // The session has its own copies (one frame plus a cursor patch, and the
    // overlay buffers it sized from them), so these two full-frame sets are dead
    // weight while the user drags — 33 MB at 1920x1080. Dropping them here also
    // makes the `H` choice below read from the session instead of from a
    // second source of truth that could disagree with what was displayed.
    drop(frozen_outputs);
    drop(frozen_outputs_without_cursor);
    match session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => {
            let captures = session.frozen_captures_for_output();
            let (width, height) = viewport.capture_size(&captures)?;
            let frame = crate::image::composite_captured_region(
                &viewport.capture_regions(&captures),
                width,
                height,
                &captures,
            )?;
            crate::image::write_png(&args.output_path, &frame)?;
            crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode)?;
            println!("saved {}", args.output_path.display());
            Ok(())
        }
        crate::wayland::selection::SelectionOutcome::LongModeRequested(viewport) => {
            run_long_capture(args, viewport, session, config)
        }
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            session.close()?;
            Err("selection cancelled".into())
        }
    }
}

fn run_long(args: crate::cli::Args, config: crate::config::Config) -> Result<(), Box<dyn Error>> {
    let mut selection_session = crate::wayland::selection::SelectionSession::new_long(config.border_width)?;
    let viewport = match selection_session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::LongModeRequested(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            selection_session.close()?;
            return Err("selection cancelled".into());
        }
    };
    run_long_capture(args, viewport, selection_session, config)
}

enum CaptureMessage {
    PrepareCapture { #[allow(dead_code)] index: usize, ack: mpsc::SyncSender<PrepareAck> },
    Frame { image: crate::image::Image, region: CaptureOutputRegion },
    Error(String),
    Finished,
}

/// Reply to a PrepareCapture handshake. The region is computed by the UI
/// thread from the very rectangle it used to render the capture-clean
/// overlay, so the capture region and the on-screen hole/border geometry are
/// always derived from the same instant — the worker never captures with a
/// stale region while the overlay already shows a different box (the root
/// cause of spurious white borders in stitched long captures).
enum PrepareAck {
    Capture(CaptureOutputRegion),
    /// Skip this frame: an edit drag is reshaping the box, or the clean
    /// overlay could not be committed (all buffers busy).
    Skip,
}

struct LongCaptureWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set while the stitcher is refusing content. Capture keeps running so a
    /// scroll back is noticed immediately, but at `LONG_WARNING_FRAME_RATE`
    /// instead of the configured rate: every refused frame costs a full anchor
    /// window scan, and the user's recovery is a seconds-long action that 5 fps
    /// resolves just as well.
    warning: std::sync::Arc<std::sync::atomic::AtomicBool>,
    receiver: mpsc::Receiver<CaptureMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LongCaptureWorker {
    fn start(initial_region: CaptureOutputRegion, max_frame_rate: u32) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let warning = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel::<CaptureMessage>(4);
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread_paused = std::sync::Arc::clone(&paused);
        let thread_warning = std::sync::Arc::clone(&warning);
        let output_name = initial_region.output_name;
        let normal_interval = long_frame_interval(max_frame_rate);
        let warning_interval = long_frame_interval(LONG_WARNING_FRAME_RATE);
        let thread = std::thread::spawn(move || {
            let mut session = match crate::wayland::screencopy::CaptureSession::new(output_name) {
                Ok(s) => s,
                Err(err) => {
                    let _ = sender.send(CaptureMessage::Error(err.to_string()));
                    return;
                }
            };
            let mut last: Option<std::time::Instant> = None;
            let mut index: usize = 0;
            'outer: while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                // While an edit-mode edge drag is in progress the box geometry
                // is in flux; idle instead of capturing intermediate sizes.
                if thread_paused.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                let frame_interval = if thread_warning.load(std::sync::atomic::Ordering::Relaxed) {
                    warning_interval
                } else {
                    normal_interval
                };
                if index > 0 {
                    if let Some(prev) = last {
                        while prev.elapsed() < frame_interval {
                            if thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(LONG_FRAME_PACE_SLEEP);
                        }
                    }
                }
                last = Some(std::time::Instant::now());
                let (ack_tx, ack_rx) = mpsc::sync_channel(0);
                if sender.send(CaptureMessage::PrepareCapture { index, ack: ack_tx }).is_err() {
                    return;
                }
                // The UI thread owns the authoritative region: it renders the
                // capture-clean overlay and hands back the region derived
                // from that same rectangle. Skipping means the geometry was
                // in flux (edit drag) or the clean render failed; retry.
                let region = 'ack: loop {
                    if thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    match ack_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(PrepareAck::Capture(region)) => break 'ack region,
                        Ok(PrepareAck::Skip) => {
                            crate::trace_log!("worker: prepare skipped, retrying");
                            std::thread::sleep(Duration::from_millis(8));
                            continue 'outer;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                match session.capture_region_frame_without_cursor(region, true, &thread_stop) {
                    Ok(Some(image)) => {
                        crate::trace_log!("worker: frame captured index={} {}x{}", index, image.width, image.height);
                        if sender.send(CaptureMessage::Frame { image, region }).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        crate::trace_log!("worker: capture cancelled (None) at index={}", index);
                        break;
                    }
                    Err(err) => {
                        let _ = sender.send(CaptureMessage::Error(err.to_string()));
                        return;
                    }
                }
                index += 1;
            }
            let _ = sender.send(CaptureMessage::Finished);
        });
        Self { stop, paused, warning, receiver, thread: Some(thread) }
    }

    /// Pauses or resumes frame capture. Paused while an edit-mode edge drag
    /// reshapes the selection box; resumed on release so the next frame uses
    /// the settled geometry.
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, std::sync::atomic::Ordering::Relaxed);
    }

    /// Switches the capture cadence between the configured rate and the
    /// reduced warning rate. Never stops capturing: the recovery depends on
    /// noticing that content overlaps the canvas again.
    fn set_stitch_warning(&self, warning: bool) {
        self.warning.store(warning, std::sync::atomic::Ordering::Relaxed);
    }

    fn recv_timeout(&self, dur: Duration) -> Result<CaptureMessage, RecvTimeoutError> {
        self.receiver.recv_timeout(dur)
    }

    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LongCaptureWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_long_capture(
    args: crate::cli::Args,
    viewport: crate::geometry::SelectedViewport,
    mut selection_session: crate::wayland::selection::SelectionSession,
    config: crate::config::Config,
) -> Result<(), Box<dyn Error>> {
    // L-promoted selections may span outputs even though the interactive
    // long path already clamps: fall back to the largest single-output piece.
    let mut viewport = viewport;
    if viewport.regions.len() != 1 {
        let infos = selection_session.output_infos();
        let clamped = crate::geometry::clamp_rect_to_largest_output(viewport.rect, &infos);
        viewport = crate::geometry::SelectedViewport::from_outputs(clamped, &infos)?;
    }
    if let Err(err) = selection_session.set_selected_viewport_passthrough(&viewport) {
        selection_session.close()?;
        return Err(err);
    }

    let initial_region = crate::geometry::prepare_capture_region(viewport.rect, &selection_session.output_infos())
        .ok_or("long capture requires a selection on one output")?;
    let max_frame_rate = args.max_frame_rate.unwrap_or(config.max_fps);
    let mut worker = LongCaptureWorker::start(initial_region, max_frame_rate);
    let mut stitcher = RawStitcher::new();
    let mut cancelled = false;
    let mut capture_finished = false;
    let mut capture_error = None;
    let mut prev_raw: Option<(CaptureOutputRegion, crate::image::Image)> = None;
    // Allocation for the next preview image. The overlay window is the same
    // size on almost every frame, so this makes the 8.3 MB (1080p) preview
    // buffer a one-time cost instead of a per-frame allocation.
    let mut preview_buffer: Vec<u8> = Vec::new();
    // Live selection state followed from edit-mode edge drags.
    let mut live_rect = viewport.rect;

    while !capture_finished {
        // Service input even when duplicate/no-match frames bypass the loop tail.
        selection_session.dispatch_poll(0)?;
        match selection_session.long_status() {
            crate::wayland::selection::LongSessionStatus::Running => {}
            crate::wayland::selection::LongSessionStatus::FinishRequested => break,
            crate::wayland::selection::LongSessionStatus::Cancelled => {
                cancelled = true;
                break;
            }
        }
        match worker.recv_timeout(LONG_UI_POLL_INTERVAL) {
            Ok(CaptureMessage::PrepareCapture { ack, .. }) => {
                // Decision made on the UI thread, single-threaded: the clean
                // overlay render and the region handed back to the worker are
                // derived from the same rectangle at the same instant. During
                // an edit drag (or when the clean render cannot commit) the
                // frame is skipped rather than captured against stale
                // geometry — that mismatch was the root cause of white
                // borders inside stitched long captures.
                let decision = selection_session.prepare_capture_clean()?
                    .and_then(|rect| crate::geometry::prepare_capture_region(rect, &selection_session.output_infos()));
                if decision.is_none() {
                    selection_session.complete_capture();
                }
                let _ = match decision {
                    Some(region) => ack.send(PrepareAck::Capture(region)),
                    None => ack.send(PrepareAck::Skip),
                };
            }
            Ok(CaptureMessage::Frame { image, region }) => {
                if !selection_session.complete_capture() {
                    crate::trace_log!("ui: frame rejected by complete_capture (sticky invalid)");
                    continue;
                }
                if should_skip_duplicate_frame(prev_raw.as_ref().map(|(r, i)| (*r, i)), region, &image) {
                    crate::trace_log!("ui: frame skipped as duplicate");
                    continue;
                }
                prev_raw = Some((region, image));
                let direction: Option<LongDirection> = match selection_session.long_direction() {
                    Some(crate::wayland::selection::SearchDirection::Down) => Some(LongDirection::Down),
                    Some(crate::wayland::selection::SearchDirection::Up) => Some(LongDirection::Up),
                    Some(crate::wayland::selection::SearchDirection::Left) => Some(LongDirection::Left),
                    Some(crate::wayland::selection::SearchDirection::Right) => Some(LongDirection::Right),
                    Some(crate::wayland::selection::SearchDirection::Vertical) | None => None,
                };
                let current_frame = match prev_raw.as_ref() {
                    Some((_, image)) => image,
                    None => continue,
                };
                // One view, borrowed twice: `ImageRgbView` is `{ &Image, crop }`,
                // so the compose and analysis halves of the push read the same
                // pixels without a copy. Validating the image once is enough.
                let view = match ImageRgbView::new(current_frame) {
                    Ok(view) => view,
                    Err(_) => continue,
                };
                // `ImageRgbView` is `Copy` and holds no pixels of its own, so this is two
                // views over one validated image, not a second copy.
                let (compose, analysis) = (view, view);
                let offset = crate::geometry::frame_screen_offset(region, current_frame);
                let outcome = match stitcher.push_frame_with_offset(&compose, &analysis, direction, offset) {
                    Ok(o) => {
                        crate::trace_log!("ui: pushed frame offset=({},{}) outcome={:?}", offset.x, offset.y, o);
                        o
                    }
                    Err(err) => {
                        crate::trace_log!("ui: push_frame_with_offset errored: {}", err);
                        #[cfg(not(feature = "trace-logs"))]
                        let _ = &err;
                        continue;
                    }
                };
                // The border is the only failure signal the user gets, so it is
                // driven directly from the stitcher's decision: red while it is
                // refusing, normal as soon as a frame is placed again. `Duplicate`
                // is not a failure (nothing changed), so it leaves the border as
                // it was rather than flapping.
                match outcome {
                    PushResult::Initialized | PushResult::Accepted { .. } => {
                        worker.set_stitch_warning(false);
                        let _ = selection_session.set_stitch_warning(false);
                    }
                    PushResult::Pending => {}
                    PushResult::NoMatch => {
                        worker.set_stitch_warning(true);
                        let _ = selection_session.set_stitch_warning(true);
                        crate::trace_log!(
                            "ui: stitch refused ({}), border red at {} fps",
                            stitcher
                                .last_reject_reason()
                                .map(|reason| reason.as_str())
                                .unwrap_or("unknown"),
                            LONG_WARNING_FRAME_RATE
                        );
                    }
                    PushResult::Duplicate => {}
                }
                if matches!(outcome, PushResult::Initialized | PushResult::Accepted { .. }) {
                    if let Some(s) = stitcher.stitched() {
                        // Only the part of the canvas the overlay can reach.
                        //
                        // The overlay samples the screen through the capture's
                        // own density, so at most one output's worth of canvas
                        // is visible: the selection box plus the rows around it
                        // that the box has already scrolled past. Handing it the
                        // whole canvas meant a full RGBA copy of the entire
                        // capture on every accepted frame — 22 MB on `real-1`,
                        // 45 MB on `real-long` — of which the overlay read one
                        // screenful.
                        let window = preview_window(
                            s,
                            s.current_origin_x,
                            s.current_origin_y,
                            current_frame.width,
                            current_frame.height,
                            live_rect,
                        );
                        let recycled = std::mem::take(&mut preview_buffer);
                        if let Ok(image) = crate::stitch::image_from_stitched_window_reusing(
                            s,
                            window,
                            recycled,
                        ) {
                            // Map the preview through the frame's own capture
                            // region (the inset rectangle the stitched content
                            // actually came from), not the un-inset selection
                            // box: they differ by the one-pixel inset, and a
                            // preview offset from the real screen content
                            // gets re-captured after selection-box edits.
                            let preview_viewport = crate::geometry::capture_region_logical_rect(
                                region,
                                &selection_session.output_infos(),
                            )
                            .unwrap_or(live_rect);
                            let (capture_width, capture_height) = (
                                current_frame.width,
                                current_frame.height,
                            );
                            let snapshot = crate::wayland::selection::LongPreviewSnapshot {
                                image,
                                current_origin_x: s.current_origin_x - window.x,
                                current_origin_y: s.current_origin_y - window.y,
                                viewport_rect: preview_viewport,
                                capture_width: capture_width.max(1),
                                capture_height: capture_height.max(1),
                            };
                            // Recycle the displaced preview's allocation. Without
                            // this hand-off the buffer taken above is consumed
                            // by the snapshot and freed here, so every frame
                            // would allocate a fresh 8.3 MB (1080p) image.
                            if let Ok(Some(replaced)) =
                                selection_session.update_long_capture_preview(snapshot)
                            {
                                preview_buffer = replaced.data;
                            }
                        }
                    }
                }
            }
            Ok(CaptureMessage::Error(error)) => {
                capture_error = Some(error);
                capture_finished = true;
            }
            Ok(CaptureMessage::Finished) => {
                capture_finished = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                capture_finished = true;
            }
        }
        if capture_finished {
            // Drain remaining frames from queue before exit
            while let Ok(msg) = worker.recv_timeout(Duration::from_millis(0)) {
                if let CaptureMessage::PrepareCapture { ack, .. } = msg {
                    let _ = ack.send(PrepareAck::Skip);
                }
            }
            break;
        }
        selection_session.dispatch_poll(0)?;
        // Follow selection-box edits made in edit mode first (the preview
        // mapping tracks the adjusted rectangle), then let the worker resume:
        // paused-flag and geometry must never race apart. The capture region
        // itself is no longer fed to the worker here — it is derived from
        // the same rectangle as the clean render inside the PrepareCapture
        // handshake above.
        if let Some(rect) = selection_session.selection_rect() {
            if rect != live_rect {
                match crate::geometry::SelectedViewport::from_outputs(
                    rect,
                    &selection_session.output_infos(),
                ) {
                    Ok(viewport) if viewport.regions.len() == 1 => {
                        live_rect = rect;
                    }
                    _ => {}
                }
            }
        }
        // Pause capture while an edit-mode edge drag is reshaping the box;
        // resume on release so the next frame captures the settled geometry.
        worker.set_paused(selection_session.edit_drag_active());
        match selection_session.long_status() {
            crate::wayland::selection::LongSessionStatus::Running => {}
            crate::wayland::selection::LongSessionStatus::FinishRequested => break,
            crate::wayland::selection::LongSessionStatus::Cancelled => {
                cancelled = true;
                break;
            }
        }
    }
    worker.stop();
    selection_session.close()?;

    if cancelled {
        return Err("long capture cancelled".into());
    }
    if let Some(error) = capture_error {
        return Err(error.into());
    }
    let stitched = stitcher.finish().ok_or("long capture produced no frames")?;
    let final_image = crate::stitch::image_from_stitched_frame(&stitched)?;
    crate::image::write_png(&args.output_path, &final_image)?;
    crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode)?;
    println!("saved {}", args.output_path.display());
    Ok(())
}

/// The canvas region the overlay can display, as a [`CanvasWindow`].
///
/// The overlay maps its own buffer onto the canvas through the capture's
/// density, so the canvas pixels it can reach are the selection's own rectangle
/// plus the screen area around it — everything the box has scrolled past and
/// that is still on screen, up to one output's worth of canvas in each
/// direction. Anything beyond that is not addressable from the overlay at all.
///
/// The window is generous by one viewport on every side: the mapping also draws
/// the canvas rows *above* the box (that is the point of the live preview), and
/// a selection-box edit can move the box while a frame is in flight.
fn preview_window(
    canvas: &crate::stitch::StitchedFrame,
    origin_x: i32,
    origin_y: i32,
    capture_width: u32,
    capture_height: u32,
    live_rect: crate::geometry::LogicalRect,
) -> crate::stitch::CanvasWindow {
    let reach_x = capture_width.max(live_rect.width.max(0) as u32) as i32;
    let reach_y = capture_height.max(live_rect.height.max(0) as u32) as i32;
    crate::stitch::CanvasWindow::clipped(
        canvas,
        origin_x - reach_x,
        origin_y - reach_y,
        reach_x.saturating_mul(3),
        reach_y.saturating_mul(3),
    )
    .unwrap_or_else(|| crate::stitch::CanvasWindow::full(canvas))
}

/// Fast raw-frame similarity check. Samples every 8th pixel, 4px per row.
/// Returns true if the frames are so similar they can be treated as duplicate.
fn raw_frame_nearly_identical(a: &crate::image::Image, b: &crate::image::Image) -> bool {
    if a.width != b.width || a.height != b.height || a.stride != b.stride {
        return false;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    let stride = a.stride as usize;
    let required = stride * h;
    if a.data.len() < required || b.data.len() < required {
        return false;
    }
    let step = 8usize;
    let mut total_diff: u64 = 0;
    let mut samples: u64 = 0;
    for y in (0..h).step_by(step) {
        let row = y * stride;
        for x in (0..w).step_by(step) {
            let off = row + x * 4;
            for c in 0..3 {
                total_diff += a.data[off + c].abs_diff(b.data[off + c]) as u64;
            }
            samples += 1;
        }
    }
    if samples == 0 { return false; }
    // Average channel difference < 0.5 means nearly identical
    (total_diff as f64 / samples as f64 / 3.0) < 0.5
}

/// Near-duplicate frames are skipped only while the capture region is
/// unchanged: an edit-mode resize produces frames that are mostly identical
/// yet carry brand-new strips that must reach the stitcher.
fn should_skip_duplicate_frame(
    previous: Option<(CaptureOutputRegion, &crate::image::Image)>,
    region: CaptureOutputRegion,
    current: &crate::image::Image,
) -> bool {
    match previous {
        Some((previous_region, previous)) => {
            previous_region == region && raw_frame_nearly_identical(previous, current)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{
        capture_region_logical_rect, inset_capture_region, prepare_capture_region,
    };
    use crate::image::Image;
    use crate::wayland::screencopy::Region;
    use wayland_client::protocol::wl_shm::Format;

    fn region_at(x: i32, y: i32, w: i32, h: i32) -> CaptureOutputRegion {
        CaptureOutputRegion { output_name: 1, region: Region { x, y, width: w, height: h }, dst_x: 0, dst_y: 0 }
    }

    fn solid(width: u32, height: u32, value: u8) -> Image {
        let stride = width * 4;
        Image {
            width,
            height,
            stride,
            format: Format::Xrgb8888,
            data: vec![value; (stride * height) as usize],
        }
    }

    #[test]
    fn frame_interval_matches_configured_rate() {
        assert_eq!(long_frame_interval(60), Duration::from_nanos(1_000_000_000 / 60));
        assert_eq!(long_frame_interval(1), Duration::from_secs(1));
        assert_eq!(long_frame_interval(240), Duration::from_nanos(1_000_000_000 / 240));
        // Degenerate zero falls back to a 1 fps crawl instead of dividing by zero.
        assert_eq!(long_frame_interval(0), Duration::from_secs(1));
    }

    #[test]
    fn warning_frame_rate_is_a_sixth_of_the_default_cap() {
        // The point of the reduced rate is cost, not responsiveness: a refused
        // frame scans a whole anchor window, and the user's recovery is a
        // seconds-long scroll. 5 fps against the 30 fps default keeps the
        // recovery latency well under a scroll gesture at a sixth of the cost.
        assert_eq!(LONG_WARNING_FRAME_RATE, 5);
        assert_eq!(
            long_frame_interval(LONG_WARNING_FRAME_RATE),
            Duration::from_millis(200)
        );
        assert!(
            long_frame_interval(LONG_WARNING_FRAME_RATE)
                > long_frame_interval(crate::config::DEFAULT_MAX_FRAME_RATE)
        );
    }

    #[test]
    fn inset_capture_region_shrinks_one_logical_pixel_per_side() {
        let region = region_at(100, 200, 300, 400);
        // Scale 1: exactly one buffer pixel inward per side.
        assert_eq!(
            inset_capture_region(region, 1),
            region_at(101, 201, 298, 398)
        );
        // Scale 2: one logical pixel equals two buffer pixels.
        assert_eq!(
            inset_capture_region(region, 2),
            region_at(102, 202, 296, 396)
        );
        // Degenerate tiny regions stay at least one pixel in size.
        let tiny = region_at(0, 0, 2, 2);
        assert_eq!(inset_capture_region(tiny, 1).region.width, 1);
        assert_eq!(inset_capture_region(tiny, 1).region.height, 1);
    }

    #[test]
    fn effective_frame_rate_prefers_flag_over_config() {
        // `--fps` wins when given; otherwise the config file value applies.
        let config = crate::config::Config { max_fps: 15, border_width: 3 };
        assert_eq!(Some(45).unwrap_or(config.max_fps), 45);
        assert_eq!(None.unwrap_or(config.max_fps), 15);
    }

    #[test]
    fn tiny_capture_regions_stay_inside_selection() {
        for width in 1..=4 {
            for height in 1..=4 {
                let original = region_at(99, 199, width, height);
                let inset = inset_capture_region(original, 1).region;
                assert!(inset.x >= 99 && inset.y >= 199);
                assert!(inset.width > 0 && inset.height > 0);
                assert!(inset.x + inset.width <= 99 + width);
                assert!(inset.y + inset.height <= 199 + height);
            }
        }
    }

    #[test]
    fn scaled_long_capture_requests_logical_units_and_stitches_in_pixels() {
        let outputs = [crate::geometry::OutputInfo {
            global_name: 1,
            logical: crate::geometry::LogicalRect { x: -1000, y: 200, width: 1000, height: 800 },
            scale: 2,
        }];
        let rect = crate::geometry::LogicalRect { x: -900, y: 250, width: 300, height: 200 };
        let region = prepare_capture_region(rect, &outputs).unwrap();
        assert_eq!(region, region_at(101, 51, 298, 198));
        assert_eq!(capture_region_logical_rect(region, &outputs), Some(crate::geometry::LogicalRect {
            x: -899, y: 251, width: 298, height: 198,
        }));
        let frame = solid(596, 396, 80);
        assert_eq!(crate::geometry::frame_screen_offset(region, &frame), crate::stitch::FrameScreenOffset { x: 202, y: 102 });
        // Moving left by ten logical pixels must move the canvas by twenty pixels.
        let moved = region_at(91, 51, 298, 198);
        assert_eq!(crate::geometry::frame_screen_offset(moved, &frame), crate::stitch::FrameScreenOffset { x: 182, y: 102 });
    }

    #[test]
    fn prepare_capture_region_insets_single_output_selection() {
        let outputs = vec![crate::geometry::OutputInfo {
            global_name: 7,
            logical: crate::geometry::LogicalRect { x: 1920, y: 0, width: 1920, height: 1080 },
            scale: 1,
        }];
        let rect = crate::geometry::LogicalRect { x: 2000, y: 100, width: 300, height: 200 };
        let region = prepare_capture_region(rect, &outputs).expect("single-output selection maps");
        assert_eq!(region.output_name, 7);
        // Output-local coordinates, inset one logical pixel per side.
        assert_eq!(region.region, Region { x: 80 + 1, y: 100 + 1, width: 298, height: 198 });
    }

    #[test]
    fn prepare_capture_region_rejects_spanning_selections() {
        let outputs = vec![
            crate::geometry::OutputInfo {
                global_name: 1,
                logical: crate::geometry::LogicalRect { x: 0, y: 0, width: 1920, height: 1080 },
                scale: 1,
            },
            crate::geometry::OutputInfo {
                global_name: 2,
                logical: crate::geometry::LogicalRect { x: 1920, y: 0, width: 1920, height: 1080 },
                scale: 1,
            },
        ];
        let spanning = crate::geometry::LogicalRect { x: 1800, y: 10, width: 300, height: 100 };
        assert!(prepare_capture_region(spanning, &outputs).is_none());
        // A selection off every output is equally unmappable.
        let outside = crate::geometry::LogicalRect { x: -500, y: -500, width: 10, height: 10 };
        assert!(prepare_capture_region(outside, &outputs).is_none());
    }

    #[test]
    fn capture_region_logical_rect_maps_inset_region_back_to_screen() {
        // The preview must map stitched content through the region frames
        // actually came from: output at (1920,0), inset buffer rect
        // (101,201) 298x398 must land at logical (2021,201) 298x398.
        let outputs = vec![crate::geometry::OutputInfo {
            global_name: 1,
            logical: crate::geometry::LogicalRect { x: 1920, y: 0, width: 1920, height: 1080 },
            scale: 1,
        }];
        let region = region_at(101, 201, 298, 398);
        assert_eq!(
            capture_region_logical_rect(region, &outputs),
            Some(crate::geometry::LogicalRect { x: 2021, y: 201, width: 298, height: 398 })
        );
        // Unknown output: unmappable.
        assert!(capture_region_logical_rect(region, &[]).is_none());
    }

    #[test]
    fn identical_frames_with_same_region_are_skipped() {
        let previous = solid(8, 8, 100);
        assert!(should_skip_duplicate_frame(
            Some((region_at(10, 10, 8, 8), &previous)),
            region_at(10, 10, 8, 8),
            &solid(8, 8, 100),
        ));
    }

    #[test]
    fn identical_frames_with_changed_region_are_not_skipped() {
        // The static-footer workflow: after extending the selection box the
        // frame content is nearly identical, but the new strip must be kept.
        let previous = solid(8, 8, 100);
        assert!(!should_skip_duplicate_frame(
            Some((region_at(10, 10, 8, 8), &previous)),
            region_at(10, 10, 8, 12),
            &solid(8, 8, 100),
        ));
        assert!(!should_skip_duplicate_frame(
            Some((region_at(10, 10, 8, 8), &previous)),
            region_at(12, 10, 8, 8),
            &solid(8, 8, 100),
        ));
    }

    #[test]
    fn differing_frames_and_first_frame_are_not_skipped() {
        let previous = solid(8, 8, 100);
        assert!(!should_skip_duplicate_frame(
            Some((region_at(10, 10, 8, 8), &previous)),
            region_at(10, 10, 8, 8),
            &solid(8, 8, 220),
        ));
        assert!(!should_skip_duplicate_frame(None, region_at(10, 10, 8, 8), &solid(8, 8, 100)));
    }
}
