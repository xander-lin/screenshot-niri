use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::{Command, Mode};
use crate::stitch::{ImageRgbView, PushResult, RawStitcher, SearchDirection};

const LONG_UI_POLL_INTERVAL: Duration = Duration::from_millis(10);
const LONG_CAPTURE_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 120);

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
    let viewport = match crate::wayland::selection::select_viewport(&frozen_outputs)? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::LongModeRequested(viewport) => {
            return run_long_with_viewport(args, viewport);
        }
        crate::wayland::selection::SelectionOutcome::Cancelled => return Err("selection cancelled".into()),
    };
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

enum WorkerMsg {
    PrepareCapture,
    Frame(crate::image::Image),
}

struct LongCaptureWorker {
    stop: Arc<AtomicBool>,
    receiver: mpsc::Receiver<WorkerMsg>,
    ack_sender: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LongCaptureWorker {
    fn start(region: crate::wayland::screencopy::CaptureOutputRegion) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (msg_sender, receiver) = mpsc::sync_channel(2);
        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut last: Option<Instant> = None;
            while !thread_stop.load(Ordering::Relaxed) {
                if msg_sender.send(WorkerMsg::PrepareCapture).is_err() {
                    return;
                }
                if !wait_for_ack(&thread_stop, &ack_receiver) {
                    return;
                }
                if let Some(prev) = last {
                    while prev.elapsed() < LONG_CAPTURE_FRAME_INTERVAL {
                        if thread_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                last = Some(Instant::now());
                match crate::wayland::screencopy::capture_region(region, false) {
                    Ok(image) => {
                        if msg_sender.send(WorkerMsg::Frame(image)).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Self { stop, receiver, ack_sender, thread: Some(thread) }
    }

    fn try_recv(&self) -> Result<WorkerMsg, TryRecvError> {
        self.receiver.try_recv()
    }

    fn ack(&self) -> bool {
        self.ack_sender.try_send(()).is_ok()
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

fn wait_for_ack(stop: &AtomicBool, ack_receiver: &mpsc::Receiver<()>) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        match ack_receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(()) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
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

fn run_long_with_viewport(args: crate::cli::Args, viewport: crate::geometry::SelectedViewport) -> Result<(), Box<dyn Error>> {
    let mut selection_session = crate::wayland::selection::SelectionSession::new_long()?;
    selection_session.dispatch_poll(100)?;
    selection_session.dispatch_poll(0)?;
    run_long_capture(args, viewport, selection_session)
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
    selection_session.set_selected_viewport_passthrough(&viewport)?;

    let regions = viewport.capture_regions();
    let mut worker = LongCaptureWorker::start(regions[0]);
    let mut stitcher = RawStitcher::new();
    let capture_size = viewport.capture_size()?;
    let mut cancelled = false;

    loop {
        match worker.try_recv() {
            Ok(WorkerMsg::PrepareCapture) => {
                selection_session.render_capture_clean(&viewport)?;
                if !worker.ack() {
                    break;
                }
            }
            Ok(WorkerMsg::Frame(frame)) => {
                let direction: Option<SearchDirection> = match selection_session.long_direction() {
                    Some(crate::wayland::selection::SearchDirection::Down) => Some(SearchDirection::Down),
                    Some(crate::wayland::selection::SearchDirection::Up) => Some(SearchDirection::Up),
                    Some(crate::wayland::selection::SearchDirection::Vertical) => Some(SearchDirection::Down),
                    None => None,
                };
                let compose = ImageRgbView::new(&frame);
                let analysis = ImageRgbView::new(&frame);
                if let (Ok(compose), Ok(analysis)) = (compose, analysis) {
                    if let Ok(outcome) = stitcher.push_frame_views(compose, analysis, direction) {
                        if matches!(outcome, PushResult::Initialized | PushResult::Accepted { .. }) {
                            if let Some(s) = stitcher.stitched() {
                                if let Ok(image) = crate::stitch::image_from_stitched_frame(s) {
                                    let snapshot = crate::wayland::selection::LongPreviewSnapshot {
                                        image,
                                        current_origin_x: s.current_origin_x,
                                        current_origin_y: s.current_origin_y,
                                        viewport_rect: viewport.rect,
                                        capture_width: capture_size.0,
                                        capture_height: capture_size.1,
                                    };
                                    let _ = selection_session.update_long_capture_preview(snapshot);
                                }
                            }
                        }
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }
        selection_session.dispatch_poll(LONG_UI_POLL_INTERVAL.as_millis() as i32)?;
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
