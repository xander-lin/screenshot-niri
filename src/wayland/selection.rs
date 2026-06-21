use std::error::Error;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_region::WlRegion,
    wl_registry::{self, WlRegistry},
    wl_seat::{self, Capability, WlSeat},
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::geometry::{LogicalRect, OutputInfo, SelectedViewport};
use crate::image::Image;
use crate::wayland::screencopy::CapturedOutput;

const BTN_LEFT: u32 = 0x110;
const KEY_ESC: u32 = 1;
const KEY_L: u32 = 38;
const KEY_ENTER: u32 = 28;
const KEY_SPACE: u32 = 57;
const KEY_DOWN: u32 = 108;
const KEY_UP: u32 = 103;
const OVERLAY_BUFFER_COUNT: usize = 2;
const SELECTION_BORDER_WIDTH: i32 = 3;
const OVERLAY_OUTSIDE_MASK_BGRA: [u8; 4] = [0, 0, 0, 110];
const OVERLAY_SELECTED_TRANSPARENT_BGRA: [u8; 4] = [0, 0, 0, 0];
const OVERLAY_BORDER_WHITE_BGRA: [u8; 4] = [255, 255, 255, 255];
const OVERLAY_BORDER_DARK_BGRA: [u8; 4] = [0, 0, 0, 220];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    Selected(SelectedViewport),
    LongModeRequested(SelectedViewport),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragState {
    Idle,
    Dragging { origin_x: i32, origin_y: i32, current_x: i32, current_y: i32 },
    Finished(LogicalRect),
    Cancelled,
}

impl DragState {
    fn begin(&mut self, x: i32, y: i32) {
        *self = Self::Dragging { origin_x: x, origin_y: y, current_x: x, current_y: y };
    }

    fn update(&mut self, x: i32, y: i32) -> bool {
        if let Self::Dragging { origin_x, origin_y, current_x, current_y } = *self {
            if current_x == x && current_y == y {
                return false;
            }
            *self = Self::Dragging { origin_x, origin_y, current_x: x, current_y: y };
            return true;
        }
        false
    }

    fn finish(&mut self, x: i32, y: i32) {
        if let Self::Dragging { origin_x, origin_y, .. } = *self {
            *self = Self::Finished(LogicalRect::from_points(origin_x, origin_y, x, y));
        }
    }

    fn current_rect(self) -> Option<LogicalRect> {
        match self {
            Self::Dragging { origin_x, origin_y, current_x, current_y } => Some(LogicalRect::from_points(origin_x, origin_y, current_x, current_y)),
            Self::Finished(rect) => Some(rect),
            Self::Idle | Self::Cancelled => None,
        }
    }

    fn selected_rect(self) -> Option<LogicalRect> {
        match self {
            Self::Finished(rect) => Some(rect),
            _ => None,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Finished(_) | Self::Cancelled)
    }

    fn is_finished(self) -> bool {
        matches!(self, Self::Finished(_))
    }
}

struct OverlayBuffer {
    size: usize,
    data: *mut u8,
    _fd: OwnedFd,
    pool: WlShmPool,
    buffer: WlBuffer,
    available: bool,
    initialized: bool,
    last_selected: Option<LogicalRect>,
}

impl Drop for OverlayBuffer {
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

#[derive(Debug, Clone, Copy)]
struct BufferId {
    output_name: u32,
    index: usize,
}

struct OutputRuntime {
    info: OutputInfo,
    output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
}

struct OverlaySurface {
    output_name: u32,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffers: Vec<OverlayBuffer>,
    render_cache: Option<OverlayRenderCache>,
    width: i32,
    height: i32,
}

struct OverlayRenderCache {
    width: i32,
    height: i32,
    base: Vec<u8>,
    dimmed: Vec<u8>,
}

struct FrozenOutput {
    info: OutputInfo,
    width: i32,
    height: i32,
    stride: i32,
    data: Vec<u8>,
}

impl Drop for OverlaySurface {
    fn drop(&mut self) {
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

struct UiState {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    outputs: Vec<OutputRuntime>,
    frozen_outputs: Vec<FrozenOutput>,
    overlays: Vec<OverlaySurface>,
    pointer_output_name: Option<u32>,
    pointer_x: i32,
    pointer_y: i32,
    drag: DragState,
    long_requested: bool,
    long_finish_requested: bool,
    long_direction: Option<SearchDirection>,
    long_preview: Option<LongPreviewSnapshot>,
}

impl UiState {
    fn output_infos(&self) -> Vec<OutputInfo> {
        self.outputs.iter().map(|output| output.info).collect()
    }

    fn selected_viewport(&self) -> Result<SelectedViewport, Box<dyn Error>> {
        let rect = self.drag.selected_rect().ok_or("selection finished without a rectangle")?;
        Ok(SelectedViewport::from_outputs(rect, &self.output_infos())?)
    }

    fn pointer_global_position(&self) -> Option<(i32, i32)> {
        let output_name = self.pointer_output_name?;
        let output = self.outputs.iter().find(|output| output.info.global_name == output_name)?;
        Some((output.info.logical.x + self.pointer_x, output.info.logical.y + self.pointer_y))
    }
}

pub fn select_viewport(frozen_outputs: &[CapturedOutput]) -> Result<SelectionOutcome, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<UiState>();
    let qh = event_queue.handle();
    let mut state = UiState {
        compositor: None,
        shm: None,
        layer_shell: None,
        xdg_output_manager: None,
        seat: None,
        pointer: None,
        keyboard: None,
        outputs: Vec::new(),
        frozen_outputs: frozen_outputs_from_captures(frozen_outputs)?,
        overlays: Vec::new(),
        pointer_output_name: None,
        pointer_x: 0,
        pointer_y: 0,
        drag: DragState::Idle,
            long_requested: false,
        long_finish_requested: false,
        long_direction: None,
            long_preview: None,
    };

    conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    bind_xdg_outputs(&mut state, &qh);
    event_queue.roundtrip(&mut state)?;
    if state.compositor.is_none() {
        return Err("compositor does not expose wl_compositor required by zwlr_layer_shell_v1".into());
    }
    if state.shm.is_none() {
        return Err("compositor does not expose wl_shm required by zwlr_layer_shell_v1 overlay buffers".into());
    }
    if state.layer_shell.is_none() {
        return Err("compositor does not expose zwlr_layer_shell_v1 required for selection overlay".into());
    }
    if state.outputs.is_empty() {
        return Err("compositor did not advertise any wl_output".into());
    }
    if state.pointer.is_none() {
        return Err("no wl_pointer from wl_seat is available for selection".into());
    }
    validate_frozen_outputs(&state.outputs, &state.frozen_outputs)?;
    create_overlays(&mut state, &qh)?;
    conn.flush()?;

    while !state.drag.is_terminal() {
        event_queue.blocking_dispatch(&mut state)?;
    }
    if state.drag == DragState::Cancelled {
        Ok(SelectionOutcome::Cancelled)
    } else if state.long_requested {
        Ok(SelectionOutcome::LongModeRequested(state.selected_viewport()?))
    } else {
        Ok(SelectionOutcome::Selected(state.selected_viewport()?))
    }
}

// ── Long screenshot session ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Down,
    Up,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongSessionStatus {
    Running,
    FinishRequested,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct LongPreviewSnapshot {
    pub image: Image,
    pub current_origin_x: i32,
    pub current_origin_y: i32,
    pub viewport_rect: LogicalRect,
    pub capture_width: u32,
    pub capture_height: u32,
}

pub struct SelectionSession {
    conn: Connection,
    event_queue: wayland_client::EventQueue<UiState>,
    state: UiState,
}

impl SelectionSession {
    pub fn new_long() -> Result<Self, Box<dyn Error>> {
        Self::new_internal(None)
    }

    pub fn with_frozen(frozen_outputs: &[CapturedOutput]) -> Result<Self, Box<dyn Error>> {
        Self::new_internal(Some(frozen_outputs))
    }

    fn new_internal(frozen_outputs: Option<&[CapturedOutput]>) -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()?;
        let mut event_queue = conn.new_event_queue::<UiState>();
        let qh = event_queue.handle();
        let frozen_outputs = match frozen_outputs {
            Some(captures) => frozen_outputs_from_captures(captures)?,
            None => Vec::new(),
        };
        let mut state = UiState {
            compositor: None,
            shm: None,
            layer_shell: None,
            xdg_output_manager: None,
            seat: None,
            pointer: None,
            keyboard: None,
            outputs: Vec::new(),
            frozen_outputs,
            overlays: Vec::new(),
            pointer_output_name: None,
            pointer_x: 0,
            pointer_y: 0,
            drag: DragState::Idle,
            long_requested: false,
            long_finish_requested: false,
            long_direction: None,
            long_preview: None,
        };

        conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;
        bind_xdg_outputs(&mut state, &qh);
        event_queue.roundtrip(&mut state)?;
        if state.compositor.is_none() {
            return Err("compositor does not expose wl_compositor required by zwlr_layer_shell_v1".into());
        }
        if state.shm.is_none() {
            return Err("compositor does not expose wl_shm required by zwlr_layer_shell_v1 overlay buffers".into());
        }
        if state.layer_shell.is_none() {
            return Err("compositor does not expose zwlr_layer_shell_v1 required for selection overlay".into());
        }
        if state.outputs.is_empty() {
            return Err("compositor did not advertise any wl_output".into());
        }
        if state.pointer.is_none() {
            return Err("no wl_pointer from wl_seat is available for selection".into());
        }
        create_live_overlays(&mut state, &qh)?;
        conn.flush()?;
        Ok(Self { conn, event_queue, state })
    }

    pub fn run_selection(&mut self) -> Result<SelectionOutcome, Box<dyn Error>> {
        while !self.state.drag.is_terminal() {
            self.event_queue.blocking_dispatch(&mut self.state)?;
        }
        self.event_queue.dispatch_pending(&mut self.state)?;
        if self.state.drag == DragState::Cancelled {
            Ok(SelectionOutcome::Cancelled)
        } else if self.state.long_requested {
            Ok(SelectionOutcome::LongModeRequested(self.state.selected_viewport()?))
        } else {
            Ok(SelectionOutcome::Selected(self.state.selected_viewport()?))
        }
    }

    pub fn set_selected_viewport_passthrough(
        &mut self,
        viewport: &SelectedViewport,
    ) -> Result<(), Box<dyn Error>> {
        let qh = self.event_queue.handle();
        self.state.drag = DragState::Finished(viewport.rect);
        self.state.long_requested = false;
        self.state.long_finish_requested = false;
        self.state.long_direction = None;
        self.state.long_preview = None;
        for overlay in &mut self.state.overlays {
            overlay.render_cache = None;
        }
        render_overlays_full_dim(&mut self.state);
        set_overlay_keyboard_exclusive(&mut self.state);
        set_overlay_pointer_passthrough(&mut self.state, &qh)?;
        self.event_queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn long_status(&self) -> LongSessionStatus {
        if self.state.drag == DragState::Cancelled {
            LongSessionStatus::Cancelled
        } else if self.state.long_finish_requested {
            LongSessionStatus::FinishRequested
        } else {
            LongSessionStatus::Running
        }
    }

    pub fn long_direction(&self) -> Option<SearchDirection> {
        self.state.long_direction
    }

    pub fn update_long_capture_preview(&mut self, snapshot: LongPreviewSnapshot) -> Result<(), Box<dyn Error>> {
        self.state.long_preview = Some(snapshot);
        render_overlays_full_dim(&mut self.state);
        self.event_queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn dispatch_poll(&mut self, timeout_ms: i32) -> Result<(), Box<dyn Error>> {
        if self.event_queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(());
        }
        self.event_queue.flush()?;
        let Some(guard) = self.event_queue.prepare_read() else {
            return Ok(());
        };
        let fd = guard.connection_fd().as_raw_fd();
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLERR,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
            if ready > 0 {
                break;
            }
            if ready == 0 {
                drop(guard);
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                drop(guard);
                return Err(err.into());
            }
        }
        guard.read()?;
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn dispatch_blocking(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.blocking_dispatch(&mut self.state)?;
        Ok(())
    }

    pub fn dispatch_pending(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn wait_configured(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.blocking_dispatch(&mut self.state)?;
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn close(self) -> Result<(), Box<dyn Error>> {
        drop(self.state);
        drop(self.event_queue);
        let _ = self.conn.flush();
        Ok(())
    }
}

fn create_live_overlays(state: &mut UiState, qh: &QueueHandle<UiState>) -> Result<(), Box<dyn Error>> {
    let compositor = state.compositor.as_ref().ok_or("compositor does not expose wl_compositor")?;
    let layer_shell = state.layer_shell.as_ref().ok_or("compositor does not expose zwlr_layer_shell_v1")?;
    for output in &state.outputs {
        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&output.output),
            Layer::Overlay,
            "screenshot-selection".into(),
            qh,
            output.info.global_name,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Right | Anchor::Bottom | Anchor::Left);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer_surface.set_size(0, 0);
        surface.commit();
        state.overlays.push(OverlaySurface {
            output_name: output.info.global_name,
            surface,
            layer_surface,
            buffers: Vec::new(),
            render_cache: None,
            width: output.info.logical.width.max(1),
            height: output.info.logical.height.max(1),
        });
    }
    Ok(())
}

fn render_overlays_full_dim(state: &mut UiState) {
    for overlay in &mut state.overlays {
        let Some(buffer) = overlay.buffers.iter_mut().find(|buffer| buffer.available) else {
            continue;
        };
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.data, buffer.size) };
        let stride = overlay.width as usize * 4;
        for y in 0..overlay.height as usize {
            for x in 0..overlay.width as usize {
                let offset = y * stride + x * 4;
                data[offset..offset + 4].copy_from_slice(&OVERLAY_OUTSIDE_MASK_BGRA);
            }
        }
        let rect = state.drag.current_rect();
        let mut output_logical = LogicalRect { x: 0, y: 0, width: overlay.width, height: overlay.height };
        let mut selection_local: Option<LogicalRect> = None;
        if let Some(rect) = rect {
            if let Some(output) = state.outputs.iter().find(|o| o.info.global_name == overlay.output_name) {
                output_logical = output.info.logical;
                let local = selected_local_intersection_for_buffer(output.info.logical, rect, overlay.width, overlay.height);
                if let Some(ref local) = local {
                    for y in local.y..local.y + local.height {
                        for x in local.x..local.x + local.width {
                            let offset = y as usize * stride + x as usize * 4;
                            data[offset + 3] = 0;
                        }
                    }
                    draw_selection_border(data, overlay.width, overlay.height, local.x, local.y, local.width, local.height);
                }
                selection_local = local;
            }
        }
        if let Some(ref preview) = state.long_preview {
            draw_preview_on_overlay(data, overlay.width, overlay.height, preview, output_logical, selection_local);
        }
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay.surface.damage_buffer(0, 0, overlay.width, overlay.height);
        overlay.surface.commit();
        buffer.available = false;
    }
}

fn draw_selection_border(data: &mut [u8], _width: i32, _height: i32, x: i32, y: i32, w: i32, h: i32) {
    let stride = _width as usize * 4;
    let x = x.max(0) as usize;
    let y = y.max(0) as usize;
    let w = w as usize;
    let h = h as usize;
    let right = (x + w).min(_width as usize);
    let bottom = (y + h).min(_height as usize);
    let border = OVERLAY_BORDER_WHITE_BGRA;
    if y > 0 && x < right {
        let py = y - 1;
        for px in x..right {
            let offset = py * stride + px * 4;
            data[offset..offset + 4].copy_from_slice(&border);
        }
    }
    if bottom < _height as usize && x < right {
        let py = bottom;
        for px in x..right {
            let offset = py * stride + px * 4;
            data[offset..offset + 4].copy_from_slice(&border);
        }
    }
    if x > 0 && y < bottom {
        let px = x - 1;
        for py in y..bottom {
            let offset = py * stride + px * 4;
            data[offset..offset + 4].copy_from_slice(&border);
        }
    }
    if right < _width as usize && y < bottom {
        let px = right;
        for py in y..bottom {
            let offset = py * stride + px * 4;
            data[offset..offset + 4].copy_from_slice(&border);
        }
    }
}

fn set_overlay_keyboard_exclusive(state: &mut UiState) {
    for overlay in &mut state.overlays {
        overlay.layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        overlay.surface.commit();
    }
}

fn set_overlay_pointer_passthrough(
    state: &mut UiState,
    qh: &QueueHandle<UiState>,
) -> Result<(), Box<dyn Error>> {
    let compositor = state.compositor.as_ref().ok_or("compositor does not expose wl_compositor")?;
    for overlay in &mut state.overlays {
        let region = compositor.create_region(qh, ());
        overlay.surface.set_input_region(Some(&region));
        overlay.surface.commit();
        region.destroy();
    }
    Ok(())
}

fn bind_xdg_outputs(state: &mut UiState, qh: &QueueHandle<UiState>) {
    let Some(manager) = state.xdg_output_manager.as_ref() else {
        return;
    };
    for output in &mut state.outputs {
        output.xdg_output = Some(manager.get_xdg_output(&output.output, qh, output.info.global_name));
    }
}

fn create_overlays(state: &mut UiState, qh: &QueueHandle<UiState>) -> Result<(), Box<dyn Error>> {
    let compositor = state.compositor.as_ref().ok_or("compositor does not expose wl_compositor")?;
    let layer_shell = state.layer_shell.as_ref().ok_or("compositor does not expose zwlr_layer_shell_v1")?;
    for output in &state.outputs {
        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&output.output),
            Layer::Overlay,
            "screenshot-selection".into(),
            qh,
            output.info.global_name,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Right | Anchor::Bottom | Anchor::Left);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer_surface.set_size(0, 0);
        surface.commit();
        state.overlays.push(OverlaySurface {
            output_name: output.info.global_name,
            surface,
            layer_surface,
            buffers: Vec::new(),
            render_cache: None,
            width: output.info.logical.width.max(1),
            height: output.info.logical.height.max(1),
        });
    }
    Ok(())
}

fn render_overlays(state: &mut UiState) {
    let rect = state.drag.current_rect();
    for overlay in &mut state.overlays {
        let Some(buffer) = overlay.buffers.iter_mut().find(|buffer| buffer.available) else {
            continue;
        };
        let Some(output) = state.outputs.iter().find(|output| output.info.global_name == overlay.output_name) else {
            continue;
        };
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.data, buffer.size) };
        let current_selected = rect.and_then(|selected| selected_local_intersection_for_buffer(output.info.logical, selected, overlay.width, overlay.height));
        let damage = if buffer.initialized {
            dirty_selected_region(buffer.last_selected, current_selected, overlay.width, overlay.height)
        } else {
            Some(LogicalRect { x: 0, y: 0, width: overlay.width, height: overlay.height })
        };
        let Some(damage) = damage else {
            continue;
        };
        let Some(render_cache) = overlay.render_cache.as_ref() else {
            draw_overlay_dim_with_selection(data, overlay.width, overlay.height, rect);
            buffer.initialized = true;
            overlay.surface.attach(Some(&buffer.buffer), 0, 0);
            overlay.surface.damage_buffer(0, 0, overlay.width, overlay.height);
            overlay.surface.commit();
            buffer.available = false;
            continue;
        };
        if buffer.initialized {
            draw_overlay_region(data, render_cache, output.info, rect, damage);
        } else {
            draw_overlay(data, render_cache, output.info, rect);
            buffer.initialized = true;
        }
        buffer.last_selected = current_selected;
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay.surface.damage_buffer(damage.x, damage.y, damage.width, damage.height);
        overlay.surface.commit();
        buffer.available = false;
    }
}

fn frozen_outputs_from_captures(captures: &[CapturedOutput]) -> Result<Vec<FrozenOutput>, Box<dyn Error>> {
    let mut frozen_outputs = Vec::with_capacity(captures.len());
    for capture in captures {
        let image = &capture.image;
        if !matches!(image.format, Format::Xrgb8888 | Format::Argb8888) {
            return Err(format!("unsupported frozen output format for output {}: {:?}", capture.output_name, image.format).into());
        }
        if image.width <= 0 || image.height <= 0 {
            return Err(format!("frozen output {} dimensions must be positive", capture.output_name).into());
        }
        let minimum_stride = image.width.checked_mul(4).ok_or("frozen output stride overflow")?;
        if image.stride < minimum_stride {
            return Err(format!("frozen output {} stride is smaller than width * 4", capture.output_name).into());
        }
        let minimum_len = usize::try_from(image.stride)?.checked_mul(usize::try_from(image.height)?).ok_or("frozen output data length overflow")?;
        if image.data.len() < minimum_len {
            return Err(format!("frozen output {} data is shorter than stride * height", capture.output_name).into());
        }
        let width = i32::try_from(image.width)?;
        let height = i32::try_from(image.height)?;
        let stride = i32::try_from(image.stride)?;
        frozen_outputs.push(FrozenOutput {
            info: OutputInfo {
                global_name: capture.output_name,
                logical: LogicalRect { x: 0, y: 0, width, height },
                scale: 1,
            },
            width,
            height,
            stride,
            data: image.data.clone(),
        });
    }
    Ok(frozen_outputs)
}

fn validate_frozen_outputs(outputs: &[OutputRuntime], frozen_outputs: &[FrozenOutput]) -> Result<(), Box<dyn Error>> {
    for output in outputs {
        if !frozen_outputs.iter().any(|frozen| frozen.info.global_name == output.info.global_name) {
            return Err(format!("missing frozen screenshot for advertised output {}", output.info.global_name).into());
        }
    }
    Ok(())
}

fn frozen_pixel(frozen: &FrozenOutput, output: OutputInfo, overlay_width: i32, overlay_height: i32, local_x: i32, local_y: i32) -> [u8; 4] {
    if frozen.width <= 0 || frozen.height <= 0 || frozen.stride <= 0 || overlay_width <= 0 || overlay_height <= 0 {
        return [0, 0, 0, 255];
    }

    let scale = output.scale.max(1);
    let exact_scaled = frozen.width == output.logical.width.saturating_mul(scale) && frozen.height == output.logical.height.saturating_mul(scale);
    let (source_x, source_y) = if exact_scaled {
        (local_x.saturating_mul(scale), local_y.saturating_mul(scale))
    } else {
        (
            ((local_x as i64 * frozen.width as i64) / overlay_width as i64) as i32,
            ((local_y as i64 * frozen.height as i64) / overlay_height as i64) as i32,
        )
    };
    let source_x = source_x.clamp(0, frozen.width - 1);
    let source_y = source_y.clamp(0, frozen.height - 1);
    let Ok(offset) = usize::try_from(source_y)
        .and_then(|y| usize::try_from(frozen.stride).map(|stride| y * stride))
        .and_then(|row| usize::try_from(source_x).map(|x| row + x * 4))
    else {
        return [0, 0, 0, 255];
    };
    let Some(pixel) = frozen.data.get(offset..offset + 4) else {
        return [0, 0, 0, 255];
    };
    [pixel[0], pixel[1], pixel[2], 255]
}

fn build_overlay_render_cache(width: i32, height: i32, output: OutputInfo, frozen: &FrozenOutput) -> OverlayRenderCache {
    let buffer_width = width.max(0) as usize;
    let buffer_height = height.max(0) as usize;
    let len = buffer_width.saturating_mul(buffer_height).saturating_mul(4);
    let mut base = vec![0; len];
    let mut dimmed = vec![0; len];
    for local_y in 0..buffer_height {
        for local_x in 0..buffer_width {
            let offset = (local_y * buffer_width + local_x) * 4;
            let pixel = frozen_pixel(frozen, output, width, height, local_x as i32, local_y as i32);
            base[offset..offset + 4].copy_from_slice(&pixel);
            dimmed[offset..offset + 4].copy_from_slice(&composited_overlay_pixel(pixel, output.logical.x + local_x as i32, output.logical.y + local_y as i32, None));
        }
    }
    OverlayRenderCache { width, height, base, dimmed }
}

fn composited_overlay_pixel(base: [u8; 4], global_x: i32, global_y: i32, selected: Option<LogicalRect>) -> [u8; 4] {
    let overlay = overlay_pixel(global_x, global_y, selected);
    let alpha = overlay[3] as u32;
    if alpha == 0 {
        return base;
    }
    if alpha == 255 {
        return [overlay[0], overlay[1], overlay[2], 255];
    }
    let inverse_alpha = 255 - alpha;
    [
        (((overlay[0] as u32 * alpha) + (base[0] as u32 * inverse_alpha) + 127) / 255) as u8,
        (((overlay[1] as u32 * alpha) + (base[1] as u32 * inverse_alpha) + 127) / 255) as u8,
        (((overlay[2] as u32 * alpha) + (base[2] as u32 * inverse_alpha) + 127) / 255) as u8,
        255,
    ]
}

fn draw_overlay(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, selected: Option<LogicalRect>) {
    draw_overlay_region(data, cache, output, selected, LogicalRect { x: 0, y: 0, width: cache.width, height: cache.height });
}

fn draw_overlay_region(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, selected: Option<LogicalRect>, dirty: LogicalRect) {
    let width = cache.width.max(0) as usize;
    for local_y in dirty.y as usize..dirty.bottom() as usize {
        for local_x in dirty.x as usize..dirty.right() as usize {
            let global_x = output.logical.x + local_x as i32;
            let global_y = output.logical.y + local_y as i32;
            let offset = (local_y * width + local_x) * 4;
            let source = match overlay_pixel(global_x, global_y, selected) {
                OVERLAY_SELECTED_TRANSPARENT_BGRA => &cache.base,
                OVERLAY_OUTSIDE_MASK_BGRA => &cache.dimmed,
                border => {
                    data[offset..offset + 4].copy_from_slice(&border);
                    continue;
                }
            };
            data[offset..offset + 4].copy_from_slice(&source[offset..offset + 4]);
        }
    }
}

fn overlay_pixel(global_x: i32, global_y: i32, selected: Option<LogicalRect>) -> [u8; 4] {
    let Some(rect) = selected else {
        return OVERLAY_OUTSIDE_MASK_BGRA;
    };
    if global_x < rect.x || global_x >= rect.right() || global_y < rect.y || global_y >= rect.bottom() {
        return OVERLAY_OUTSIDE_MASK_BGRA;
    }

    selected_overlay_pixel(global_x, global_y, rect)
}

fn draw_overlay_dim_with_selection(data: &mut [u8], width: i32, height: i32, selected: Option<LogicalRect>) {
    let stride = width.max(0) as usize * 4;
    for y in 0..height.max(0) as usize {
        for x in 0..width.max(0) as usize {
            let offset = y * stride + x * 4;
            data[offset..offset + 4].copy_from_slice(&OVERLAY_OUTSIDE_MASK_BGRA);
        }
    }
    if let Some(sel) = selected {
        let local = selected_local_intersection_for_buffer(
            LogicalRect { x: 0, y: 0, width, height },
            sel,
            width, height,
        );
        if let Some(local) = local {
            for y in local.y..local.y + local.height {
                for x in local.x..local.x + local.width {
                    let offset = (y as usize) * stride + (x as usize) * 4;
                    data[offset..offset + 4].copy_from_slice(&OVERLAY_SELECTED_TRANSPARENT_BGRA);
                }
            }
            draw_selection_border(data, width, height, local.x, local.y, local.width, local.height);
        }
    }
}

fn selected_local_intersection(output: LogicalRect, selected: LogicalRect) -> Option<LogicalRect> {
    selected.intersection(output).map(|intersection| LogicalRect {
        x: intersection.x - output.x,
        y: intersection.y - output.y,
        width: intersection.width,
        height: intersection.height,
    })
}

fn selected_local_intersection_for_buffer(output: LogicalRect, selected: LogicalRect, width: i32, height: i32) -> Option<LogicalRect> {
    let buffer = LogicalRect { x: 0, y: 0, width, height };
    selected_local_intersection(output, selected).and_then(|local| local.intersection(buffer))
}

fn draw_preview_on_overlay(data: &mut [u8], overlay_w: i32, overlay_h: i32, preview: &LongPreviewSnapshot, output_logical: LogicalRect, selection_local: Option<LogicalRect>) {
    let pw = preview.image.width;
    let ph = preview.image.height;
    if pw == 0 || ph == 0 || overlay_w <= 0 || overlay_h <= 0 {
        return;
    }
    let vp = preview.viewport_rect;
    let cw = preview.capture_width.max(1);
    let ch = preview.capture_height.max(1);
    let origin_x = preview.current_origin_x;
    let origin_y = preview.current_origin_y;
    let buf_stride = overlay_w as usize * 4;
    for oy in 0..overlay_h {
        let global_y = output_logical.y + oy;
        for ox in 0..overlay_w {
            let global_x = output_logical.x + ox;
            if let Some(ref sel) = selection_local {
                if ox >= sel.x && ox < sel.x + sel.width && oy >= sel.y && oy < sel.y + sel.height {
                    continue;
                }
            }
            let sx = origin_x + (global_x - vp.x) * cw as i32 / vp.width.max(1);
            let sy = origin_y + (global_y - vp.y) * ch as i32 / vp.height.max(1);
            if sx >= 0 && (sx as u32) < pw && sy >= 0 && (sy as u32) < ph {
                let src_off = (sy as u32 * preview.image.stride + sx as u32 * 4) as usize;
                if src_off + 3 < preview.image.data.len() {
                    let dst_off = (oy as usize) * buf_stride + (ox as usize) * 4;
                    data[dst_off] = preview.image.data[src_off];
                    data[dst_off + 1] = preview.image.data[src_off + 1];
                    data[dst_off + 2] = preview.image.data[src_off + 2];
                    data[dst_off + 3] = 255;
                }
            }
        }
    }
}

fn dirty_selected_region(previous: Option<LogicalRect>, current: Option<LogicalRect>, width: i32, height: i32) -> Option<LogicalRect> {
    let dirty = match (previous, current) {
        (Some(previous), Some(current)) => rect_union(previous, current),
        (Some(previous), None) => previous,
        (None, Some(current)) => current,
        (None, None) => return None,
    };
    dirty.intersection(LogicalRect { x: 0, y: 0, width, height })
}

fn rect_union(a: LogicalRect, b: LogicalRect) -> LogicalRect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = a.right().max(b.right());
    let bottom = a.bottom().max(b.bottom());
    LogicalRect { x, y, width: right - x, height: bottom - y }
}

fn selected_overlay_pixel(global_x: i32, global_y: i32, rect: LogicalRect) -> [u8; 4] {
    let distance_to_edge = (global_x - rect.x)
        .min(rect.right() - global_x - 1)
        .min(global_y - rect.y)
        .min(rect.bottom() - global_y - 1);
    if distance_to_edge == 0 {
        OVERLAY_BORDER_DARK_BGRA
    } else if distance_to_edge < SELECTION_BORDER_WIDTH {
        OVERLAY_BORDER_WHITE_BGRA
    } else {
        OVERLAY_SELECTED_TRANSPARENT_BGRA
    }
}

fn create_overlay_buffers(
    shm: &WlShm,
    output_name: u32,
    width: i32,
    height: i32,
    qh: &QueueHandle<UiState>,
) -> Result<Vec<OverlayBuffer>, Box<dyn Error>> {
    let mut buffers = Vec::with_capacity(OVERLAY_BUFFER_COUNT);
    for index in 0..OVERLAY_BUFFER_COUNT {
        buffers.push(create_overlay_buffer(shm, width, height, qh, BufferId { output_name, index })?);
    }
    Ok(buffers)
}

fn create_overlay_buffer(
    shm: &WlShm,
    width: i32,
    height: i32,
    qh: &QueueHandle<UiState>,
    id: BufferId,
) -> Result<OverlayBuffer, Box<dyn Error>> {
    if width <= 0 || height <= 0 {
        return Err("overlay dimensions must be positive".into());
    }
    let stride = width.checked_mul(4).ok_or("overlay stride overflow")?;
    let size = usize::try_from(stride)?.checked_mul(usize::try_from(height)?).ok_or("overlay buffer size overflow")?;
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
    let buffer = pool.create_buffer(0, width, height, stride, Format::Argb8888, qh, id);
    Ok(OverlayBuffer { size, data: data.cast(), _fd: fd, pool, buffer, available: true, initialized: false, last_selected: None })
}

impl Dispatch<WlRegistry, ()> for UiState {
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
            "wl_compositor" => state.compositor = Some(registry.bind::<WlCompositor, _, _>(name, version.min(5), qh, ())),
            "wl_shm" => state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ())),
            "zwlr_layer_shell_v1" => state.layer_shell = Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ())),
            "zxdg_output_manager_v1" => state.xdg_output_manager = Some(registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ())),
            "wl_output" => {
                let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);
                state.outputs.push(OutputRuntime {
                    info: OutputInfo { global_name: name, logical: LogicalRect { x: 0, y: 0, width: 1, height: 1 }, scale: 1 },
                    output,
                    xdg_output: None,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for UiState {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let WEnum::Value(capabilities) = capabilities else {
                return;
            };
            if capabilities.contains(Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()))
            }
            if capabilities.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()))
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for UiState {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, surface, .. } => {
                state.pointer_x = surface_x as i32;
                state.pointer_y = surface_y as i32;
                state.pointer_output_name = state.overlays.iter().find(|overlay| overlay.surface == surface).map(|overlay| overlay.output_name);
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer_x = surface_x as i32;
                state.pointer_y = surface_y as i32;
                if let Some((x, y)) = state.pointer_global_position() {
                    if state.drag.update(x, y) {
                        render_overlays(state);
                    }
                }
            }
            wl_pointer::Event::Button { button, state: button_state, .. } if button == BTN_LEFT => {
                let WEnum::Value(button_state) = button_state else {
                    return;
                };
                if let Some((x, y)) = state.pointer_global_position() {
                    match button_state {
                        wl_pointer::ButtonState::Pressed => state.drag.begin(x, y),
                        wl_pointer::ButtonState::Released => state.drag.finish(x, y),
                        _ => {}
                    }
                    render_overlays(state);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for UiState {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
            if key == KEY_ESC && matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
                state.drag = DragState::Cancelled;
            } else if key == KEY_L
                && matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed))
            {
                state.long_requested = true;
                if state.drag.is_finished() {
                    state.drag.finish(state.pointer_x, state.pointer_y);
                }
            } else if state.drag.is_finished()
                && matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed))
            {
                match key {
                    KEY_ENTER | KEY_SPACE => {
                        state.long_finish_requested = true;
                    }
                    KEY_DOWN => {
                        state.long_direction = Some(SearchDirection::Down);
                    }
                    KEY_UP => {
                        state.long_direction = Some(SearchDirection::Up);
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for UiState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = event {
            layer_surface.ack_configure(serial);
            let Some(shm) = state.shm.clone() else {
                state.drag = DragState::Cancelled;
                return;
            };
            let configured_width = width as i32;
            let configured_height = height as i32;
            let Some(overlay) = state.overlays.iter_mut().find(|overlay| overlay.output_name == *name) else {
                return;
            };
            overlay.width = configured_width;
            overlay.height = configured_height;
            if let (Some(output), Some(frozen)) = (
                state.outputs.iter().find(|o| o.info.global_name == *name),
                state.frozen_outputs.iter().find(|f| f.info.global_name == *name),
            ) {
                overlay.render_cache = Some(build_overlay_render_cache(configured_width, configured_height, output.info, frozen));
            }
            match create_overlay_buffers(&shm, *name, overlay.width, overlay.height, qh) {
                Ok(buffers) => overlay.buffers = buffers,
                Err(_) => state.drag = DragState::Cancelled,
            }
            render_overlays(state);
        }
    }
}

impl Dispatch<WlBuffer, BufferId> for UiState {
    fn event(state: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, id: &BufferId, _: &Connection, _: &QueueHandle<Self>) {
        if let Some(buffer) = state
            .overlays
            .iter_mut()
            .find(|overlay| overlay.output_name == id.output_name)
            .and_then(|overlay| overlay.buffers.get_mut(id.index))
        {
            buffer.available = true;
        }
    }
}

impl Dispatch<WlOutput, u32> for UiState {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, name: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(output) = state.outputs.iter_mut().find(|output| output.info.global_name == *name) else {
            return;
        };
        match event {
            wl_output::Event::Scale { factor } => output.info.scale = factor.max(1),
            wl_output::Event::Mode { width, height, .. } if output.info.logical.width <= 1 && output.info.logical.height <= 1 => {
                output.info.logical.width = width;
                output.info.logical.height = height;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for UiState {
    fn event(state: &mut Self, _: &ZxdgOutputV1, event: zxdg_output_v1::Event, name: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(output) = state.outputs.iter_mut().find(|output| output.info.global_name == *name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.info.logical.x = x;
                output.info.logical.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.info.logical.width = width.max(1);
                output.info.logical.height = height.max(1);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for UiState {
    fn event(_: &mut Self, _: &WlShm, _: <WlShm as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlCompositor, ()> for UiState {
    fn event(_: &mut Self, _: &WlCompositor, _: <WlCompositor as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrLayerShellV1, ()> for UiState {
    fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZxdgOutputManagerV1, ()> for UiState {
    fn event(_: &mut Self, _: &ZxdgOutputManagerV1, _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(UiState: ignore WlSurface);
delegate_noop!(UiState: ignore WlRegion);
delegate_noop!(UiState: ignore WlShmPool);

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTED: LogicalRect = LogicalRect { x: 10, y: 20, width: 30, height: 40 };

    #[test]
    fn overlay_pixel_dims_outside_selection() {
        assert_eq!(overlay_pixel(9, 20, Some(SELECTED)), OVERLAY_OUTSIDE_MASK_BGRA);
        assert_eq!(overlay_pixel(10, 19, Some(SELECTED)), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn overlay_pixel_keeps_selection_interior_transparent() {
        assert_eq!(overlay_pixel(13, 23, Some(SELECTED)), OVERLAY_SELECTED_TRANSPARENT_BGRA);
    }

    #[test]
    fn overlay_pixel_draws_dark_edge_border() {
        assert_eq!(overlay_pixel(10, 20, Some(SELECTED)), OVERLAY_BORDER_DARK_BGRA);
    }

    #[test]
    fn overlay_pixel_draws_white_adjacent_border() {
        assert_eq!(overlay_pixel(11, 21, Some(SELECTED)), OVERLAY_BORDER_WHITE_BGRA);
    }

    #[test]
    fn selected_local_intersection_translates_global_selection_to_output_local() {
        let output = LogicalRect { x: 100, y: 50, width: 80, height: 60 };
        let selected = LogicalRect { x: 90, y: 70, width: 40, height: 30 };

        assert_eq!(
            selected_local_intersection(output, selected),
            Some(LogicalRect { x: 0, y: 20, width: 30, height: 30 })
        );
    }

    #[test]
    fn selected_local_intersection_returns_none_for_other_outputs() {
        let output = LogicalRect { x: 100, y: 50, width: 80, height: 60 };
        let selected = LogicalRect { x: 0, y: 0, width: 40, height: 30 };

        assert_eq!(selected_local_intersection(output, selected), None);
    }

    #[test]
    fn dirty_selected_region_unions_previous_and_current_selection() {
        let previous = LogicalRect { x: 5, y: 6, width: 10, height: 10 };
        let current = LogicalRect { x: 12, y: 14, width: 10, height: 4 };

        assert_eq!(
            dirty_selected_region(Some(previous), Some(current), 40, 40),
            Some(LogicalRect { x: 5, y: 6, width: 17, height: 12 })
        );
    }

    #[test]
    fn dirty_selected_region_clips_to_buffer_bounds() {
        let previous = LogicalRect { x: 20, y: 20, width: 20, height: 20 };
        let current = LogicalRect { x: 25, y: 25, width: 20, height: 20 };

        assert_eq!(
            dirty_selected_region(Some(previous), Some(current), 30, 32),
            Some(LogicalRect { x: 20, y: 20, width: 10, height: 12 })
        );
    }

    #[test]
    fn drag_update_returns_false_when_idle() {
        let mut drag = DragState::Idle;

        assert!(!drag.update(10, 20));
        assert_eq!(drag, DragState::Idle);
    }

    #[test]
    fn drag_update_returns_false_for_same_current_point() {
        let mut drag = DragState::Idle;
        drag.begin(10, 20);

        assert!(!drag.update(10, 20));
        assert_eq!(drag.current_rect(), Some(LogicalRect::from_points(10, 20, 10, 20)));
    }

    #[test]
    fn drag_update_returns_true_when_current_point_changes() {
        let mut drag = DragState::Idle;
        drag.begin(10, 20);

        assert!(drag.update(15, 25));
        assert_eq!(drag.current_rect(), Some(LogicalRect::from_points(10, 20, 15, 25)));
    }

    #[test]
    fn draw_overlay_masks_output_and_clears_only_local_selection() {
        let output = LogicalRect { x: 100, y: 50, width: 10, height: 10 };
        let selected = LogicalRect { x: 102, y: 52, width: 8, height: 8 };
        let mut data = vec![0; (output.width * output.height * 4) as usize];

        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let frozen_pixel = [10, 20, 30, 255];
        let frozen = frozen_test_output(output_info, output.width, output.height, frozen_pixel);
        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen);

        draw_overlay(&mut data, &cache, output_info, Some(selected));

        assert_eq!(pixel_at(&data, output.width as usize, 0, 0), [6, 11, 17, 255]);
        assert_eq!(pixel_at(&data, output.width as usize, 2, 2), OVERLAY_BORDER_DARK_BGRA);
        assert_eq!(pixel_at(&data, output.width as usize, 3, 3), OVERLAY_BORDER_WHITE_BGRA);
        assert_eq!(pixel_at(&data, output.width as usize, 5, 5), frozen_pixel);
    }

    #[test]
    fn draw_overlay_region_restores_old_selection_area() {
        let output = LogicalRect { x: 100, y: 50, width: 8, height: 8 };
        let previous = LogicalRect { x: 101, y: 51, width: 5, height: 5 };
        let current = LogicalRect { x: 103, y: 53, width: 3, height: 3 };
        let dirty = dirty_selected_region(
            selected_local_intersection(output, previous),
            selected_local_intersection(output, current),
            output.width,
            output.height,
        )
        .unwrap();
        let mut data = vec![0; (output.width * output.height * 4) as usize];

        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let frozen_pixel = [10, 20, 30, 255];
        let frozen = frozen_test_output(output_info, output.width, output.height, frozen_pixel);
        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen);

        draw_overlay(&mut data, &cache, output_info, Some(previous));
        draw_overlay_region(&mut data, &cache, output_info, Some(current), dirty);

        assert_eq!(pixel_at(&data, output.width as usize, 1, 1), [6, 11, 17, 255]);
        assert_eq!(pixel_at(&data, output.width as usize, 3, 3), OVERLAY_BORDER_DARK_BGRA);
    }

    #[test]
    fn build_overlay_render_cache_stores_base_and_dimmed_pixels() {
        let output = LogicalRect { x: 0, y: 0, width: 2, height: 1 };
        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let mut frozen = frozen_test_output(output_info, output.width, output.height, [10, 20, 30, 255]);
        frozen.data[4..8].copy_from_slice(&[40, 50, 60, 255]);

        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen);

        assert_eq!(pixel_at(&cache.base, output.width as usize, 0, 0), [10, 20, 30, 255]);
        assert_eq!(pixel_at(&cache.base, output.width as usize, 1, 0), [40, 50, 60, 255]);
        assert_eq!(pixel_at(&cache.dimmed, output.width as usize, 0, 0), [6, 11, 17, 255]);
        assert_eq!(pixel_at(&cache.dimmed, output.width as usize, 1, 0), [23, 28, 34, 255]);
    }

    #[test]
    fn frozen_pixel_samples_scale_two_outputs() {
        let output_info = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 2, height: 2 }, scale: 2 };
        let mut frozen = frozen_test_output(output_info, 4, 4, [0, 0, 0, 255]);
        frozen.data[(2 * frozen.stride + 2 * 4) as usize..(2 * frozen.stride + 3 * 4) as usize].copy_from_slice(&[11, 22, 33, 255]);

        assert_eq!(frozen_pixel(&frozen, output_info, 2, 2, 1, 1), [11, 22, 33, 255]);
    }

    fn pixel_at(data: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * width + x) * 4;
        data[offset..offset + 4].try_into().unwrap()
    }

    fn frozen_test_output(info: OutputInfo, width: i32, height: i32, pixel: [u8; 4]) -> FrozenOutput {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&pixel);
        }
        FrozenOutput { info, width, height, stride: width * 4, data }
    }
}
