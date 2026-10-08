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
const KEY_H: u32 = 35;
const KEY_E: u32 = 18;
const KEY_ENTER: u32 = 28;
const KEY_SPACE: u32 = 57;
const KEY_DOWN: u32 = 108;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DIGIT_FIRST: u32 = 2;
const KEY_DIGIT_LAST: u32 = 10;
const OVERLAY_BUFFER_COUNT: usize = 3;
const SELECTION_BORDER_WIDTH: i32 = 3;
/// Logical-pixel snap range around each selection edge in edit mode.
const EDGE_SNAP_RANGE: i32 = 8;
/// Smallest selection kept while dragging an edge.
const MIN_SELECTION_SIZE: i32 = 8;
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

    fn finished_rect_mut(&mut self) -> Option<&mut LogicalRect> {
        match self {
            Self::Finished(rect) => Some(rect),
            _ => None,
        }
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
    base_pixels: Option<Vec<u8>>,
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
    frozen_outputs_without_cursor: Vec<FrozenOutput>,
    overlays: Vec<OverlaySurface>,
    pointer_output_name: Option<u32>,
    long_mode: bool,
    screenshot_cursor_hidden: bool,
    pointer_x: i32,
    pointer_y: i32,
    drag: DragState,
    long_requested: bool,
    long_finish_requested: bool,
    long_direction: Option<SearchDirection>,
    long_preview: Option<LongPreviewSnapshot>,
    pending_render: Option<bool>,
    /// Selection-box edit mode (toggled with E). While set, the four box
    /// edges have snap targets and can be dragged to resize the box.
    editing: bool,
    edge_drag: Option<SelectionEdge>,
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

#[allow(dead_code)]
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
        frozen_outputs_without_cursor: Vec::new(),
        overlays: Vec::new(),
        pointer_output_name: None,
        long_mode: false,
        screenshot_cursor_hidden: true,
        pointer_x: 0,
        pointer_y: 0,
        drag: DragState::Idle,
            long_requested: false,
        long_finish_requested: false,
        long_direction: None,
            long_preview: None,
            pending_render: None,
        editing: false,
        edge_drag: None,
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

    while selection_loop_active(&state) {
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
    Left,
    Right,
    #[allow(dead_code)]
    Vertical,
}

/// One of the four selection-box edges that can be grabbed in edit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Returns the edge whose on-screen segment is nearest to `(x, y)`, provided
/// the distance is within `snap` logical pixels. Deterministic tie-break
/// order is Left, Top, Right, Bottom, so corners resolve consistently.
pub fn hit_test_edge(rect: LogicalRect, x: i32, y: i32, snap: i32) -> Option<SelectionEdge> {
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let candidates = [
        (
            SelectionEdge::Left,
            distance_point_segment_squared(x, y, rect.x, rect.y, rect.x, rect.bottom()),
        ),
        (
            SelectionEdge::Top,
            distance_point_segment_squared(x, y, rect.x, rect.y, rect.right(), rect.y),
        ),
        (
            SelectionEdge::Right,
            distance_point_segment_squared(x, y, rect.right(), rect.y, rect.right(), rect.bottom()),
        ),
        (
            SelectionEdge::Bottom,
            distance_point_segment_squared(x, y, rect.x, rect.bottom(), rect.right(), rect.bottom()),
        ),
    ];
    let snap_squared = f64::from(snap).max(0.0) * f64::from(snap).max(0.0);
    let mut best: Option<(SelectionEdge, f64)> = None;
    for (edge, distance) in candidates {
        if distance <= snap_squared && best.map_or(true, |(_, best_distance)| distance < best_distance) {
            best = Some((edge, distance));
        }
    }
    best.map(|(edge, _)| edge)
}

fn distance_point_segment_squared(px: i32, py: i32, ax: i32, ay: i32, bx: i32, by: i32) -> f64 {
    let dx = f64::from(bx - ax);
    let dy = f64::from(by - ay);
    let length_squared = dx * dx + dy * dy;
    let to_x = f64::from(px - ax);
    let to_y = f64::from(py - ay);
    if length_squared == 0.0 {
        return to_x * to_x + to_y * to_y;
    }
    let t = ((to_x * dx + to_y * dy) / length_squared).clamp(0.0, 1.0);
    let closest_x = to_x - t * dx;
    let closest_y = to_y - t * dy;
    closest_x * closest_x + closest_y * closest_y
}

/// Moves one edge of `rect` to follow the pointer, clamped to `bounds` and to
/// the minimum selectable size. The opposite edge always stays fixed.
pub fn apply_edge_drag(
    rect: LogicalRect,
    edge: SelectionEdge,
    x: i32,
    y: i32,
    bounds: LogicalRect,
) -> LogicalRect {
    match edge {
        SelectionEdge::Left => {
            let max_x = rect.right() - MIN_SELECTION_SIZE;
            let new_x = x.clamp(bounds.x.min(max_x), max_x.max(bounds.x));
            LogicalRect { x: new_x, width: rect.right() - new_x, ..rect }
        }
        SelectionEdge::Right => {
            let min_right = rect.x + MIN_SELECTION_SIZE;
            let new_right = x.clamp(min_right, bounds.right().max(min_right));
            LogicalRect { width: new_right - rect.x, ..rect }
        }
        SelectionEdge::Top => {
            let max_y = rect.bottom() - MIN_SELECTION_SIZE;
            let new_y = y.clamp(bounds.y.min(max_y), max_y.max(bounds.y));
            LogicalRect { y: new_y, height: rect.bottom() - new_y, ..rect }
        }
        SelectionEdge::Bottom => {
            let min_bottom = rect.y + MIN_SELECTION_SIZE;
            let new_bottom = y.clamp(min_bottom, bounds.bottom().max(min_bottom));
            LogicalRect { height: new_bottom - rect.y, ..rect }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongDirection {
    Down,
    Up,
    #[allow(dead_code)]
    Right,
    #[allow(dead_code)]
    Left,
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
        Self::new_internal(None, None, true)
    }

    pub fn with_frozen_pair(
        frozen_outputs: &[CapturedOutput],
        frozen_outputs_without_cursor: &[CapturedOutput],
    ) -> Result<Self, Box<dyn Error>> {
        Self::new_internal(Some(frozen_outputs), Some(frozen_outputs_without_cursor), false)
    }

    fn new_internal(
        frozen_outputs: Option<&[CapturedOutput]>,
        frozen_outputs_without_cursor: Option<&[CapturedOutput]>,
        long_mode: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()?;
        let mut event_queue = conn.new_event_queue::<UiState>();
        let qh = event_queue.handle();
        let frozen_outputs = match frozen_outputs {
            Some(captures) => frozen_outputs_from_captures(captures)?,
            None => Vec::new(),
        };
        let frozen_outputs_without_cursor = match frozen_outputs_without_cursor {
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
            frozen_outputs_without_cursor,
            overlays: Vec::new(),
            pointer_output_name: None,
            long_mode,
            screenshot_cursor_hidden: true,
            pointer_x: 0,
            pointer_y: 0,
            drag: DragState::Idle,
            long_requested: false,
            long_finish_requested: false,
            long_direction: None,
            long_preview: None,
            pending_render: None,
            editing: false,
            edge_drag: None,
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
        if !state.frozen_outputs.is_empty() {
            validate_frozen_outputs(&state.outputs, &state.frozen_outputs)?;
        }
        if !state.frozen_outputs_without_cursor.is_empty() {
            validate_frozen_outputs(&state.outputs, &state.frozen_outputs_without_cursor)?;
        }
        create_live_overlays(&mut state, &qh)?;
        conn.flush()?;
        Ok(Self { conn, event_queue, state })
    }

    pub fn run_selection(&mut self) -> Result<SelectionOutcome, Box<dyn Error>> {
        while selection_loop_active(&self.state) {
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

    /// Current selection rectangle once a selection exists, live-updated by
    /// edge drags in edit mode. The long-capture loop polls this to follow
    /// selection-box edits made during capture.
    pub fn selection_rect(&self) -> Option<LogicalRect> {
        self.state.drag.selected_rect()
    }

    /// Advertised output geometry, for recomputing viewports after the
    /// selection rectangle changed in edit mode.
    pub fn output_infos(&self) -> Vec<OutputInfo> {
        self.state.output_infos()
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
        self.state.editing = false;
        self.state.edge_drag = None;
        for overlay in &mut self.state.overlays {
            overlay.render_cache = None;
            overlay.base_pixels = None;
        }
        // Retry until at least one overlay is rendered (buffer available)
        for _ in 0..50 {
            self.event_queue.dispatch_pending(&mut self.state)?;
            if render_overlays_full_dim(&mut self.state) > 0 {
                break;
            }
            self.dispatch_poll(10)?;
        }
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

    pub fn screenshot_cursor_hidden(&self) -> bool {
        self.state.screenshot_cursor_hidden
    }

    pub fn update_long_capture_preview(&mut self, snapshot: LongPreviewSnapshot) -> Result<(), Box<dyn Error>> {
        self.state.long_preview = Some(snapshot);
        queue_render(&mut self.state, true);
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

    #[allow(dead_code)]
    pub fn dispatch_blocking(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.blocking_dispatch(&mut self.state)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn dispatch_pending(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn wait_configured(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.blocking_dispatch(&mut self.state)?;
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn prepare_capture_clean(&mut self) -> Result<(), Box<dyn Error>> {
        render_overlays_full_dim(&mut self.state);
        self.event_queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn close(self) -> Result<(), Box<dyn Error>> {
        drop(self.state);
        drop(self.event_queue);
        let _ = self.conn.flush();
        Ok(())
    }
}

/// The interactive selection loop keeps running while a drag is in progress
/// or while edit mode holds a finished rectangle open for adjustment.
fn selection_loop_active(state: &UiState) -> bool {
    !state.drag.is_terminal() || state.editing
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
            base_pixels: None,
            width: output.info.logical.width.max(1),
            height: output.info.logical.height.max(1),
        });
    }
    Ok(())
}

fn render_overlays_full_dim(state: &mut UiState) -> usize {
    render_overlays_inner(state, true)
}

#[allow(dead_code)]
fn render_overlays_capture_clean(state: &mut UiState) -> usize {
    render_overlays_inner(state, false)
}

fn render_overlays_inner(state: &mut UiState, draw_border: bool) -> usize {
    let mut committed = 0;
    for i in 0..state.overlays.len() {
        let overlay = &mut state.overlays[i];
        let buffer = match overlay.buffers.iter().position(|b| b.available) {
            Some(pos) => &mut overlay.buffers[pos],
            None => continue,
        };
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.data, buffer.size) };
        let buf_size = overlay.width as usize * overlay.height as usize * 4;
        let stride = overlay.width as usize * 4;
        let rect = state.drag.current_rect();
        let output_logical = state.outputs.iter()
            .find(|o| o.info.global_name == overlay.output_name)
            .map(|o| o.info.logical)
            .unwrap_or(LogicalRect { x: 0, y: 0, width: overlay.width, height: overlay.height });
        let selection_local = rect.and_then(|r| {
            selected_local_intersection_for_buffer(output_logical, r, overlay.width, overlay.height)
        });

        let rebuild = overlay.base_pixels.as_ref().map_or(true, |b| b.len() != buf_size);
        if rebuild {
            for y in 0..overlay.height as usize {
                for x in 0..overlay.width as usize {
                    let offset = y * stride + x * 4;
                    data[offset..offset + 4].copy_from_slice(&OVERLAY_OUTSIDE_MASK_BGRA);
                }
            }
            if let Some(ref local) = selection_local {
                for y in local.y..local.y + local.height {
                    for x in local.x..local.x + local.width {
                        let offset = y as usize * stride + x as usize * 4;
                        data[offset + 3] = 0;
                    }
                }
                if draw_border {
                    draw_selection_border(data, overlay.width, overlay.height, local.x, local.y, local.width, local.height);
                }
            }
            let cache = data[..buf_size].to_vec();
            overlay.base_pixels = Some(cache);
        } else if let Some(ref base) = overlay.base_pixels {
            data[..buf_size.min(base.len())].copy_from_slice(&base[..buf_size.min(base.len())]);
        }

        if let Some(ref preview) = state.long_preview {
            draw_preview_on_overlay(data, overlay.width, overlay.height, preview, output_logical, selection_local);
        }
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay.surface.damage_buffer(0, 0, overlay.width, overlay.height);
        overlay.surface.commit();
        buffer.available = false;
        committed += 1;
    }
    committed
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

/// Restores full pointer input on the overlays so edit mode can receive
/// button/motion events. Receiving the pointer also means wheel events stop
/// at the overlay: the underlying content cannot scroll while the selection
/// box is being edited.
fn set_overlay_pointer_input_interactive(state: &mut UiState) {
    for overlay in &mut state.overlays {
        overlay.surface.set_input_region(None);
        overlay.surface.commit();
    }
}

/// Drops cached overlay bases so the next render rebuilds them with the
/// current selection rectangle (used after selection-box edits).
fn invalidate_overlay_bases(state: &mut UiState) {
    for overlay in &mut state.overlays {
        overlay.base_pixels = None;
    }
}

fn digit_key_index(key: u32) -> Option<usize> {
    if (KEY_DIGIT_FIRST..=KEY_DIGIT_LAST).contains(&key) {
        Some((key - KEY_DIGIT_FIRST) as usize)
    } else {
        None
    }
}

/// Toggles selection-box edit mode (E key). Entering edit mode finalizes an
/// in-progress drag into an adjustable rectangle; in long mode it also grabs
/// pointer input (blocking wheel scroll). Leaving edit mode restores long
/// mode pointer passthrough.
fn handle_edit_toggle(state: &mut UiState, qh: &QueueHandle<UiState>) {
    if state.editing {
        state.editing = false;
        state.edge_drag = None;
        if state.long_mode {
            let _ = set_overlay_pointer_passthrough(state, qh);
        }
        invalidate_overlay_bases(state);
        render_overlays(state);
    } else if state.drag.current_rect().is_some() {
        if let DragState::Dragging { .. } = state.drag {
            let (x, y) = state
                .pointer_global_position()
                .unwrap_or((state.pointer_x, state.pointer_y));
            state.drag.finish(x, y);
        }
        state.editing = true;
        state.edge_drag = None;
        if state.long_mode {
            set_overlay_pointer_input_interactive(state);
        }
        invalidate_overlay_bases(state);
        render_overlays(state);
    }
}

/// Output-local bounds the selection is being edited within, so edge drags
/// cannot move the box off the output it was captured on.
fn selection_output_bounds(state: &UiState, rect: LogicalRect) -> LogicalRect {
    let center_x = rect.x + rect.width / 2;
    let center_y = rect.y + rect.height / 2;
    state
        .outputs
        .iter()
        .find(|output| {
            let logical = output.info.logical;
            center_x >= logical.x
                && center_x < logical.right()
                && center_y >= logical.y
                && center_y < logical.bottom()
        })
        .map(|output| output.info.logical)
        .or_else(|| {
            state
                .outputs
                .iter()
                .find(|output| output.info.logical.intersection(rect).is_some())
                .map(|output| output.info.logical)
        })
        .unwrap_or(rect)
}

fn full_output_rect_by_index(output_infos: &[OutputInfo], index: usize) -> Option<LogicalRect> {
    let mut ordered: Vec<&OutputInfo> = output_infos.iter().collect();
    ordered.sort_by_key(|info| (info.logical.x, info.logical.y));
    Some(ordered.get(index)?.logical)
}

fn toggle_frozen_frame(state: &mut UiState) {
    state.screenshot_cursor_hidden = !state.screenshot_cursor_hidden;
    for index in 0..state.overlays.len() {
        let output_name = state.overlays[index].output_name;
        let width = state.overlays[index].width;
        let height = state.overlays[index].height;
        let Some(output) = state.outputs.iter().find(|output| output.info.global_name == output_name) else {
            continue;
        };
        let captures = if state.screenshot_cursor_hidden {
            &state.frozen_outputs_without_cursor
        } else {
            &state.frozen_outputs
        };
        let Some(frozen) = captures.iter().find(|frozen| frozen.info.global_name == output_name) else {
            continue;
        };
        let overlay = &mut state.overlays[index];
        overlay.render_cache = Some(build_overlay_render_cache(width, height, output.info, frozen));
        for buffer in &mut overlay.buffers {
            buffer.initialized = false;
            buffer.last_selected = None;
        }
    }
    queue_render(state, false);
}

fn queue_render(state: &mut UiState, full: bool) {
    let committed = if full {
        render_overlays_full_dim(state)
    } else {
        render_overlays(state)
    };
    state.pending_render = if committed < state.overlays.len() { Some(full) } else { None };
}

fn flush_pending_render(state: &mut UiState) {
    let Some(full) = state.pending_render else { return; };
    let committed = if full {
        render_overlays_full_dim(state)
    } else {
        render_overlays(state)
    };
    if committed >= state.overlays.len() {
        state.pending_render = None;
    }
}

fn bind_xdg_outputs(state: &mut UiState, qh: &QueueHandle<UiState>) {
    let Some(manager) = state.xdg_output_manager.as_ref() else {
        return;
    };
    for output in &mut state.outputs {
        output.xdg_output = Some(manager.get_xdg_output(&output.output, qh, output.info.global_name));
    }
}

#[allow(dead_code)]
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
            base_pixels: None,
            width: output.info.logical.width.max(1),
            height: output.info.logical.height.max(1),
        });
    }
    Ok(())
}

fn render_overlays(state: &mut UiState) -> usize {
    let mut committed = 0;
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
            committed += 1;
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
        committed += 1;
    }
    committed
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

#[allow(dead_code)]
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
    let pw = preview.image.width as i32;
    let ph = preview.image.height as i32;
    if pw == 0 || ph == 0 || overlay_w <= 0 || overlay_h <= 0 {
        return;
    }
    let vp = preview.viewport_rect;
    let vp_w = vp.width.max(1);
    let vp_h = vp.height.max(1);
    let cw = preview.capture_width.max(1) as i32;
    let ch = preview.capture_height.max(1) as i32;
    let origin_x = preview.current_origin_x;
    let origin_y = preview.current_origin_y;
    let buf_stride = overlay_w as usize * 4;
    let sel = selection_local.unwrap_or(LogicalRect { x: 0, y: 0, width: 0, height: 0 });
    let img_data = &preview.image.data;
    let img_stride = preview.image.stride as i32;

    let global_y0 = output_logical.y;
    let global_y1 = output_logical.y + overlay_h;
    let global_x0 = output_logical.x;
    let global_x1 = output_logical.x + overlay_w;
    let sy0 = (origin_y + (global_y0 - vp.y) * ch / vp_h).max(0).min(ph);
    let sy1 = (origin_y + (global_y1 - vp.y) * ch / vp_h + 1).max(0).min(ph);
    // Precompute visible x-range in stitched coords
    let sx_min = (origin_x + (global_x0 - vp.x) * cw / vp_w).max(0);
    let sx_max = (origin_x + (global_x1 - vp.x) * cw / vp_w + 1).min(pw);

    for sy in sy0..sy1 {
        let global_y = vp.y + (sy - origin_y) * vp_h / ch;
        if global_y < global_y0 || global_y >= global_y1 {
            continue;
        }
        let oy = global_y - global_y0;
        let in_sel_y = oy >= sel.y && oy < sel.y + sel.height;
        // For rows fully covered by selection, skip entire row
        if in_sel_y {
            let global_sx0 = vp.x + (sx_min - origin_x) * vp_w / cw;
            let global_sx1 = vp.x + (sx_max - origin_x) * vp_w / cw;
            let ox0 = global_sx0 - global_x0;
            let ox1 = global_sx1 - global_x0;
            if ox0 >= sel.x && ox1 <= sel.x + sel.width {
                continue;
            }
        }
        let src_row = (sy * img_stride) as usize;

        for sx in sx_min..sx_max {
            let global_x = vp.x + (sx - origin_x) * vp_w / cw;
            if global_x < global_x0 || global_x >= global_x1 {
                continue;
            }
            let ox = global_x - global_x0;
            if in_sel_y && ox >= sel.x && ox < sel.x + sel.width {
                continue;
            }
            let src_off = src_row + (sx * 4) as usize;
            if src_off + 3 >= img_data.len() {
                continue;
            }
            // Never-captured corners of ragged (T/L-shaped) stitched canvases
            // stay transparent; leave the dimmed overlay visible beneath them.
            if img_data[src_off + 3] == 0 {
                continue;
            }
            let dst_off = (oy as usize) * buf_stride + (ox as usize) * 4;
            data[dst_off] = img_data[src_off];
            data[dst_off + 1] = img_data[src_off + 1];
            data[dst_off + 2] = img_data[src_off + 2];
            data[dst_off + 3] = 255;
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
                let Some((x, y)) = state.pointer_global_position() else {
                    return;
                };
                if let Some(edge) = state.edge_drag {
                    if let Some(rect) = state.drag.selected_rect() {
                        let bounds = selection_output_bounds(state, rect);
                        let updated = apply_edge_drag(rect, edge, x, y, bounds);
                        if updated != rect {
                            if let Some(current) = state.drag.finished_rect_mut() {
                                *current = updated;
                            }
                            invalidate_overlay_bases(state);
                            render_overlays(state);
                        }
                    }
                } else if state.drag.update(x, y) {
                    render_overlays(state);
                }
            }
            wl_pointer::Event::Button { button, state: button_state, .. } if button == BTN_LEFT => {
                let WEnum::Value(button_state) = button_state else {
                    return;
                };
                let Some((x, y)) = state.pointer_global_position() else {
                    return;
                };
                match button_state {
                    wl_pointer::ButtonState::Pressed => {
                        if state.editing && state.drag.is_finished() {
                            // Edges have snap targets only in edit mode.
                            state.edge_drag = state
                                .drag
                                .selected_rect()
                                .and_then(|rect| hit_test_edge(rect, x, y, EDGE_SNAP_RANGE));
                        } else if !state.editing {
                            state.drag.begin(x, y);
                        }
                    }
                    wl_pointer::ButtonState::Released => {
                        if state.edge_drag.take().is_some() {
                            // Finished one edge drag; stay in edit mode.
                        } else if !state.editing {
                            state.drag.finish(x, y);
                        }
                    }
                    _ => {}
                }
                render_overlays(state);
            }
            // Axis (wheel) events are intentionally ignored: edit mode keeps
            // pointer input on the overlay so scrolling stops here and the
            // displayed content never moves while the box is adjusted.
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for UiState {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
            if !matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
                return;
            }
            if key == KEY_ESC {
                state.editing = false;
                state.edge_drag = None;
                state.drag = DragState::Cancelled;
            } else if key == KEY_E {
                handle_edit_toggle(state, qh);
            } else if key == KEY_L {
                state.editing = false;
                state.edge_drag = None;
                state.long_requested = true;
                state.long_mode = true;
                if state.drag.is_finished() {
                    state.drag.finish(state.pointer_x, state.pointer_y);
                }
            } else if key == KEY_H && !state.long_mode {
                toggle_frozen_frame(state);
            } else if key == KEY_ENTER || key == KEY_SPACE {
                // Confirm the edited rectangle: leaving edit mode lets the
                // selection loop return the adjusted rect (and finishes a
                // running long capture).
                state.editing = false;
                state.edge_drag = None;
                if state.long_mode && state.drag.is_finished() {
                    state.long_finish_requested = true;
                }
            } else if state.long_mode && state.drag.is_finished() {
                match key {
                    KEY_DOWN => {
                        state.long_direction = Some(SearchDirection::Down);
                    }
                    KEY_UP => {
                        state.long_direction = Some(SearchDirection::Up);
                    }
                    KEY_LEFT => {
                        state.long_direction = Some(SearchDirection::Left);
                    }
                    KEY_RIGHT => {
                        state.long_direction = Some(SearchDirection::Right);
                    }
                    _ => {}
                }
            } else if !state.long_mode && state.drag == DragState::Idle {
                if let Some(rect) = digit_key_index(key).and_then(|index| full_output_rect_by_index(&state.output_infos(), index)) {
                    state.drag = DragState::Finished(rect);
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
                (if state.screenshot_cursor_hidden {
                    &state.frozen_outputs_without_cursor
                } else {
                    &state.frozen_outputs
                })
                .iter()
                .find(|f| f.info.global_name == *name),
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
        flush_pending_render(state);
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

    fn solid_image(width: u32, height: u32, rows: &[(u32, u32, [u8; 4])]) -> Image {
        let stride = width * 4;
        let mut data = vec![0u8; (stride * height) as usize];
        for &(row_begin, row_end, pixel) in rows {
            for y in row_begin..row_end {
                for x in 0..width {
                    let offset = ((y * stride) + x * 4) as usize;
                    data[offset..offset + 4].copy_from_slice(&pixel);
                }
            }
        }
        Image {
            width,
            height,
            stride,
            format: Format::Xrgb8888,
            data,
        }
    }

    #[test]
    fn preview_draws_grown_stitch_above_selection_and_keeps_hole_clear() {
        // Output occupies global (0,0) 100x100; selection hole at local (10,20) 40x40.
        // Stitch grew upward: current frame (40x40) sits at stitched rows 40..80,
        // history rows 0..40 are white, current rows are black.
        let preview = LongPreviewSnapshot {
            image: solid_image(
                40,
                80,
                &[(0, 40, [255, 255, 255, 255]), (40, 80, [0, 0, 0, 255])],
            ),
            current_origin_x: 0,
            current_origin_y: 40,
            viewport_rect: LogicalRect { x: 10, y: 20, width: 40, height: 40 },
            capture_width: 40,
            capture_height: 40,
        };
        let output_logical = LogicalRect { x: 0, y: 0, width: 100, height: 100 };
        let selection_local = Some(LogicalRect { x: 10, y: 20, width: 40, height: 40 });

        let mut data = vec![7u8; 100 * 100 * 4];
        draw_preview_on_overlay(&mut data, 100, 100, &preview, output_logical, selection_local);

        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 100 + x) * 4;
            [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]
        };
        // History rows above the hole are drawn opaque white.
        assert_eq!(pixel(30, 10), [255, 255, 255, 255]);
        // Inside the selection hole nothing is drawn (live content stays visible).
        assert_eq!(pixel(30, 40), [7, 7, 7, 7]);
        // Outside the stitched width nothing is drawn.
        assert_eq!(pixel(5, 10), [7, 7, 7, 7]);
    }

    #[test]
    fn digit_key_maps_to_zero_based_index() {
        assert_eq!(digit_key_index(KEY_DIGIT_FIRST), Some(0));
        assert_eq!(digit_key_index(KEY_DIGIT_LAST), Some(8));
        assert_eq!(digit_key_index(KEY_DIGIT_LAST + 1), None);
        assert_eq!(digit_key_index(KEY_H), None);
    }

    #[test]
    fn hit_test_edge_finds_nearest_edge_within_snap() {
        let rect = LogicalRect { x: 100, y: 100, width: 40, height: 30 };
        assert_eq!(hit_test_edge(rect, 98, 115, 8), Some(SelectionEdge::Left));
        assert_eq!(hit_test_edge(rect, 142, 115, 8), Some(SelectionEdge::Right));
        assert_eq!(hit_test_edge(rect, 120, 97, 8), Some(SelectionEdge::Top));
        assert_eq!(hit_test_edge(rect, 120, 133, 8), Some(SelectionEdge::Bottom));
        // Deep inside the box nothing snaps.
        assert_eq!(hit_test_edge(rect, 120, 115, 8), None);
        assert_eq!(hit_test_edge(rect, 50, 50, 8), None);
    }

    #[test]
    fn hit_test_edge_resolves_corners_deterministically() {
        let rect = LogicalRect { x: 100, y: 100, width: 40, height: 30 };
        // Equidistant to the left and top edges: Left wins by tie-break order.
        assert_eq!(hit_test_edge(rect, 97, 97, 8), Some(SelectionEdge::Left));
        assert_eq!(hit_test_edge(rect, 143, 133, 8), Some(SelectionEdge::Right));
    }

    #[test]
    fn hit_test_edge_rejects_empty_rectangles() {
        let rect = LogicalRect { x: 100, y: 100, width: 0, height: 30 };
        assert_eq!(hit_test_edge(rect, 100, 110, 8), None);
    }

    #[test]
    fn apply_edge_drag_moves_edges_independently() {
        let rect = LogicalRect { x: 100, y: 100, width: 40, height: 30 };
        let bounds = LogicalRect { x: 0, y: 0, width: 1920, height: 1080 };
        assert_eq!(
            apply_edge_drag(rect, SelectionEdge::Left, 90, 115, bounds),
            LogicalRect { x: 90, y: 100, width: 50, height: 30 }
        );
        assert_eq!(
            apply_edge_drag(rect, SelectionEdge::Right, 150, 115, bounds),
            LogicalRect { x: 100, y: 100, width: 50, height: 30 }
        );
        assert_eq!(
            apply_edge_drag(rect, SelectionEdge::Top, 115, 80, bounds),
            LogicalRect { x: 100, y: 80, width: 40, height: 50 }
        );
        assert_eq!(
            apply_edge_drag(rect, SelectionEdge::Bottom, 115, 140, bounds),
            LogicalRect { x: 100, y: 100, width: 40, height: 40 }
        );
    }

    #[test]
    fn apply_edge_drag_clamps_to_bounds_and_min_size() {
        let rect = LogicalRect { x: 100, y: 100, width: 40, height: 30 };
        let bounds = LogicalRect { x: 90, y: 90, width: 60, height: 60 };
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Left, 0, 115, bounds).x, 90);
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Left, 200, 115, bounds).width, MIN_SELECTION_SIZE);
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Right, 500, 115, bounds).width, 50);
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Right, 0, 115, bounds).width, MIN_SELECTION_SIZE);
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Top, 115, 0, bounds).y, 90);
        assert_eq!(apply_edge_drag(rect, SelectionEdge::Bottom, 115, 500, bounds).height, 50);
    }

    #[test]
    fn full_output_rect_orders_outputs_left_to_right() {
        let right = OutputInfo { global_name: 2, logical: LogicalRect { x: 300, y: 0, width: 100, height: 80 }, scale: 1 };
        let left = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 20, width: 100, height: 80 }, scale: 1 };
        let infos = [right, left];

        assert_eq!(full_output_rect_by_index(&infos, 0), Some(left.logical));
        assert_eq!(full_output_rect_by_index(&infos, 1), Some(right.logical));
        assert_eq!(full_output_rect_by_index(&infos, 2), None);
    }

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
