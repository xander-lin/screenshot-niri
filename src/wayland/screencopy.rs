use std::error::Error;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::WlOutput,
    wl_registry::{self, WlRegistry},
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::image::Image;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOutputRegion {
    pub output_name: u32,
    pub region: Region,
    pub dst_x: u32,
    pub dst_y: u32,
}

#[derive(Debug)]
pub struct CapturedOutput {
    pub output_name: u32,
    pub image: Image,
}

/// The geometry a shm capture buffer was created for.
///
/// A session keeps its pool, buffer and mapping alive across frames and only
/// reallocates when this changes. A fresh 33 MB memfd per frame made the kernel
/// allocate and zero a new set of pages every time — 19.3 ms of first touch
/// measured at 4K, paid by whoever writes the capture — and freed the old ones
/// again, so at 30 fps the session churned a gigabyte per second of page
/// allocation for pixels it had already mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferSpec {
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
}

impl BufferSpec {
    fn matches(self, width: u32, height: u32, stride: u32, format: Format) -> bool {
        self.width == width && self.height == height && self.stride == stride && self.format == format
    }
}

struct ShmImage {
    spec: BufferSpec,
    size: usize,
    data: *mut u8,
    _fd: OwnedFd,
    pool: WlShmPool,
    buffer: WlBuffer,
}

impl Drop for ShmImage {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        if !self.data.is_null() && self.size != 0 {
            unsafe {
                libc::munmap(self.data.cast(), self.size);
            }
        }
    }
}

struct State {
    requested_output_name: Option<u32>,
    output: Option<WlOutput>,
    output_names: Vec<u32>,
    shm: Option<WlShm>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    frame: Option<ZwlrScreencopyFrameV1>,
    shm_image: Option<ShmImage>,
    y_inverted: bool,
    done: bool,
    failed: bool,
    image: Option<Image>,
    wait_for_damage: bool,
}

impl State {
    fn new(requested_output_name: Option<u32>) -> Self {
        Self {
            requested_output_name,
            output: None,
            output_names: Vec::new(),
            shm: None,
            screencopy: None,
            frame: None,
            shm_image: None,
            y_inverted: false,
            done: false,
            failed: false,
            image: None,
            wait_for_damage: false,
        }
    }
}

pub fn capture_outputs_with_cursor() -> Result<Vec<CapturedOutput>, Box<dyn Error>> {
    capture_outputs(true)
}

pub fn capture_outputs_without_cursor() -> Result<Vec<CapturedOutput>, Box<dyn Error>> {
    capture_outputs(false)
}

fn capture_outputs(overlay_cursor: bool) -> Result<Vec<CapturedOutput>, Box<dyn Error>> {
    let output_names = list_output_names()?;
    if output_names.is_empty() {
        return Err("compositor did not advertise any wl_output".into());
    }

    let mut outputs = Vec::with_capacity(output_names.len());
    for output_name in output_names {
        outputs.push(CapturedOutput {
            output_name,
            image: capture_output(output_name, overlay_cursor)?,
        });
    }
    Ok(outputs)
}

fn list_output_names() -> Result<Vec<u32>, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<State>();
    let mut state = State::new(None);
    conn.display().get_registry(&event_queue.handle(), ());
    event_queue.roundtrip(&mut state)?;
    Ok(state.output_names)
}

fn capture_output(output_name: u32, overlay_cursor: bool) -> Result<Image, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();
    let mut state = State::new(Some(output_name));

    conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    if state.shm.is_none() {
        return Err("compositor does not expose wl_shm required by zwlr_screencopy_manager_v1".into());
    }
    let screencopy = state.screencopy.as_ref().ok_or("compositor does not expose zwlr_screencopy_manager_v1")?;
    let output = state.output.as_ref().ok_or("requested wl_output was not advertised by the compositor")?;
    state.frame = Some(screencopy.capture_output(i32::from(overlay_cursor), output, &qh, ()));
    conn.flush()?;

    while !state.done && !state.failed {
        event_queue.blocking_dispatch(&mut state)?;
    }
    if let Some(frame) = state.frame.take() {
        frame.destroy();
    }
    if state.failed {
        return Err("screencopy failed".into());
    }
    state.image.take().ok_or_else(|| "screencopy completed without image".into())
}

#[allow(dead_code)]
pub fn capture_region(region: CaptureOutputRegion, overlay_cursor: bool, wait_for_damage: bool) -> Result<Image, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();
    let mut state = State::new(Some(region.output_name));
    state.wait_for_damage = wait_for_damage;

    conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    if state.shm.is_none() {
        return Err("compositor does not expose wl_shm".into());
    }
    let screencopy = state.screencopy.as_ref().ok_or("compositor does not expose zwlr_screencopy_manager_v1")?;
    let output = state.output.as_ref().ok_or("requested wl_output was not advertised by the compositor")?;
    state.frame = Some(screencopy.capture_output_region(
        i32::from(overlay_cursor),
        output,
        region.region.x,
        region.region.y,
        region.region.width,
        region.region.height,
        &qh,
        (),
    ));
    conn.flush()?;

    while !state.done && !state.failed {
        event_queue.blocking_dispatch(&mut state)?;
    }
    if let Some(frame) = state.frame.take() {
        frame.destroy();
    }
    if state.failed {
        return Err("screencopy region capture failed".into());
    }
    state.image.take().ok_or_else(|| "screencopy region completed without image".into())
}

// ── Persistent capture session ─────────────────────────────────────────

pub struct CaptureSession {
    conn: Connection,
    event_queue: wayland_client::EventQueue<State>,
    state: State,
}

impl CaptureSession {
    pub fn new(output_name: u32) -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()?;
        let mut event_queue = conn.new_event_queue::<State>();
        let qh = event_queue.handle();
        let mut state = State::new(Some(output_name));

        conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;
        if state.shm.is_none() {
            return Err("compositor does not expose wl_shm".into());
        }
        if state.screencopy.is_none() {
            return Err("compositor does not expose zwlr_screencopy_manager_v1".into());
        }
        if state.output.is_none() {
            return Err("requested wl_output was not advertised".into());
        }
        Ok(Self { conn, event_queue, state })
    }

    /// Capture an output-local logical rectangle. Returned dimensions are
    /// compositor-provided buffer pixels; `None` means the stop flag was set.
    pub fn capture_region_frame_without_cursor(
        &mut self,
        region: CaptureOutputRegion,
        wait_for_damage: bool,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<Image>, Box<dyn Error>> {
        self.state.image = None;
        self.state.wait_for_damage = wait_for_damage;
        self.state.done = false;
        self.state.failed = false;
        self.state.y_inverted = false;

        let qh = self.event_queue.handle();
        let screencopy = self.state.screencopy.as_ref().ok_or("missing screencopy")?;
        let output = self.state.output.as_ref().ok_or("missing output")?;
        self.state.frame = Some(screencopy.capture_output_region(
            0,
            output,
            region.region.x,
            region.region.y,
            region.region.width,
            region.region.height,
            &qh,
            (),
        ));
        self.conn.flush()?;

        while !self.state.done && !self.state.failed {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(frame) = self.state.frame.take() {
                    frame.destroy();
                }
                // The compositor may still be writing into the shared buffer
                // when the frame is destroyed, so the next frame must not
                // reuse it. (A cancelled frame ends the session in practice;
                // this keeps reuse safe rather than lucky.)
                self.state.shm_image = None;
                self.conn.flush()?;
                return Ok(None);
            }
            self.event_queue.dispatch_pending(&mut self.state)?;
            self.event_queue.flush()?;
            if self.state.done || self.state.failed {
                break;
            }
            let Some(guard) = self.event_queue.prepare_read() else { continue; };
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                revents: 0,
            };
            // Damage may never arrive on a static screen. Poll with a bounded
            // wait so Enter/Esc can stop and join the worker without another redraw.
            let ready = unsafe { libc::poll(&mut fd, 1, 20) };
            if ready > 0 {
                guard.read()?;
            } else {
                drop(guard);
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EINTR) {
                        return Err(error.into());
                    }
                }
            }
        }
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }
        if self.state.failed {
            return Err("screencopy region capture failed".into());
        }
        self.state.image.take().map(Some).ok_or_else(|| "screencopy region completed without image".into())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        let _ = self.conn.flush();
    }
}

// ── Dispatch impls ─────────────────────────────────────────────────────

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_shm" => {
                state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()))
            }
            "wl_output" => {
                let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                state.output_names.push(name);
                if state.requested_output_name == Some(name)
                    || (state.requested_output_name.is_none() && state.output.is_none())
                {
                    state.output = Some(output);
                }
            }
            "zwlr_screencopy_manager_v1" => {
                if version >= 3 {
                    state.screencopy = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, 3, qh, ()));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                let WEnum::Value(format) = format else {
                    state.failed = true;
                    state.done = true;
                    return;
                };
                if format != Format::Argb8888 && format != Format::Xrgb8888 {
                    state.failed = true;
                    state.done = true;
                    return;
                }
                let Some(shm) = state.shm.as_ref() else {
                    state.failed = true;
                    state.done = true;
                    return;
                };
                // Reuse the pool, buffer and mapping when the compositor asks
                // for the same geometry: `ready` for the previous copy already
                // arrived (a session only starts a frame after the previous one
                // completed), so the buffer is ours again.
                if state
                    .shm_image
                    .as_ref()
                    .is_some_and(|image| image.spec.matches(width, height, stride, format))
                {
                    return;
                }
                state.shm_image = None;
                match create_shm_image(shm, width, height, stride, format, qh) {
                    Ok(image) => state.shm_image = Some(image),
                    Err(_) => {
                        state.failed = true;
                        state.done = true;
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Some(image) = state.shm_image.as_ref() {
                    if state.wait_for_damage {
                        frame.copy_with_damage(&image.buffer);
                    } else {
                        frame.copy(&image.buffer);
                    }
                } else {
                    state.failed = true;
                    state.done = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {}
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(flags) = flags {
                    state.y_inverted = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                let Some(image) = state.shm_image.as_ref() else {
                    state.failed = true;
                    state.done = true;
                    return;
                };
                let bytes = unsafe { std::slice::from_raw_parts(image.data, image.size) };
                let spec = image.spec;
                state.image = Some(Image {
                    width: spec.width,
                    height: spec.height,
                    stride: spec.stride,
                    format: spec.format,
                    data: if state.y_inverted {
                        flip_rows(bytes, spec.height, spec.stride)
                    } else {
                        bytes.to_vec()
                    },
                });
                state.done = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.failed = true;
                state.done = true;
            }
            _ => {}
        }
    }
}

fn flip_rows(bytes: &[u8], height: u32, stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let height = height as usize;
    let mut flipped = vec![0; bytes.len()];
    for y in 0..height {
        let src = (height - 1 - y) * stride;
        let dst = y * stride;
        flipped[dst..dst + stride].copy_from_slice(&bytes[src..src + stride]);
    }
    flipped
}

fn create_shm_image(
    shm: &WlShm,
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
    qh: &QueueHandle<State>,
) -> Result<ShmImage, Box<dyn Error>> {
    if width == 0 || height == 0 || stride < width.saturating_mul(4) {
        return Err(format!("invalid screencopy buffer geometry {width}x{height} stride {stride}").into());
    }
    let size = (stride as usize).checked_mul(height as usize).ok_or("screencopy buffer size overflow")?;
    if size > i32::MAX as usize {
        return Err("screencopy buffer is too large for wl_shm".into());
    }
    let file = tempfile::tempfile()?;
    file.set_len(size as u64)?;
    let fd = OwnedFd::from(file);
    let data = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_fd().as_raw_fd(),
            0,
        )
    };
    if data == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().into());
    }
    // `wl_shm.create_pool` takes an i32 byte count. Above 2 GiB (~537
    // megapixels) the cast would wrap to a negative length, so refuse
    // rather than hand the compositor a bogus size.
    let pool_size = i32::try_from(size).map_err(|_| "shm buffer exceeds 2 GiB")?;
    let pool = shm.create_pool(fd.as_fd(), pool_size, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    let spec = BufferSpec { width, height, stride, format };
    Ok(ShmImage { spec, size, data: data.cast(), _fd: fd, pool, buffer })
}

impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: <WlShm as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrScreencopyManagerV1, _: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(State: ignore WlOutput);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlShmPool);


#[cfg(test)]
mod tests {
    use super::*;

    /// A capture session reuses its pool, buffer and mapping across frames.
    /// Reusing them for a different geometry would hand the compositor a
    /// buffer of the wrong size, so the match has to be exact.
    #[test]
    fn a_capture_buffer_is_reused_only_for_the_same_geometry() {
        let spec = BufferSpec { width: 100, height: 50, stride: 400, format: Format::Argb8888 };
        assert!(spec.matches(100, 50, 400, Format::Argb8888), "identical geometry");
        assert!(!spec.matches(100, 50, 400, Format::Xrgb8888), "a different format needs a new buffer");
        assert!(!spec.matches(101, 50, 400, Format::Argb8888), "a wider region");
        assert!(!spec.matches(100, 51, 400, Format::Argb8888), "a taller region");
        assert!(!spec.matches(100, 50, 404, Format::Argb8888), "a different stride");
    }
}
