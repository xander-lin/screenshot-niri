//! Measures the stitcher's per-frame cost, for performance work.
//!
//! Two modes, both reporting the same numbers:
//!
//!     stitch-bench --dir DIR              # replay a captured corpus
//!     stitch-bench --synthetic WxHxN      # a deterministic scrolling page
//!
//! What it reports, per run:
//!
//! * `push` — the whole `push_frame_with_offset` call, i.e. what the UI thread
//!   pays per frame in a real long capture. Reported as mean/p50/p95/max.
//! * `preview` — the overlay's window image, which the UI thread also pays per
//!   accepted frame. Measured through the same reusing entry point the
//!   screenshot binary uses, so the allocation figure is the real one.
//! * `allocs` — the worst single push's allocation volume and count.
//!
//! The allocation counters are the reason this tool exists as a binary rather
//! than a `#[test]`: they need a global allocator wrapper, and the point is to
//! see the *sum* of what one frame allocates, which a test can only observe
//! through the same hook.
//!
//! The oracle is deliberately absent. `replay-frames` answers "is the placement
//! correct"; this answers "what does it cost", and the exhaustive search it
//! would have to run costs orders of magnitude more than the thing being
//! measured.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use screenshot_rs_rebuild::image::Image;
use screenshot_rs_rebuild::stitch::{
    image_from_stitched_window_reusing, CanvasWindow, FrameScreenOffset, ImageRgbView, PushResult,
    RawStitcher,
};
use screenshot_rs_rebuild::wayland::selection::LongDirection;
use wayland_client::protocol::wl_shm::Format;

// ---------------------------------------------------------------------------
// Allocation accounting
// ---------------------------------------------------------------------------

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are only
// updated on the side. Nothing here changes what is allocated or where.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn reset_allocation_counters() {
    ALLOCATED.store(0, Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

fn load_png(path: &Path) -> Result<Image, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size().ok_or("PNG reports no output size")?];
    let info = reader.next_frame(&mut buffer)?;
    let (width, height) = (info.width, info.height);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => return Err(format!("unexpected PNG colour type {other:?}").into()),
    };
    let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
    for pixel in 0..(width as usize) * (height as usize) {
        let source = pixel * channels;
        // The stitcher takes ARGB8888, whose memory order is B, G, R, X.
        data[pixel * 4] = buffer[source + 2];
        data[pixel * 4 + 1] = buffer[source + 1];
        data[pixel * 4 + 2] = buffer[source];
        data[pixel * 4 + 3] = 255;
    }
    Ok(Image {
        width,
        height,
        stride: width * 4,
        format: Format::Argb8888,
        data,
    })
}

fn corpus(dir: &Path) -> Result<Vec<Image>, Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "png"))
        .collect();
    paths.sort();
    if paths.len() < 2 {
        return Err(format!("need at least two frames in {}, found {}", dir.display(), paths.len()).into());
    }
    // Decoded up front so PNG decoding is never attributed to the stitcher.
    paths.iter().map(|path| load_png(path)).collect()
}

/// A deterministic scrolling page, cropped into frames.
///
/// The content has to be a genuine scroll of one page: a page that is not
/// shifted produces refusals, and the profile would then measure the rejection
/// path instead of the placement path.
fn synthetic_page(width: u32, height: u32, count: u32, step: u32) -> Vec<Image> {
    // Tall enough that no frame wraps back onto content it has already shown:
    // a wrapped page repeats, the stitcher correctly refuses the repeat, and the
    // profile would then measure the refusal path.
    let page_height = height + step * (count + 2);
    let mut page = vec![0u8; (width as usize) * (page_height as usize) * 4];
    for y in 0..page_height {
        for x in 0..width {
            let pixel = ((y as usize) * (width as usize) + x as usize) * 4;
            // Horizontal banding plus per-line marks: text-like, so it packs and
            // matches the way a real page does.
            //
            // `line` must not repeat within the page. A period that divides the
            // scroll step makes every frame identical and a period shorter than
            // the page makes distant rows look alike; either way the stitcher
            // correctly refuses, and the profile then measures the refusal path
            // instead of placement.
            let texture = ((x / 5 + y / 3) % 23) as u8;
            let line = ((y * 7) % 251) as u8;
            page[pixel] = 200u8.wrapping_sub(texture).wrapping_sub(line);
            page[pixel + 1] = 205u8.wrapping_sub(texture / 2).wrapping_sub(line / 3);
            page[pixel + 2] = 210u8.wrapping_sub(texture / 3).wrapping_sub(line / 7);
            page[pixel + 3] = 255;
        }
    }
    (0..count)
        .map(|index| {
            let top = index * step;
            let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
            for row in 0..height as usize {
                let source = ((top as usize + row) * width as usize) * 4;
                let destination = row * width as usize * 4;
                data[destination..destination + width as usize * 4]
                    .copy_from_slice(&page[source..source + width as usize * 4]);
            }
            Image {
                width,
                height,
                stride: width * 4,
                format: Format::Argb8888,
                data,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

struct Report {
    accepted: u32,
    push: Vec<f64>,
    preview: Vec<f64>,
    worst_allocated: usize,
    worst_allocations: usize,
    /// The worst frame after the first, which is the steady state a long
    /// capture actually runs in: the first preview has to allocate its buffer.
    steady_allocated: usize,
    steady_allocations: usize,
    frames_measured: usize,
}

fn measure(frames: &[Image], with_preview: bool) -> Result<Report, Box<dyn std::error::Error>> {
    let mut stitcher = RawStitcher::new();
    let first = ImageRgbView::new(&frames[0])?;
    stitcher.push_frame_with_offset(
        &first,
        &first,
        Some(LongDirection::Down),
        FrameScreenOffset { x: 0, y: 0 },
    )?;

    // Carried across frames exactly as `app` carries it.
    let mut preview_buffer: Vec<u8> = Vec::new();
    let mut report = Report {
        accepted: 0,
        push: Vec::with_capacity(frames.len()),
        preview: Vec::new(),
        worst_allocated: 0,
        worst_allocations: 0,
        steady_allocated: 0,
        steady_allocations: 0,
        frames_measured: 0,
    };
    for frame in &frames[1..] {
        let view = ImageRgbView::new(frame)?;
        reset_allocation_counters();
        let started = Instant::now();
        let outcome = stitcher.push_frame_with_offset(
            &view,
            &view,
            Some(LongDirection::Down),
            FrameScreenOffset { x: 0, y: 0 },
        )?;
        report.push.push(started.elapsed().as_secs_f64() * 1e3);
        let allocated = ALLOCATED.load(Ordering::Relaxed);
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        report.worst_allocated = report.worst_allocated.max(allocated);
        report.worst_allocations = report.worst_allocations.max(allocations);
        report.frames_measured += 1;
        // The first frames are where the recycled buffers are created. Steady
        // state is what a long capture actually runs in, so it is reported
        // separately from the peak.
        if report.frames_measured > 3 {
            report.steady_allocated = report.steady_allocated.max(allocated);
            report.steady_allocations = report.steady_allocations.max(allocations);
        }
        if !matches!(outcome, PushResult::Accepted { .. } | PushResult::Initialized) {
            continue;
        }
        report.accepted += 1;
        if !with_preview {
            continue;
        }
        let Some(stitched) = stitcher.stitched() else {
            continue;
        };
        let window = CanvasWindow::clipped(
            stitched,
            0,
            stitched.current_origin_y,
            frame.width as i32,
            frame.height as i32,
        )
        .unwrap_or_else(|| CanvasWindow::full(stitched));
        reset_allocation_counters();
        let started = Instant::now();
        let recycled = std::mem::take(&mut preview_buffer);
        let image = image_from_stitched_window_reusing(stitched, window, recycled)?;
        preview_buffer = image.data;
        report.preview.push(started.elapsed().as_secs_f64() * 1e3);
        let allocated = ALLOCATED.load(Ordering::Relaxed);
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        report.worst_allocated = report.worst_allocated.max(allocated);
        report.worst_allocations = report.worst_allocations.max(allocations);
        if report.frames_measured > 3 {
            report.steady_allocated = report.steady_allocated.max(allocated);
            report.steady_allocations = report.steady_allocations.max(allocations);
        }
    }
    Ok(report)
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * fraction) as usize]
}

fn summarize(label: &str, mut report: Report) {
    report.push.sort_by(f64::total_cmp);
    report.preview.sort_by(f64::total_cmp);
    let mean = |values: &[f64]| {
        if values.is_empty() {
            f64::NAN
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        }
    };
    println!("{label}");
    println!(
        "  push    mean {:6.2}  p50 {:6.2}  p95 {:6.2}  max {:6.2} ms   ({} frames, {} accepted)",
        mean(&report.push),
        percentile(&report.push, 0.5),
        percentile(&report.push, 0.95),
        percentile(&report.push, 1.0),
        report.push.len(),
        report.accepted,
    );
    println!(
        "  preview mean {:6.2}  p95 {:6.2}  max {:6.2} ms",
        mean(&report.preview),
        percentile(&report.preview, 0.95),
        percentile(&report.preview, 1.0),
    );
    println!(
        "  allocations: worst frame {:.2} MB / {}, steady state {:.2} MB / {}",
        report.worst_allocated as f64 / 1e6,
        report.worst_allocations,
        report.steady_allocated as f64 / 1e6,
        report.steady_allocations,
    );
    let total = mean(&report.push) + mean(&report.preview);
    println!(
        "  UI thread per frame {:.2} ms of a 33.3 ms budget ({:.0}%)",
        total,
        total / 33.3 * 100.0,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut directory: Option<PathBuf> = None;
    let mut synthetic: Option<(u32, u32, u32)> = None;
    let mut no_preview = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--dir" | "-d" => {
                directory = Some(PathBuf::from(arguments.next().ok_or("--dir needs a path")?))
            }
            "--synthetic" | "-s" => {
                let spec = arguments.next().ok_or("--synthetic needs WxHxN")?;
                let (size, count) = spec.rsplit_once('x').ok_or("expected WxHxN")?;
                let (width, height) = size.split_once('x').ok_or("expected WxHxN")?;
                synthetic = Some((
                    width.parse()?,
                    height.parse()?,
                    count.parse()?,
                ));
            }
            "--no-preview" => no_preview = true,
            "--help" | "-h" => {
                println!(
                    "stitch-bench --dir DIR [--no-preview]\n\
                     stitch-bench --synthetic WIDTHxHEIGHTxFRAMES [--no-preview]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    if let Some((width, height, count)) = synthetic {
        let frames = synthetic_page(width, height, count, 40);
        summarize(
            &format!("{width}x{height} synthetic, {count} frames"),
            measure(&frames, !no_preview)?,
        );
        return Ok(());
    }
    let directory = directory.ok_or("either --dir or --synthetic is required")?;
    let frames = corpus(&directory)?;
    summarize(&directory.display().to_string(), measure(&frames, !no_preview)?);
    Ok(())
}
