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

pub fn run() -> Result<(), Box<dyn Error>> {
    match Command::parse()? {
        Command::Screenshot(args) if args.help => {
            print!("{}", crate::cli::HELP);
            Ok(())
        }
        Command::Screenshot(args) if args.mode == Mode::Long => run_long(args),
        Command::Screenshot(args) => run_normal(args),
        Command::ClipboardProvider(args) => crate::clipboard::serve_path(&args.path, args.mode),
    }
}

fn run_normal(args: crate::cli::Args) -> Result<(), Box<dyn Error>> {
    crate::runtime::ensure_niri_session()?;
    let frozen_outputs = crate::wayland::screencopy::capture_outputs_with_cursor()?;
    let frozen_outputs_without_cursor = crate::wayland::screencopy::capture_outputs_without_cursor()?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen_pair(
        &frozen_outputs,
        &frozen_outputs_without_cursor,
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
            run_long_capture(args, viewport, session)
        }
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            session.close()?;
            Err("selection cancelled".into())
        }
    }
}

fn run_long(args: crate::cli::Args) -> Result<(), Box<dyn Error>> {
    crate::runtime::ensure_niri_session()?;
    let mut selection_session = crate::wayland::selection::SelectionSession::new_long()?;
    let viewport = match selection_session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::LongModeRequested(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            selection_session.close()?;
            return Err("selection cancelled".into());
        }
    };
    run_long_capture(args, viewport, selection_session)
}

enum CaptureMessage {
    PrepareCapture { #[allow(dead_code)] index: usize, ack: mpsc::SyncSender<()> },
    Frame { image: crate::image::Image, region: CaptureOutputRegion },
    Error(#[allow(dead_code)] String),
    Finished,
}

struct LongCaptureWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    region: std::sync::Arc<std::sync::Mutex<CaptureOutputRegion>>,
    receiver: mpsc::Receiver<CaptureMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LongCaptureWorker {
    fn start(region: CaptureOutputRegion, max_frame_rate: u32) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let region = std::sync::Arc::new(std::sync::Mutex::new(region));
        let (sender, receiver) = mpsc::sync_channel::<CaptureMessage>(4);
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread_region = std::sync::Arc::clone(&region);
        let frame_interval = long_frame_interval(max_frame_rate);
        let thread = std::thread::spawn(move || {
            let output_name = thread_region.lock().map(|region| region.output_name).unwrap_or_default();
            let mut session = match crate::wayland::screencopy::CaptureSession::new(output_name) {
                Ok(s) => s,
                Err(err) => {
                    let _ = sender.send(CaptureMessage::Error(err.to_string()));
                    return;
                }
            };
            let mut last: Option<std::time::Instant> = None;
            let mut index: usize = 0;
            while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
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
                let region = match thread_region.lock() {
                    Ok(region) => *region,
                    Err(_) => return,
                };
                let (ack_tx, ack_rx) = mpsc::sync_channel(0);
                if sender.send(CaptureMessage::PrepareCapture { index, ack: ack_tx }).is_err() {
                    return;
                }
                loop {
                    if thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    match ack_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(_) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                match session.capture_region_frame_without_cursor(region, index > 0) {
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
        Self { stop, region, receiver, thread: Some(thread) }
    }

    /// Updates the capture region used for subsequent frames. Selection-box
    /// edits in long mode call this so the worker follows the resized box.
    fn update_region(&self, region: CaptureOutputRegion) {
        if let Ok(mut current) = self.region.lock() {
            *current = region;
        }
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
) -> Result<(), Box<dyn Error>> {
    if viewport.regions.len() != 1 {
        selection_session.close()?;
        return Err("long capture selection must be contained within a single output".into());
    }
    if let Err(err) = selection_session.set_selected_viewport_passthrough(&viewport) {
        selection_session.close()?;
        return Err(err);
    }

    let initial_region = viewport.capture_regions()[0];
    let mut worker = LongCaptureWorker::start(initial_region, args.max_frame_rate);
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
                let _ = selection_session.prepare_capture_clean();
                let _ = ack.send(());
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
                    let _ = ack.send(());
                }
            }
            break;
        }
        let _ = selection_session.dispatch_poll(0);
        // Follow selection-box edits made in edit mode: the worker region and
        // the preview mapping track the adjusted rectangle, and each frame is
        // tagged with the region it was captured from so the stitcher can
        // place geometry-changed frames deterministically.
        if let Some(rect) = selection_session.selection_rect() {
            if rect != live_rect {
                match crate::geometry::SelectedViewport::from_outputs(
                    rect,
                    &selection_session.output_infos(),
                ) {
                    Ok(viewport) if viewport.regions.len() == 1 => {
                        live_rect = rect;
                        live_region = viewport.capture_regions()[0];
                        worker.update_region(live_region);
                    }
                    _ => {}
                }
            }
        }
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
