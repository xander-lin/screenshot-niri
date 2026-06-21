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

struct ShmImage {
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
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
        }
    }
}

pub fn capture_outputs(overlay_cursor: bool) -> Result<Vec<CapturedOutput>, Box<dyn Error>> {
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

pub fn capture_region(region: CaptureOutputRegion, overlay_cursor: bool) -> Result<Image, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<State>();
    let qh = event_queue.handle();
    let mut state = State::new(Some(region.output_name));

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
            "wl_shm" => state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ())),
            "wl_output" => {
                let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                state.output_names.push(name);
                if state.requested_output_name == Some(name) || (state.requested_output_name.is_none() && state.output.is_none()) {
                    state.output = Some(output);
                }
            }
            "zwlr_screencopy_manager_v1" if version >= 3 => {
                state.screencopy = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, 3, qh, ()))
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
                let Some(shm) = state.shm.as_ref() else {
                    state.failed = true;
                    state.done = true;
                    return;
                };
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
                    frame.copy(&image.buffer);
                } else {
                    state.failed = true;
                    state.done = true;
                }
            }
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
                state.image = Some(Image {
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                    format: image.format,
                    data: if state.y_inverted { flip_rows(bytes, image.height, image.stride) } else { bytes.to_vec() },
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

fn create_shm_image(
    shm: &WlShm,
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
    qh: &QueueHandle<State>,
) -> Result<ShmImage, Box<dyn Error>> {
    if format != Format::Argb8888 && format != Format::Xrgb8888 {
        return Err(format!("unsupported screencopy shm format: {format:?}").into());
    }
    if width == 0 || height == 0 || stride < width.saturating_mul(4) {
        return Err(format!("invalid screencopy buffer geometry {width}x{height} stride {stride}").into());
    }
    let size = (stride as usize).checked_mul(height as usize).ok_or("screencopy buffer size overflow")?;
    if size > i32::MAX as usize || width > i32::MAX as u32 || height > i32::MAX as u32 || stride > i32::MAX as u32 {
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
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    Ok(ShmImage { width, height, stride, format, size, data: data.cast(), _fd: fd, pool, buffer })
}

fn flip_rows(bytes: &[u8], height: u32, stride: u32) -> Vec<u8> {
    let mut flipped = vec![0; bytes.len()];
    for y in 0..height as usize {
        let src = (height as usize - 1 - y) * stride as usize;
        let dst = y * stride as usize;
        flipped[dst..dst + stride as usize].copy_from_slice(&bytes[src..src + stride as usize]);
    }
    flipped
}

impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: <WlShm as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlOutput);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlShmPool);
