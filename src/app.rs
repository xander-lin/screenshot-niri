use std::error::Error;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use crate::cli::{Command, Mode};
use crate::stitch::{ImageRgbView, PushResult, RawStitcher, SearchDirection};

const LONG_UI_POLL_INTERVAL: Duration = Duration::from_millis(10);
const LONG_MAX_FRAME_RATE: u32 = 120;
const LONG_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / LONG_MAX_FRAME_RATE as u64);

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
    let frozen_outputs = crate::wayland::screencopy::capture_outputs(true)?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen(&frozen_outputs)?;
    let viewport = match session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::LongModeRequested(viewport) => {
            return run_long_capture(args, viewport, session);
        }
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            session.close()?;
            return Err("selection cancelled".into());
        }
    };
    session.close()?;
    let (width, height) = viewport.capture_size()?;
    let frame = crate::image::composite_captured_region(
        &viewport.capture_regions(),
        width,
        height,
        &frozen_outputs,
    )?;
    crate::image::write_png(&args.output_path, &frame)?;
    crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode)?;
    println!("saved {}", args.output_path.display());
    Ok(())
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
    PrepareCapture { index: usize, ack: mpsc::SyncSender<()> },
    Frame(crate::image::Image),
    Error(String),
    Finished,
}

struct LongCaptureWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    receiver: mpsc::Receiver<CaptureMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LongCaptureWorker {
    fn start(region: crate::wayland::screencopy::CaptureOutputRegion) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel::<CaptureMessage>(4);
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut last: Option<std::time::Instant> = None;
            let mut index: usize = 0;
            while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                if index > 0 {
                    if let Some(prev) = last {
                        while prev.elapsed() < LONG_FRAME_INTERVAL {
                            if thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
                last = Some(std::time::Instant::now());
                let (ack_tx, ack_rx) = mpsc::sync_channel(0);
                if sender.send(CaptureMessage::PrepareCapture { index, ack: ack_tx }).is_err() {
                    return;
                }
                // Wait for main thread to render clean overlay
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
                match crate::wayland::screencopy::capture_region(region, false, index > 0) {
                    Ok(image) => {
                        if sender.send(CaptureMessage::Frame(image)).is_err() {
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
        Self { stop, receiver, thread: Some(thread) }
    }

    fn try_recv(&self) -> Result<CaptureMessage, TryRecvError> {
        self.receiver.try_recv()
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

    let regions = viewport.capture_regions();
    let mut worker = LongCaptureWorker::start(regions[0]);
    let mut stitcher = RawStitcher::new();
    let mut cancelled = false;
    let mut capture_finished = false;
    let mut capture_error: Option<String> = None;

    loop {
        let mut processed = false;
        for _ in 0..4 {
            match worker.try_recv() {
                Ok(CaptureMessage::PrepareCapture { ack, .. }) => {
                    let _ = selection_session.prepare_capture_clean();
                    let _ = ack.send(());
                    processed = true;
                }
                Ok(CaptureMessage::Frame(frame)) => {
                    let direction: Option<SearchDirection> = match selection_session.long_direction() {
                        Some(crate::wayland::selection::SearchDirection::Down) => Some(SearchDirection::Down),
                        Some(crate::wayland::selection::SearchDirection::Up) => Some(SearchDirection::Up),
                        Some(crate::wayland::selection::SearchDirection::Vertical) => Some(SearchDirection::Down),
                        None => None,
                    };
                    let compose = match ImageRgbView::new(&frame) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let analysis = match ImageRgbView::new(&frame) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let outcome = match stitcher.push_frame_views(compose, analysis, direction) {
                        Ok(o) => o,
                        Err(_) => continue,
                    };
                    if matches!(outcome, PushResult::Initialized | PushResult::Accepted { .. }) {
                        if let Some(s) = stitcher.stitched() {
                            if let Ok(image) = crate::stitch::image_from_stitched_frame(s) {
                                let snapshot = crate::wayland::selection::LongPreviewSnapshot {
                                    image,
                                    current_origin_x: s.current_origin_x,
                                    current_origin_y: s.current_origin_y,
                                    viewport_rect: viewport.rect,
                                    capture_width: viewport.capture_size().map(|(w, _)| w).unwrap_or(1),
                                    capture_height: viewport.capture_size().map(|(_, h)| h).unwrap_or(1),
                                };
                                let _ = selection_session.update_long_capture_preview(snapshot);
                            }
                        }
                    }
                    processed = true;
                }
                Ok(CaptureMessage::Error(err)) => {
                    capture_error = Some(err);
                    break;
                }
                Ok(CaptureMessage::Finished) => {
                    capture_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    capture_finished = true;
                    break;
                }
            }
        }
        if capture_error.is_some() || capture_finished {
            break;
        }
        if let Err(_) = selection_session.dispatch_poll(if processed { 0 } else { LONG_UI_POLL_INTERVAL.as_millis() as i32 }) {
            break;
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
