use std::error::Error;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::cli::{Command, Mode};
use crate::stitch::{FrameScreenOffset, ImageRgbView, PushResult, RawStitcher};
use crate::wayland::screencopy::CaptureOutputRegion;
use crate::wayland::selection::LongDirection;

const LONG_UI_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Granularity of the capture-thread pacing sleep loop.
const LONG_FRAME_PACE_SLEEP: Duration = Duration::from_millis(2);

fn long_frame_interval(max_frame_rate: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(max_frame_rate.max(1)))
}

/// Shrinks a capture region one logical pixel inward on every side (scaled
/// to buffer pixels) so the selection overlay's own border — however the
/// compositor rasterizes it — can never bleed into captured frames.
fn inset_capture_region(
    region: CaptureOutputRegion,
    scale: i32,
) -> CaptureOutputRegion {
    let inset = scale.max(1);
    CaptureOutputRegion {
        output_name: region.output_name,
        region: crate::wayland::screencopy::Region {
            x: region.region.x + inset,
            y: region.region.y + inset,
            width: (region.region.width - 2 * inset).max(1),
            height: (region.region.height - 2 * inset).max(1),
        },
        dst_x: region.dst_x,
        dst_y: region.dst_y,
    }
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
    crate::runtime::ensure_niri_session()?;
    let frozen_outputs = crate::wayland::screencopy::capture_outputs_with_cursor()?;
    let frozen_outputs_without_cursor = crate::wayland::screencopy::capture_outputs_without_cursor()?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen_pair(
        &frozen_outputs,
        &frozen_outputs_without_cursor,
        config.border_width,
    )?;
    match session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => {
            let captures = if session.screenshot_cursor_hidden() {
                &frozen_outputs_without_cursor
            } else {
                &frozen_outputs
            };
            session.close()?;
            let (width, height) = viewport.capture_size()?;
            let frame = crate::image::composite_captured_region(
                &viewport.capture_regions(),
                width,
                height,
                captures,
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
    crate::runtime::ensure_niri_session()?;
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
    Error(#[allow(dead_code)] String),
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

/// Capture region for a settled selection rectangle on a single output,
/// inset one logical pixel per side. Returns `None` for selections that do
/// not map to exactly one output (long capture cannot follow them).
fn prepare_capture_region(
    rect: crate::geometry::LogicalRect,
    outputs: &[crate::geometry::OutputInfo],
) -> Option<CaptureOutputRegion> {
    let viewport = crate::geometry::SelectedViewport::from_outputs(rect, outputs).ok()?;
    if viewport.regions.len() != 1 {
        return None;
    }
    let scale = viewport.regions[0].output_scale;
    Some(inset_capture_region(viewport.capture_regions()[0], scale))
}

struct LongCaptureWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    receiver: mpsc::Receiver<CaptureMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LongCaptureWorker {
    fn start(initial_region: CaptureOutputRegion, max_frame_rate: u32) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel::<CaptureMessage>(4);
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread_paused = std::sync::Arc::clone(&paused);
        let output_name = initial_region.output_name;
        let frame_interval = long_frame_interval(max_frame_rate);
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
                            std::thread::sleep(Duration::from_millis(8));
                            continue 'outer;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                match session.capture_region_frame_without_cursor(region, true) {
                    Ok(image) => {
                        if sender.send(CaptureMessage::Frame { image, region }).is_err() {
                            return;
                        }
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
        Self { stop, paused, receiver, thread: Some(thread) }
    }

    /// Pauses or resumes frame capture. Paused while an edit-mode edge drag
    /// reshapes the selection box; resumed on release so the next frame uses
    /// the settled geometry.
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, std::sync::atomic::Ordering::Relaxed);
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

    let initial_region = inset_capture_region(
        viewport.capture_regions()[0],
        viewport.regions[0].output_scale,
    );
    let max_frame_rate = args.max_frame_rate.unwrap_or(config.max_fps);
    let mut worker = LongCaptureWorker::start(initial_region, max_frame_rate);
    let mut stitcher = RawStitcher::new();
    let mut cancelled = false;
    let mut capture_finished = false;
    let mut prev_raw: Option<(CaptureOutputRegion, crate::image::Image)> = None;
    // Live selection state followed from edit-mode edge drags.
    let mut live_region = initial_region;
    let mut live_rect = viewport.rect;

    while !capture_finished {
        match worker.recv_timeout(LONG_UI_POLL_INTERVAL) {
            Ok(CaptureMessage::PrepareCapture { ack, .. }) => {
                // Decision made on the UI thread, single-threaded: the clean
                // overlay render and the region handed back to the worker are
                // derived from the same rectangle at the same instant. During
                // an edit drag (or when the clean render cannot commit) the
                // frame is skipped rather than captured against stale
                // geometry — that mismatch was the root cause of white
                // borders inside stitched long captures.
                let decision = if selection_session.edit_drag_active() {
                    None
                } else {
                    selection_session
                        .prepare_capture_clean()
                        .then(|| {
                            selection_session
                                .selection_rect()
                                .and_then(|rect| prepare_capture_region(rect, &selection_session.output_infos()))
                        })
                        .flatten()
                };
                let _ = match decision {
                    Some(region) => ack.send(PrepareAck::Capture(region)),
                    None => ack.send(PrepareAck::Skip),
                };
            }
            Ok(CaptureMessage::Frame { image, region }) => {
                if should_skip_duplicate_frame(prev_raw.as_ref().map(|(r, i)| (*r, i)), region, &image) {
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
                let compose = match ImageRgbView::new(current_frame) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let analysis = match ImageRgbView::new(current_frame) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let offset = FrameScreenOffset { x: region.region.x, y: region.region.y };
                let outcome = match stitcher.push_frame_with_offset(&compose, &analysis, direction, offset) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                if matches!(outcome, PushResult::Initialized | PushResult::Accepted { .. }) {
                    if let Some(s) = stitcher.stitched() {
                        if let Ok(image) = crate::stitch::image_from_stitched_frame(s) {
                            let (capture_width, capture_height) = (
                                u32::try_from(live_region.region.width.max(0)).unwrap_or(1),
                                u32::try_from(live_region.region.height.max(0)).unwrap_or(1),
                            );
                            let snapshot = crate::wayland::selection::LongPreviewSnapshot {
                                image,
                                current_origin_x: s.current_origin_x,
                                current_origin_y: s.current_origin_y,
                                viewport_rect: live_rect,
                                capture_width: capture_width.max(1),
                                capture_height: capture_height.max(1),
                            };
                            let _ = selection_session.update_long_capture_preview(snapshot);
                        }
                    }
                }
            }
            Ok(CaptureMessage::Error(_)) => {
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
        let _ = selection_session.dispatch_poll(0);
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
                        live_region = inset_capture_region(
                            viewport.capture_regions()[0],
                            viewport.regions[0].output_scale,
                        );
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
    let stitched = stitcher.finish().ok_or("long capture produced no frames")?;
    let final_image = crate::stitch::image_from_stitched_frame(&stitched)?;
    crate::image::write_png(&args.output_path, &final_image)?;
    crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode)?;
    println!("saved {}", args.output_path.display());
    Ok(())
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
