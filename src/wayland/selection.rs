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
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport,
    wp_viewporter::WpViewporter,
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
/// Three full-resolution buffers per overlay: one held on-screen by the
/// compositor, one committed and awaiting display, one free to keep the
/// commit cycle alive. With only two, a compositor whose `wl_buffer.release`
/// lags more than one frame (observed on multi-output niri where the frame
/// pace spans two outputs) starves both buffers, and the stall becomes
/// self-sustaining: no free buffer → no commit → no frame swap → no release.
const OVERLAY_BUFFER_COUNT: usize = 3;
/// Logical-pixel snap range around each selection edge in edit mode.
const EDGE_SNAP_RANGE: i32 = 8;
/// Smallest selection kept while dragging an edge.
const MIN_SELECTION_SIZE: i32 = 8;
const OVERLAY_OUTSIDE_MASK_BGRA: [u8; 4] = [0, 0, 0, 110];
const OVERLAY_SELECTED_TRANSPARENT_BGRA: [u8; 4] = [0, 0, 0, 0];
const OVERLAY_BORDER_WHITE_BGRA: [u8; 4] = [255, 255, 255, 255];
/// Warning border: white mixed into red (50/50). Used for a selection the
/// pointer is dragging across outputs and for stitcher refusals — lighter
/// than pure red so it reads as "attention", distinct from the plain white
/// selection border at any background brightness.
const OVERLAY_BORDER_WHITE_RED_BGRA: [u8; 4] = [128, 128, 255, 255];

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
    /// UiState::live_state_seq value this buffer was last committed with.
    live_seq: u64,
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
    /// Pixel size of the output's current mode (`wl_output.mode`), which is the
    /// grid the compositor scans out. `wl_output.scale` cannot say this: it is
    /// an integer hint, while a 1.5-scaled 1920x1080 output reports scale 2 for
    /// a 1280x720 logical size. Long capture starts without a frozen frame, so
    /// this is the only density source it has before the first capture arrives.
    mode_pixels: Option<(i32, i32)>,
}

struct OverlaySurface {
    output_name: u32,
    /// Declared before the surface so these are destroyed first: both are
    /// bound to the wl_surface they were created from.
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    /// Output's preferred scale in 120ths (wp_fractional_scale_v1); 120 until
    /// the compositor says otherwise.
    preferred_scale: i32,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffers: Vec<OverlayBuffer>,
    render_cache: Option<OverlayRenderCache>,
    /// Buffer size in pixels. On a fractionally scaled output this is the
    /// frozen capture's pixel size (larger than the logical size), so the
    /// frozen image is displayed one buffer pixel per output pixel; a
    /// wp_viewport maps it back onto the logical surface.
    width: i32,
    height: i32,
    /// Surface size in logical pixels, as configured by the compositor.
    logical_width: i32,
    logical_height: i32,
    /// Set once the layer surface has been configured.
    ///
    /// Before that there is no agreed surface size and the client must not
    /// attach a buffer. `wp_fractional_scale_v1.preferred_scale` arrives on the
    /// surface's first commit, so it can be dispatched *before* the layer
    /// configure — buffer creation and rendering therefore wait for the
    /// configure, which is also where the logical size comes from.
    configured: bool,
    base_pixels: Option<Vec<u8>>,
    /// Only the dim mask and transparent hole are cached. Preview and border
    /// are composited afterwards, in that order, on every live redraw.
    base_key: Option<(i32, i32, Option<LogicalRect>)>,
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
    /// Optional: without it the overlays keep a logical-sized buffer (the
    /// pre-viewport behaviour) and simply accept the compositor's upscale.
    viewporter: Option<WpViewporter>,
    /// Optional: reports the output's preferred scale as 120ths (180 = 1.5).
    /// Without it the frozen capture still sizes the frozen overlay correctly;
    /// only the live overlay stays at the logical density.
    fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
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
    /// Set while the stitcher is refusing frames: the new content could not be
    /// placed, so the border turns red and the user is asked to scroll back.
    /// Distinct from the spans-outputs warning, which is about the selection
    /// itself rather than the stitch state.
    stitch_warning: bool,
    pending_render: Option<bool>,
    /// Selection-box edit mode (toggled with E). While set, the four box
    /// edges have snap targets and can be dragged to resize the box.
    editing: bool,
    edit_drag: Option<EditDrag>,
    /// Selection excluded from preview until the pending capture completes.
    capture_pending: Option<LogicalRect>,
    /// Sticky until completion: edits may return to the original rectangle.
    capture_invalid: bool,
    /// Global point where the selection drag started. `L` promotion uses it to
    /// clamp a spanning normal-mode selection to the output containing this
    /// point — the box belongs to the screen it was clicked on, not to
    /// whichever screen covers more of its area. A *long-mode* drag that spans
    /// outputs is discarded on release instead (see `discard_selection`).
    selection_anchor: Option<(i32, i32)>,
    /// Visible selection-border thickness in logical pixels (configurable).
    border_width: i32,
    /// Bumped whenever the live overlay's non-rect visual inputs change
    /// (stitched preview updates). Buffers record the seq they were last
    /// committed with; a buffer whose seq and rect already match the current
    /// state needs no new commit. This keeps the commit-on-change contract
    /// from v0.1.0: steady-state capture frames issue zero overlay commits,
    /// so capture throughput never depends on compositor release timing.
    live_state_seq: u64,
    /// Long-capture render scope: when set, overlay rendering and the
    /// pending-render debt count only this output's overlay. Long capture
    /// captures a single output; the other outputs' overlays are static
    /// frozen images whose canvases turn over independently and must not
    /// gate the selected output's rendering.
    render_scope: Option<u32>,
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
        viewporter: None,
        fractional_scale_manager: None,
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
            stitch_warning: false,
            pending_render: None,
        editing: false,
        edit_drag: None,
        capture_pending: None,
        capture_invalid: false,
        selection_anchor: None,
        border_width: crate::config::DEFAULT_BORDER_WIDTH,
        live_state_seq: 1,
        render_scope: None,
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

/// An in-progress edit-mode drag: either reshaping one edge or moving the
/// whole box by its interior.
///
/// `origin` is the box as it was when the drag started. A long-mode drag that
/// ends up spanning outputs is reverted to it, so the drag behaves as if it had
/// never happened instead of leaving a box that can never be captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditDrag {
    Edge { edge: SelectionEdge, origin: LogicalRect },
    /// Pointer grab offset from the box origin, so the box follows the
    /// pointer without jumping to it.
    Move { offset_x: i32, offset_y: i32, origin: LogicalRect },
}

impl EditDrag {
    fn origin(self) -> LogicalRect {
        match self {
            Self::Edge { origin, .. } | Self::Move { origin, .. } => origin,
        }
    }
}

/// Border color for the current selection.
///
/// White-red means "attention" and has two causes: a long-mode selection spans
/// outputs (release discards it, because a long capture is a single screen by
/// construction), or the stitcher is refusing the current content. Neither is a
/// silent failure.
fn selection_border_color(spans_warning: bool, stitch_warning: bool) -> [u8; 4] {
    if stitch_warning || spans_warning {
        OVERLAY_BORDER_WHITE_RED_BGRA
    } else {
        OVERLAY_BORDER_WHITE_BGRA
    }
}

fn selection_spans_outputs(state: &UiState, rect: LogicalRect) -> bool {
    let infos = state.output_infos();
    crate::geometry::rect_spans_outputs(rect, &infos)
}

/// Border color for the live selection, honoring the spans-outputs warning.
fn border_color_for_state(state: &UiState, rect: Option<LogicalRect>) -> [u8; 4] {
    let spans = rect.map_or(false, |rect| selection_spans_outputs(state, rect));
    border_color_for(rect, spans, state.long_mode, state.stitch_warning)
}

/// The colour decision, independent of Wayland state so it can be tested.
///
/// Only long mode warns about a spanning selection: a long capture is a single
/// screen by construction, so a spanning rect there is a real problem the user
/// must fix (release discards the box and returns to an empty selection).
/// Normal mode composites a spanning selection from per-output pieces,
/// which is a supported capture — warning about it would flag a working
/// selection as an error, during the drag and in edit mode alike.
fn border_color_for(
    rect: Option<LogicalRect>,
    spans: bool,
    long_mode: bool,
    stitch_warning: bool,
) -> [u8; 4] {
    selection_border_color(rect.is_some() && spans && long_mode, stitch_warning)
}

/// Selection rectangle used for overlay rendering: the current box united
/// with the region of an in-flight capture, so the hole always covers what
/// a pending frame was prepared from. The dim tracks the box live in both
/// directions — during an edit drag the border and the dim edge move
/// together, so shrinking animates as smoothly as enlarging. Correctness of
/// captured frames is not trusted to the hole: captures pause while an edit
/// drag is in progress, and any frame exposed to changed geometry is
/// invalidated before it can reach the stitcher.
fn ui_render_selection(state: &UiState) -> Option<LogicalRect> {
    let current = state.drag.current_rect();
    match (current, state.capture_pending) {
        (Some(current), Some(pending)) => Some(rect_union(current, pending)),
        (current, pending) => current.or(pending),
    }
}

/// Edits keep the current box visible. Frames exposed to an edited border
/// are discarded instead of hiding the UI until capture completes.
fn ui_render_border(state: &UiState) -> bool {
    state.drag.current_rect().is_some()
}

fn invalidate_changed_capture(state: &mut UiState) {
    if state.capture_pending.is_some_and(|pending| state.drag.current_rect() != Some(pending)) {
        state.capture_invalid = true;
    }
}

/// During an edit drag, a pending capture prepared from a different rectangle
/// is already invalid (sticky, see `invalidate_changed_capture`); drop its
/// region so the hole and the dim edge follow the current box immediately.
/// Without this the hole stays at the widest point of the drag and shrinking
/// looks like the border slides while the dim lags behind.
fn drop_invalid_pending(state: &mut UiState) {
    if state.capture_pending.is_some() && state.capture_pending != state.drag.current_rect() {
        state.capture_pending = None;
    }
}

fn border_bounds(rect: LogicalRect, thickness: i32) -> LogicalRect {
    let thickness = thickness.max(1);
    LogicalRect {
        x: rect.x.saturating_sub(thickness),
        y: rect.y.saturating_sub(thickness),
        width: rect.width.saturating_add(thickness.saturating_mul(2)),
        height: rect.height.saturating_add(thickness.saturating_mul(2)),
    }
}

/// Converts a length in logical pixels to overlay buffer pixels, rounding up so
/// a configured border is never thinner than requested.
fn buffer_length(value: i32, buffer_extent: i32, logical_extent: i32) -> i32 {
    if buffer_extent <= 0 || logical_extent <= 0 {
        return value.max(1);
    }
    let scaled = (i64::from(value) * i64::from(buffer_extent) + i64::from(logical_extent) - 1)
        / i64::from(logical_extent);
    i32::try_from(scaled).unwrap_or(i32::MAX).max(1)
}

/// One closed, outside-only ring for both frozen selection and live capture.
fn selection_border_pixel(x: i32, y: i32, rect: LogicalRect, thickness: i32, color: [u8; 4]) -> Option<[u8; 4]> {
    let outer = border_bounds(rect, thickness);
    let inside = |r: LogicalRect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
    (!rect.is_empty() && inside(outer) && !inside(rect)).then_some(color)
}

/// The visible box follows the current rectangle. The rendered hole may be
/// larger during an edit drag so an in-flight capture remains clean.
fn ui_render_border_rect(state: &UiState) -> Option<LogicalRect> {
    ui_render_border(state).then(|| state.drag.current_rect()).flatten()
}

/// Moves the whole box so the grab point stays under the pointer, clamped to
/// the output bounds. Size never changes.
pub fn apply_box_move(
    rect: LogicalRect,
    offset_x: i32,
    offset_y: i32,
    pointer_x: i32,
    pointer_y: i32,
    bounds: LogicalRect,
) -> LogicalRect {
    let width = rect.width.min(bounds.width);
    let height = rect.height.min(bounds.height);
    let new_x = (pointer_x - offset_x).clamp(bounds.x, bounds.right() - width);
    let new_y = (pointer_y - offset_y).clamp(bounds.y, bounds.bottom() - height);
    LogicalRect { x: new_x, y: new_y, width, height }
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
    pub fn new_long(border_width: i32) -> Result<Self, Box<dyn Error>> {
        Self::new_internal(None, None, true, border_width)
    }

    pub fn with_frozen_pair(
        frozen_outputs: &[CapturedOutput],
        frozen_outputs_without_cursor: &[CapturedOutput],
        border_width: i32,
    ) -> Result<Self, Box<dyn Error>> {
        Self::new_internal(Some(frozen_outputs), Some(frozen_outputs_without_cursor), false, border_width)
    }

    fn new_internal(
        frozen_outputs: Option<&[CapturedOutput]>,
        frozen_outputs_without_cursor: Option<&[CapturedOutput]>,
        long_mode: bool,
        border_width: i32,
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
            viewporter: None,
            fractional_scale_manager: None,
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
            stitch_warning: false,
            pending_render: None,
            editing: false,
            edit_drag: None,
            capture_pending: None,
            capture_invalid: false,
            selection_anchor: None,
            border_width: border_width.clamp(crate::config::BORDER_WIDTH_MIN, crate::config::BORDER_WIDTH_MAX),
            live_state_seq: 1,
        render_scope: None,
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
        create_overlays(&mut state, &qh)?;
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

    /// Whether an edit-mode drag (edge resize or box move) is currently in
    /// progress. The capture loop pauses frame requests while this is true:
    /// intermediate box geometry must not be captured, only the settled
    /// rectangle after release.
    pub fn edit_drag_active(&self) -> bool {
        self.state.edit_drag.is_some()
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
        self.state.edit_drag = None;
        self.state.capture_pending = None;
        self.state.capture_invalid = false;
        // Long capture is single-screen: from here on, rendering and the
        // pending-render debt are scoped to the selected output only.
        self.state.render_scope = Some(viewport.regions[0].output_global_name);
        // After L promotion, future layer configure events must not rebuild
        // a frozen render cache over the live capture hole.
        self.state.frozen_outputs.clear();
        self.state.frozen_outputs_without_cursor.clear();
        for overlay in &mut self.state.overlays {
            overlay.render_cache = None;
            overlay.base_pixels = None;
            overlay.base_key = None;
        }
        // Retry until at least one overlay is rendered (buffer available).
        // Scoped to the selected output: long capture is single-screen.
        let scope = self.state.render_scope;
        for _ in 0..50 {
            self.event_queue.dispatch_pending(&mut self.state)?;
            if render_live_overlays(&mut self.state, scope) > 0 {
                break;
            }
            self.dispatch_poll(10)?;
        }
        set_overlay_keyboard_exclusive(&mut self.state);
        set_overlay_pointer_passthrough(&mut self.state, &qh)?;
        #[cfg(feature = "trace-logs")]
        for overlay in &self.state.overlays {
            crate::trace_log!(
                "long-start: output={} cache={} buffers={} available={}",
                overlay.output_name,
                overlay.render_cache.is_some(),
                overlay.buffers.len(),
                overlay.buffers.iter().filter(|b| b.available).count(),
            );
        }
        self.event_queue.dispatch_pending(&mut self.state)?;
        if let Err(err) = self.conn.flush() {
            // EAGAIN only means the socket accepted partial data; the rest
            // goes out with the next dispatch. Only real I/O errors are fatal.
            match err {
                wayland_client::backend::WaylandError::Io(io)
                    if io.kind() == std::io::ErrorKind::WouldBlock => {}
                other => return Err(other.into()),
            }
        }
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

    /// Turns the long-capture border red while the stitcher is refusing
    /// content, and back to normal once it places a frame again.
    ///
    /// The preview is deliberately left alone: it keeps showing what has
    /// already been stitched, so the warning reads as "new content is not being
    /// added" rather than "your capture is gone".
    pub fn set_stitch_warning(&mut self, warning: bool) -> Result<(), Box<dyn Error>> {
        if self.state.stitch_warning == warning {
            return Ok(());
        }
        self.state.stitch_warning = warning;
        self.state.live_state_seq = self.state.live_state_seq.wrapping_add(1);
        self.state.pending_render = Some(true);
        render_live_overlays(&mut self.state, None);
        self.conn.flush()?;
        Ok(())
    }

    pub fn stitch_warning(&self) -> bool {
        self.state.stitch_warning
    }

    pub fn update_long_capture_preview(&mut self, snapshot: LongPreviewSnapshot) -> Result<(), Box<dyn Error>> {
        self.state.long_preview = Some(snapshot);
        // New preview content must repaint the overlay even when the selection
        // rectangle is unchanged; buffers recorded the previous seq.
        self.state.live_state_seq = self.state.live_state_seq.wrapping_add(1);
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

    /// Commit a clean overlay on the selected output, then synchronize this
    /// connection before the worker requests capture on its own connection.
    /// Return the exact selection used. Subsequent edits invalidate that frame.
    pub fn prepare_capture_clean(&mut self) -> Result<Option<LogicalRect>, Box<dyn Error>> {
        for attempt in 0..6 {
            self.event_queue.dispatch_pending(&mut self.state)?;
            if self.state.edit_drag.is_some() {
                crate::trace_log!("prepare: skip edit_drag active");
                return Ok(None);
            }
            if self.long_status() != LongSessionStatus::Running {
                crate::trace_log!("prepare: skip status={:?}", self.long_status());
                return Ok(None);
            }
            let Some(rect) = self.state.drag.selected_rect() else {
                crate::trace_log!("prepare: skip no selected_rect");
                return Ok(None);
            };
            let viewport = SelectedViewport::from_outputs(rect, &self.state.output_infos())?;
            if viewport.regions.len() != 1 {
                crate::trace_log!(
                    "prepare: skip spanning rect={:?} regions={} outputs={:?}",
                    rect,
                    viewport.regions.len(),
                    self.state.output_infos().iter().map(|o| (o.global_name, o.logical)).collect::<Vec<_>>()
                );
                return Ok(None);
            }
            let output_name = viewport.regions[0].output_global_name;
            self.state.capture_pending = Some(rect);
            self.state.capture_invalid = false;
            {
                let target = self.state.overlays.iter().find(|o| o.output_name == output_name);
                crate::trace_log!(
                    "prepare: target output={} cache={} buffers={} available={}",
                    output_name,
                    target.map_or(false, |o| o.render_cache.is_some()),
                    target.map_or(0, |o| o.buffers.len()),
                    target.map_or(0, |o| o.buffers.iter().filter(|b| b.available).count()),
                );
            }
            let rendered = render_live_overlays(&mut self.state, Some(output_name));
            if rendered > 0 {
                // flush alone does not order requests on different connections.
                // Events dispatched here may edit the box; invalidation is
                // sticky until completion, even if it moves back again.
                self.event_queue.roundtrip(&mut self.state)?;
                crate::trace_log!("prepare: ack capture output={} rect={:?}", output_name, rect);
                return Ok(Some(rect));
            }
            self.state.capture_pending = None;
            // All buffers busy; wait for a wl_buffer.release and retry.
            crate::trace_log!("prepare: no buffer rendered={} attempt={}", rendered, attempt);
            self.dispatch_poll(4)?;
        }
        crate::trace_log!("prepare: giving up after 6 attempts");
        Ok(None)
    }

    /// Return whether the completed frame is safe to stitch. A rectangle that
    /// moved away and back still rejects the frame via sticky invalidation.
    pub fn complete_capture(&mut self) -> bool {
        invalidate_changed_capture(&mut self.state);
        let valid = !self.state.capture_invalid;
        self.state.capture_invalid = false;
        if let Some(pending) = self.state.capture_pending.take() {
            if self.state.drag.current_rect() != Some(pending) {
                queue_render(&mut self.state, true);
            }
        }
        valid
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
        let fractional_scale = state
            .fractional_scale_manager
            .as_ref()
            .map(|manager| manager.get_fractional_scale(&surface, qh, output.info.global_name));
        state.overlays.push(OverlaySurface {
            output_name: output.info.global_name,
            viewport: None,
            fractional_scale,
            preferred_scale: 120,
            surface,
            layer_surface,
            buffers: Vec::new(),
            render_cache: None,
            base_pixels: None,
            base_key: None,
            // Until the first configure both sizes are the logical geometry;
            // a fractionally scaled output replaces the buffer size with the
            // frozen capture's pixel size in the configure handler.
            width: output.info.logical.width.max(1),
            height: output.info.logical.height.max(1),
            logical_width: output.info.logical.width.max(1),
            logical_height: output.info.logical.height.max(1),
            configured: false,
        });
    }
    Ok(())
}

fn render_overlays_full_dim(state: &mut UiState) -> usize {
    render_live_overlays(state, None)
}

fn render_live_overlays(state: &mut UiState, output_name: Option<u32>) -> usize {
    invalidate_changed_capture(state);
    let border_width = state.border_width;
    let border_color = border_color_for_state(state, state.drag.current_rect());
    let rect = ui_render_selection(state);
    let border_rect = ui_render_border_rect(state);
    let live_seq = state.live_state_seq;
    // "Satisfied" overlays are either freshly committed or already display
    // exactly the requested state. Returning satisfied (not merely committed)
    // lets steady-state prepare calls ack without touching the compositor —
    // the commit-on-change contract that keeps capture throughput independent
    // of compositor buffer-release timing.
    let mut satisfied = 0;
    for i in 0..state.overlays.len() {
        let overlay = &mut state.overlays[i];
        if output_name.is_some_and(|name| overlay.output_name != name) {
            continue;
        }
        if overlay.render_cache.is_some() {
            continue;
        }
        let Some(output) = state.outputs.iter().find(|o| o.info.global_name == overlay.output_name) else {
            continue;
        };
        let buffer = match overlay.buffers.iter().position(|b| b.available) {
            Some(pos) => &mut overlay.buffers[pos],
            None => continue,
        };
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.data, buffer.size) };
        let buf_size = overlay.width as usize * overlay.height as usize * 4;
        // Map through the actual overlay buffer size, not wl_output.scale:
        // our surfaces use buffer scale 1 even on a scaled output.
        let selection_local = rect.and_then(|r| selected_buffer_rect(output.info, r, overlay.width, overlay.height));
        let border_local = border_rect.and_then(|r| selected_buffer_rect(output.info, r, overlay.width, overlay.height));
        if buffer.initialized
            && buffer.last_selected == selection_local
            && buffer.live_seq == live_seq
        {
            // This buffer already displays exactly the requested state.
            satisfied += 1;
            continue;
        }

        let cache_key = (overlay.width, overlay.height, selection_local);
        let rebuild = overlay.base_key != Some(cache_key)
            || overlay.base_pixels.as_ref().map_or(true, |b| b.len() != buf_size);
        if rebuild {
            draw_live_base(data, overlay.width, overlay.height, selection_local);
            let cache = data[..buf_size].to_vec();
            overlay.base_pixels = Some(cache);
            overlay.base_key = Some(cache_key);
        } else if let Some(ref base) = overlay.base_pixels {
            data[..buf_size.min(base.len())].copy_from_slice(&base[..buf_size.min(base.len())]);
        }

        draw_live_foreground(data, overlay.width, overlay.height, output.info.logical,
            selection_local, border_local, state.long_preview.as_ref(), border_width, border_color);
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay.surface.damage_buffer(0, 0, overlay.width, overlay.height);
        overlay.surface.commit();
        crate::trace_log!("render: committed canvas output={} (buffer freed the previous one on niri side)", overlay.output_name);
        buffer.available = false;
        buffer.initialized = true;
        buffer.last_selected = selection_local;
        buffer.live_seq = live_seq;
        satisfied += 1;
    }
    satisfied
}

fn draw_selection_border(data: &mut [u8], _width: i32, _height: i32, x: i32, y: i32, w: i32, h: i32, thickness: i32, border: [u8; 4]) {
    let stride = _width as usize * 4;
    let rect = LogicalRect { x, y, width: w, height: h };
    let thickness = thickness.max(1);
    let x0 = x.saturating_sub(thickness).max(0);
    let y0 = y.saturating_sub(thickness).max(0);
    let x1 = x.saturating_add(w).saturating_add(thickness).min(_width);
    let y1 = y.saturating_add(h).saturating_add(thickness).min(_height);
    for py in y0..y1 {
        // Skip the interior span: border work is proportional to perimeter.
        let spans = if py >= y && py < y + h {
            [(x0, x.min(x1)), ((x + w).max(x0), x1)]
        } else {
            [(x0, x1), (0, 0)]
        };
        for (start, end) in spans {
            for px in start..end {
                if selection_border_pixel(px, py, rect, thickness, border).is_some() {
                    let offset = py as usize * stride + px as usize * 4;
                    data[offset..offset + 4].copy_from_slice(&border);
                }
            }
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

/// Whether a finished long-mode selection must be discarded.
///
/// Long capture is single-screen by construction, so a box that spans outputs
/// is not a selection — the red border says so. Clamping it to one output would
/// silently capture something the user did not select, so the box is dropped
/// instead and the user drags again.
fn long_selection_must_be_discarded(long_mode: bool, rect: Option<LogicalRect>, infos: &[OutputInfo]) -> bool {
    long_mode && rect.is_some_and(|rect| crate::geometry::rect_spans_outputs(rect, infos))
}

/// Applies [`long_selection_must_be_discarded`] to the current state.
fn discard_spanning_long_selection(state: &mut UiState) -> bool {
    let infos = state.output_infos();
    if !long_selection_must_be_discarded(state.long_mode, state.drag.selected_rect(), &infos) {
        return false;
    }
    discard_selection(state);
    true
}

/// Drops the selection as if the drag had never happened.
///
/// Long capture is single-screen, so a box spanning outputs cannot become a
/// capture: clamping it to one output captures something the user did not
/// select, and the red border already told them so. Returning to the empty
/// selection state is the honest outcome — the user drags again.
fn discard_selection(state: &mut UiState) {
    state.drag = DragState::Idle;
    state.selection_anchor = None;
    invalidate_overlay_bases(state);
}

/// Drops cached overlay bases so the next render rebuilds them with the
/// current selection rectangle (used after selection-box edits).
fn invalidate_overlay_bases(state: &mut UiState) {
    for overlay in &mut state.overlays {
        overlay.base_pixels = None;
        overlay.base_key = None;
    }
}

/// Abandons the frozen screen images and switches every overlay to the live
/// render path, so the real screen shows through the selection hole
/// immediately. Called when long capture is requested: the user pressed L to
/// capture live content, and a frozen frame must not survive into the handover.
fn drop_frozen_bases(state: &mut UiState) {
    state.frozen_outputs.clear();
    state.frozen_outputs_without_cursor.clear();
    for overlay in &mut state.overlays {
        overlay.render_cache = None;
        overlay.base_pixels = None;
        overlay.base_key = None;
    }
    queue_render(state, true);
}

fn digit_key_index(key: u32) -> Option<usize> {
    if (KEY_DIGIT_FIRST..=KEY_DIGIT_LAST).contains(&key) {
        Some((key - KEY_DIGIT_FIRST) as usize)
    } else {
        None
    }
}

/// Decisions the keyboard can make over pure selection state. The Wayland
/// dispatch impl turns these into side effects; keeping the decision pure
/// makes the modal transitions unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Cancel,
    ToggleEdit,
    PromoteLong,
    ToggleCursorFrame,
    /// Enter/Space: leave edit mode (confirming the adjusted rectangle) and
    /// finish a running long capture.
    ConfirmSelection,
    HintDirection(SearchDirection),
    CaptureOutputByIndex(usize),
    Ignore,
}

fn resolve_key_action(state: &UiState, key: u32) -> KeyAction {
    if key == KEY_ESC {
        KeyAction::Cancel
    } else if key == KEY_E {
        if state.drag.current_rect().is_some() {
            KeyAction::ToggleEdit
        } else {
            KeyAction::Ignore
        }
    } else if key == KEY_L {
        KeyAction::PromoteLong
    } else if key == KEY_H && !state.long_mode {
        KeyAction::ToggleCursorFrame
    } else if (key == KEY_ENTER || key == KEY_SPACE)
        && (state.editing || (state.long_mode && state.drag.is_finished()))
    {
        KeyAction::ConfirmSelection
    } else if state.long_mode && state.drag.is_finished() {
        match key {
            KEY_DOWN => KeyAction::HintDirection(SearchDirection::Down),
            KEY_UP => KeyAction::HintDirection(SearchDirection::Up),
            KEY_LEFT => KeyAction::HintDirection(SearchDirection::Left),
            KEY_RIGHT => KeyAction::HintDirection(SearchDirection::Right),
            _ => KeyAction::Ignore,
        }
    } else if !state.long_mode && state.drag == DragState::Idle {
        digit_key_index(key).map_or(KeyAction::Ignore, KeyAction::CaptureOutputByIndex)
    } else {
        KeyAction::Ignore
    }
}

/// Overlay pointer-input change requested by an applied key action; the
/// dispatcher performs the actual Wayland input-region update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayInputMode {
    Interactive,
    Passthrough,
}

fn apply_key_action(state: &mut UiState, action: KeyAction) -> Option<OverlayInputMode> {
    match action {
        KeyAction::Cancel => {
            state.editing = false;
            state.edit_drag = None;
            state.drag = DragState::Cancelled;
            None
        }
        KeyAction::ToggleEdit => toggle_edit_mode(state),
        KeyAction::PromoteLong => {
            state.editing = false;
            state.edit_drag = None;
            state.long_requested = true;
            state.long_mode = true;
            if state.drag.is_finished() {
                state.drag.finish(state.pointer_x, state.pointer_y);
            }
            // Same anchor rule as release: a promoted spanning selection
            // belongs to the output the drag started on.
            if let Some(rect) = state.drag.selected_rect() {
                if selection_spans_outputs(state, rect) {
                    let clamped = state
                        .selection_anchor
                        .map(|(ax, ay)| crate::geometry::clamp_rect_to_output_at_point(
                            rect,
                            &state.output_infos(),
                            ax,
                            ay,
                        ))
                        .unwrap_or_else(|| crate::geometry::clamp_rect_to_largest_output(
                            rect,
                            &state.output_infos(),
                        ));
                    state.drag = DragState::Finished(clamped);
                }
            }
            // Drop the frozen images on the key press itself, not when the
            // long-capture session starts. Until this point the overlays show
            // a frozen frame; the user pressed L to capture live content and
            // should see the real screen (through the selection hole) at once,
            // with no frozen frame lingering during the session handover.
            drop_frozen_bases(state);
            None
        }
        KeyAction::ToggleCursorFrame => {
            toggle_frozen_frame(state);
            None
        }
        KeyAction::ConfirmSelection => {
            state.editing = false;
            state.edit_drag = None;
            if state.long_mode && state.drag.is_finished() {
                state.long_finish_requested = true;
            }
            None
        }
        KeyAction::HintDirection(direction) => {
            state.long_direction = Some(direction);
            None
        }
        KeyAction::CaptureOutputByIndex(index) => {
            if let Some(rect) = full_output_rect_by_index(&state.output_infos(), index) {
                state.drag = DragState::Finished(rect);
            }
            None
        }
        KeyAction::Ignore => None,
    }
}

/// Toggles selection-box edit mode (E key). Entering edit mode finalizes an
/// in-progress drag into an adjustable rectangle; in long mode the overlay
/// must grab (or release) pointer input, which also blocks wheel scroll.
/// Returns the required overlay input mode in long mode, `None` otherwise.
fn toggle_edit_mode(state: &mut UiState) -> Option<OverlayInputMode> {
    if state.editing {
        state.editing = false;
        state.edit_drag = None;
        let mode = state.long_mode.then_some(OverlayInputMode::Passthrough);
        invalidate_overlay_bases(state);
        render_overlays(state);
        mode
    } else {
        if let DragState::Dragging { .. } = state.drag {
            let (x, y) = state
                .pointer_global_position()
                .unwrap_or((state.pointer_x, state.pointer_y));
            state.drag.finish(x, y);
        }
        // Same rule as a release: a long-mode box that spans outputs is not a
        // selection, so E discards it rather than opening an edit mode on a box
        // that can never be captured.
        if discard_spanning_long_selection(state) {
            render_overlays(state);
            return None;
        }
        state.editing = true;
        state.edit_drag = None;
        let mode = state.long_mode.then_some(OverlayInputMode::Interactive);
        invalidate_overlay_bases(state);
        render_overlays(state);
        mode
    }
}

/// Bounds the selection box may be dragged within: the union of every output.
///
/// The box is deliberately *not* confined to the output it was captured on.
/// A long-mode box that crosses outputs turns red and is reverted on release
/// (`settled_edit_rect`), which tells the user why the drag was refused;
/// clamping at the output edge instead looks like the pointer stopped working.
fn selection_drag_bounds(infos: &[OutputInfo]) -> Option<LogicalRect> {
    infos
        .iter()
        .map(|info| info.logical)
        .reduce(|left, right| LogicalRect {
            x: left.x.min(right.x),
            y: left.y.min(right.y),
            width: left.right().max(right.right()) - left.x.min(right.x),
            height: left.bottom().max(right.bottom()) - left.y.min(right.y),
        })
}

/// The rect an edit drag settles on.
///
/// A long-mode box that would end up spanning outputs is not a selection (long
/// capture is single-screen), so the drag is undone: the box goes back to where
/// it was when the drag started, exactly as if the drag had never happened.
fn settled_edit_rect(long_mode: bool, dragged: LogicalRect, origin: LogicalRect, infos: &[OutputInfo]) -> LogicalRect {
    if long_mode && crate::geometry::rect_spans_outputs(dragged, infos) {
        origin
    } else {
        dragged
    }
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
        overlay.render_cache = Some(build_overlay_render_cache(width, height, output.info, frozen, state.border_width));
        for buffer in &mut overlay.buffers {
            buffer.initialized = false;
            buffer.last_selected = None;
        }
    }
    queue_render(state, false);
}

fn queue_render(state: &mut UiState, full: bool) {
    // Long capture is single-screen: scope the render (and the pending debt)
    // to the selected output. The other outputs' overlays are static frozen
    // images; counting them here would make the debt clearable only when
    // every output's canvas turns over in one pass, which per-output release
    // timing never guarantees.
    let (committed, expected) = match state.render_scope {
        Some(name) => (
            render_live_overlays(state, Some(name)),
            state.overlays.iter().filter(|o| o.output_name == name).count(),
        ),
        None => {
            let committed = if full {
                render_overlays_full_dim(state)
            } else {
                render_overlays(state)
            };
            (committed, state.overlays.len())
        }
    };
    state.pending_render = if committed < expected { Some(full) } else { None };
}

fn flush_pending_render(state: &mut UiState) {
    let Some(full) = state.pending_render else { return; };
    let (committed, expected) = match state.render_scope {
        Some(name) => (
            render_live_overlays(state, Some(name)),
            state.overlays.iter().filter(|o| o.output_name == name).count(),
        ),
        None => {
            let committed = if full {
                render_overlays_full_dim(state)
            } else {
                render_overlays(state)
            };
            (committed, state.overlays.len())
        }
    };
    crate::trace_log!(
        "flush_pending: full={} committed={}/{} (scope={:?}) -> pending={}",
        full,
        committed,
        expected,
        state.render_scope,
        if committed >= expected { "None" } else { "Some" }
    );
    if committed >= expected {
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

fn render_overlays(state: &mut UiState) -> usize {
    let mut committed = 0;
    let rect = ui_render_selection(state);
    let border_width = state.border_width;
    // The frozen path must honour the same spans/stall warning colour as the
    // live path; it used to hardcode white, which is why a drag across outputs
    // kept the plain border until long mode started.
    let border = border_color_for_state(state, state.drag.current_rect());
    for overlay in &mut state.overlays {
        // Live overlays always use the same compositor, including edit/input
        // redraws; the frozen-image dirty-region path is normal-mode only.
        let Some(render_cache) = overlay.render_cache.as_ref() else {
            continue;
        };
        let Some(buffer) = overlay.buffers.iter_mut().find(|buffer| buffer.available) else {
            continue;
        };
        let Some(output) = state.outputs.iter().find(|output| output.info.global_name == overlay.output_name) else {
            continue;
        };
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.data, buffer.size) };
        let current_selected = rect.and_then(|selected| selected_buffer_rect(output.info, selected, overlay.width, overlay.height));
        // The border is drawn in logical pixels; its dirty ring must be the
        // same ring measured in this overlay's buffer pixels.
        let border_buffer_width = buffer_length(border_width, overlay.width, output.info.logical.width);
        let damage = if buffer.initialized {
            dirty_selected_region(
                buffer.last_selected.map(|rect| border_bounds(rect, border_buffer_width)),
                current_selected.map(|rect| border_bounds(rect, border_buffer_width)),
                overlay.width, overlay.height)
        } else {
            Some(LogicalRect { x: 0, y: 0, width: overlay.width, height: overlay.height })
        };
        let Some(damage) = damage else {
            continue;
        };
        if buffer.initialized {
            draw_overlay_region(data, render_cache, output.info, rect, damage, border_width, border);
        } else {
            draw_overlay(data, render_cache, output.info, rect, border_width, border);
            buffer.initialized = true;
        }
        buffer.last_selected = current_selected;
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay.surface.damage_buffer(damage.x, damage.y, damage.width, damage.height);
        overlay.surface.commit();
        buffer.available = false;
        committed += 1;
    }
    committed + render_live_overlays(state, None)
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

fn frozen_pixel(frozen: &FrozenOutput, overlay_width: i32, overlay_height: i32, local_x: i32, local_y: i32) -> [u8; 4] {
    if frozen.width <= 0 || frozen.height <= 0 || frozen.stride <= 0 || overlay_width <= 0 || overlay_height <= 0 {
        return [0, 0, 0, 255];
    }

    // The overlay buffer is sized to the frozen capture, so this is an identity
    // whenever the output's scale could be honoured (fractional or integer);
    // it only resamples on the fallback path, where the buffer is logical.
    let source_x = ((local_x as i64 * frozen.width as i64) / overlay_width as i64) as i32;
    let source_y = ((local_y as i64 * frozen.height as i64) / overlay_height as i64) as i32;
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

fn build_overlay_render_cache(width: i32, height: i32, output: OutputInfo, frozen: &FrozenOutput, border_width: i32) -> OverlayRenderCache {
    let buffer_width = width.max(0) as usize;
    let buffer_height = height.max(0) as usize;
    let len = buffer_width.saturating_mul(buffer_height).saturating_mul(4);
    let mut base = vec![0; len];
    let mut dimmed = vec![0; len];
    for local_y in 0..buffer_height {
        for local_x in 0..buffer_width {
            let offset = (local_y * buffer_width + local_x) * 4;
            let pixel = frozen_pixel(frozen, width, height, local_x as i32, local_y as i32);
            let logical_x = (local_x as i64 * output.logical.width as i64 / width.max(1) as i64) as i32;
            let logical_y = (local_y as i64 * output.logical.height as i64 / height.max(1) as i64) as i32;
            base[offset..offset + 4].copy_from_slice(&pixel);
            dimmed[offset..offset + 4].copy_from_slice(&composited_overlay_pixel(pixel, output.logical.x + logical_x, output.logical.y + logical_y, None, border_width));
        }
    }
    OverlayRenderCache { width, height, base, dimmed }
}

fn composited_overlay_pixel(base: [u8; 4], global_x: i32, global_y: i32, selected: Option<LogicalRect>, border_width: i32) -> [u8; 4] {
    let overlay = overlay_pixel(global_x, global_y, selected, border_width, OVERLAY_BORDER_WHITE_BGRA);
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

fn draw_overlay(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, selected: Option<LogicalRect>, border_width: i32, border: [u8; 4]) {
    draw_overlay_region(data, cache, output, selected, LogicalRect { x: 0, y: 0, width: cache.width, height: cache.height }, border_width, border);
}

fn draw_overlay_region(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, selected: Option<LogicalRect>, dirty: LogicalRect, border_width: i32, border: [u8; 4]) {
    if cache.width <= 0 || cache.height <= 0 {
        return;
    }
    let width = cache.width as usize;
    // Buffer pixels are not logical pixels on a fractionally scaled output:
    // the buffer is the frozen capture's pixel grid, the selection lives in
    // global logical coordinates. Map before comparing, otherwise the hole and
    // the border land at `scale` times their position.
    let logical = output.logical;
    let to_global = |local: i32, origin: i32, logical_extent: i32, buffer_extent: i32| {
        origin + (i64::from(local) * i64::from(logical_extent.max(1)) / i64::from(buffer_extent.max(1))) as i32
    };
    for local_y in dirty.y as usize..dirty.bottom() as usize {
        for local_x in dirty.x as usize..dirty.right() as usize {
            let global_x = to_global(local_x as i32, logical.x, logical.width, cache.width);
            let global_y = to_global(local_y as i32, logical.y, logical.height, cache.height);
            let offset = (local_y * width + local_x) * 4;
            let source = match overlay_pixel(global_x, global_y, selected, border_width, border) {
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

fn overlay_pixel(global_x: i32, global_y: i32, selected: Option<LogicalRect>, border_width: i32, border: [u8; 4]) -> [u8; 4] {
    let Some(rect) = selected else {
        return OVERLAY_OUTSIDE_MASK_BGRA;
    };
    if let Some(border) = selection_border_pixel(global_x, global_y, rect, border_width, border) {
        return border;
    }
    if global_x < rect.x || global_x >= rect.right() || global_y < rect.y || global_y >= rect.bottom() {
        return OVERLAY_OUTSIDE_MASK_BGRA;
    }

    OVERLAY_SELECTED_TRANSPARENT_BGRA
}

/// Maps a selection rectangle into this overlay's buffer pixels: intersect
/// with the output's real logical geometry (the selection lives in global
/// logical coordinates), then map to the actual buffer dimensions. A pseudo
/// `(0,0,buffer)` rect would misplace or hide the box on outputs that are
/// not at the logical origin or have a scale factor.
fn selected_buffer_rect(output: OutputInfo, selected: LogicalRect, buffer_width: i32, buffer_height: i32) -> Option<LogicalRect> {
    let local = selected_local_intersection(output.logical, selected)?;
    let map_x = |x: i32| (i64::from(x) * i64::from(buffer_width) / i64::from(output.logical.width.max(1))) as i32;
    let map_y = |y: i32| (i64::from(y) * i64::from(buffer_height) / i64::from(output.logical.height.max(1))) as i32;
    LogicalRect {
        x: map_x(local.x),
        y: map_y(local.y),
        width: map_x(local.right()) - map_x(local.x),
        height: map_y(local.bottom()) - map_y(local.y),
    }
    .intersection(LogicalRect { x: 0, y: 0, width: buffer_width, height: buffer_height })
}

#[cfg(test)]
fn draw_overlay_dim_with_selection(
    data: &mut [u8],
    width: i32,
    height: i32,
    output: OutputInfo,
    selected: Option<LogicalRect>,
    border_width: i32,
    border: [u8; 4],
    draw_border: bool,
) {
    let local = selected.and_then(|sel| selected_buffer_rect(output, sel, width, height));
    draw_live_base(data, width, height, local);
    draw_live_foreground(data, width, height, output.logical, local, local.filter(|_| draw_border), None, border_width, border);
}

fn draw_live_base(data: &mut [u8], width: i32, height: i32, hole: Option<LogicalRect>) {
    let stride = width.max(0) as usize * 4;
    for y in 0..height.max(0) as usize {
        for x in 0..width.max(0) as usize {
            let offset = y * stride + x * 4;
            data[offset..offset + 4].copy_from_slice(&OVERLAY_OUTSIDE_MASK_BGRA);
        }
    }
    if let Some(local) = hole {
        for y in local.y..local.y + local.height {
            for x in local.x..local.x + local.width {
                let offset = (y as usize) * stride + (x as usize) * 4;
                data[offset..offset + 4].copy_from_slice(&OVERLAY_SELECTED_TRANSPARENT_BGRA);
            }
        }
    }
}

/// Live compositing order: cached dim/hole base → stitched preview → border.
/// Border geometry follows the current box, independently of capture exclusion.
fn draw_live_foreground(
    data: &mut [u8], width: i32, height: i32, output: LogicalRect,
    hole: Option<LogicalRect>, border_rect: Option<LogicalRect>, preview: Option<&LongPreviewSnapshot>,
    border_width: i32, border: [u8; 4],
) {
    if let Some(preview) = preview {
        draw_preview_on_overlay(data, width, height, preview, output, hole);
    }
    if let Some(local) = border_rect {
        // The box is already in buffer pixels; its thickness is configured in
        // logical pixels, so it has to be converted the same way.
        let thickness = buffer_length(border_width, width, output.width);
        draw_selection_border(data, width, height, local.x, local.y, local.width, local.height, thickness, border);
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

fn draw_preview_on_overlay(data: &mut [u8], overlay_w: i32, overlay_h: i32, preview: &LongPreviewSnapshot, output_logical: LogicalRect, selection_local: Option<LogicalRect>) {
    let pw = preview.image.width as i32;
    let ph = preview.image.height as i32;
    if pw == 0 || ph == 0 || overlay_w <= 0 || overlay_h <= 0 {
        return;
    }
    let logical_w = output_logical.width.max(1);
    let logical_h = output_logical.height.max(1);
    let vp = preview.viewport_rect;
    let cw = preview.capture_width.max(1) as i32;
    let ch = preview.capture_height.max(1) as i32;
    let origin_x = preview.current_origin_x;
    let origin_y = preview.current_origin_y;
    let buf_stride = overlay_w as usize * 4;
    let hole = selection_local.unwrap_or(LogicalRect { x: 0, y: 0, width: 0, height: 0 });
    let img_data = &preview.image.data;
    let img_stride = preview.image.stride as usize;

    // Buffer pixel -> stitched pixel, in one step.
    //
    // The buffer is at the content's own density (`overlay_buffer_size`), so
    // this is an identity whenever the capture's pixel grid is the buffer's.
    // Mapping through global logical coordinates instead — buffer to logical,
    // logical to capture — quantises the phase to logical pixels and then
    // re-expands it, which samples the canvas at 1x and draws it at the
    // buffer's density: a 1.5x upscale of a 1x sample, i.e. a visibly soft
    // preview on any fractionally scaled output.
    let buffer_x = |logical: i32| -> i64 {
        (i64::from(logical - output_logical.x) * i64::from(overlay_w)) / i64::from(logical_w)
    };
    let buffer_y = |logical: i32| -> i64 {
        (i64::from(logical - output_logical.y) * i64::from(overlay_h)) / i64::from(logical_h)
    };
    let vp_buffer_x = buffer_x(vp.x);
    let vp_buffer_y = buffer_y(vp.y);
    let vp_buffer_w = (buffer_x(vp.right()) - vp_buffer_x).max(1);
    let vp_buffer_h = (buffer_y(vp.bottom()) - vp_buffer_y).max(1);
    // Canvas pixels per buffer pixel: the capture's density over the buffer's.
    // One when they agree, which is the normal case and needs no resampling.
    let canvas_x = |bx: i32| -> i64 {
        i64::from(origin_x)
            + (i64::from(bx) - vp_buffer_x) * i64::from(cw) / vp_buffer_w
    };
    let canvas_y = |by: i32| -> i64 {
        i64::from(origin_y)
            + (i64::from(by) - vp_buffer_y) * i64::from(ch) / vp_buffer_h
    };

    // Buffer-major: every buffer pixel of the dimmed area picks the stitched
    // pixel it covers. Iterating the stitched image instead would leave gaps
    // as soon as the buffer is denser than the logical grid (fractional scale),
    // and the live screen would show through them.
    let columns: Vec<Option<usize>> = (0..overlay_w)
        .map(|bx| {
            let sx = canvas_x(bx);
            (sx >= 0 && sx < i64::from(pw)).then(|| sx as usize * 4)
        })
        .collect();
    for by in 0..overlay_h {
        let in_hole_row = by >= hole.y && by < hole.y + hole.height;
        let sy = canvas_y(by);
        if sy < 0 || sy >= i64::from(ph) {
            continue;
        }
        let src_row = sy as usize * img_stride;
        let dst_row = by as usize * buf_stride;
        for (bx, column) in columns.iter().enumerate() {
            let Some(src_column) = column else {
                continue;
            };
            if in_hole_row && (bx as i32) >= hole.x && (bx as i32) < hole.x + hole.width {
                continue;
            }
            let src_off = src_row + *src_column;
            if src_off + 3 >= img_data.len() {
                continue;
            }
            // Never-captured corners of ragged (T/L/arch) canvases stay
            // transparent; leave the dimmed overlay visible beneath them.
            if img_data[src_off + 3] == 0 {
                continue;
            }
            let dst_off = dst_row + bx * 4;
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


/// The output's pixel size from its mode, when the mode describes the same
/// shape as the logical size.
///
/// A rotated output reports the *unrotated* mode (1920x1080 for a 1080x1920
/// logical size), so the mode is not the buffer's shape there — using it would
/// squash the overlay through the viewport. The logical size wins in that case,
/// which costs sharpness but keeps the geometry right.
fn mode_pixels_for(mode: Option<(i32, i32)>, logical: (i32, i32)) -> Option<(i32, i32)> {
    let (mode_width, mode_height) = mode?;
    let (logical_width, logical_height) = logical;
    if mode_width <= 0 || mode_height <= 0 || logical_width <= 0 || logical_height <= 0 {
        return None;
    }
    let lhs = i64::from(mode_width) * i64::from(logical_height);
    let rhs = i64::from(mode_height) * i64::from(logical_width);
    // 1% tolerance: xdg_output rounds the logical size, so the cross products
    // of a genuinely unrotated output are equal only up to that rounding.
    if (lhs - rhs).abs() * 100 > lhs.max(rhs) {
        return None;
    }
    Some((mode_width, mode_height))
}

/// Overlay buffer size in pixels.
///
/// Hard lower bounds, in the order they are known:
///
/// - the frozen image that is about to be displayed (we must not resample our
///   own screenshot before the compositor shows it);
/// - when there is no frozen image yet — a `--long` session starts live — the
///   output's current mode, which is the grid the compositor scans out and the
///   density the live capture will arrive at;
/// - the output's density from `wp_fractional_scale_v1`, when the compositor
///   advertises it (180 = 1.5), otherwise the compositor upscales the overlay.
///
/// The capture wins over the mode: it is what we display, so its grid is the
/// one to match. Without `wp_viewporter` a buffer larger than the surface
/// cannot be mapped back, so the logical size wins.
fn overlay_buffer_size(
    logical: (i32, i32),
    capture: Option<(i32, i32)>,
    output_pixels: Option<(i32, i32)>,
    preferred_scale: i32,
    viewport_available: bool,
) -> (i32, i32) {
    let logical_width = logical.0.max(1);
    let logical_height = logical.1.max(1);
    if !viewport_available {
        return (logical_width, logical_height);
    }
    let numerator = i64::from(preferred_scale.max(1));
    let hinted = |extent: i32| (i64::from(extent) * numerator + 119) / 120;
    let (capture_width, capture_height) = capture.or(output_pixels).unwrap_or((0, 0));
    (
        hinted(logical_width)
            .max(i64::from(capture_width))
            .max(i64::from(logical_width)) as i32,
        hinted(logical_height)
            .max(i64::from(capture_height))
            .max(i64::from(logical_height)) as i32,
    )
}

/// Recomputes one overlay's buffer size, render cache and viewport.
///
/// Both inputs change independently and either may arrive first: the logical
/// size comes from the layer-surface configure, the pixel density from
/// `wp_fractional_scale_v1.preferred_scale`, and the frozen capture size from
/// the screenshot that was taken before the selection started.
fn refresh_overlay_surface(state: &mut UiState, output_name: u32, qh: &QueueHandle<UiState>) {
    let Some(output) = state
        .outputs
        .iter()
        .find(|output| output.info.global_name == output_name)
    else {
        return;
    };
    let output_info = output.info;
    let output_pixels = mode_pixels_for(
        output.mode_pixels,
        (output.info.logical.width, output.info.logical.height),
    );
    let frozen = (if state.screenshot_cursor_hidden {
        &state.frozen_outputs_without_cursor
    } else {
        &state.frozen_outputs
    })
    .iter()
    .find(|frozen| frozen.info.global_name == output_name);
    let shm = state.shm.clone();
    let viewporter = state.viewporter.clone();
    let border_width = state.border_width;
    let Some(index) = state.overlays.iter().position(|overlay| overlay.output_name == output_name) else {
        return;
    };
    // A `preferred_scale` that beats the layer configure is recorded in the
    // field and applied here once the configure supplies the logical size; it
    // must not create buffers, because a buffer attached before the first
    // configure is a protocol-order violation.
    if !state.overlays[index].configured {
        return;
    }
    let logical_width = state.overlays[index].logical_width.max(1);
    let logical_height = state.overlays[index].logical_height.max(1);
    let preferred_scale = state.overlays[index].preferred_scale;
    let (buffer_width, buffer_height) = overlay_buffer_size(
        (logical_width, logical_height),
        frozen.map(|frozen| (frozen.width, frozen.height)),
        output_pixels,
        preferred_scale,
        viewporter.is_some(),
    );
    let resized = (buffer_width, buffer_height)
        != (state.overlays[index].width, state.overlays[index].height)
        || state.overlays[index].buffers.is_empty();
    if resized {
        let Some(shm) = shm else {
            return;
        };
        match create_overlay_buffers(&shm, output_name, buffer_width, buffer_height, qh) {
            Ok(buffers) => {
                let overlay = &mut state.overlays[index];
                overlay.buffers = buffers;
                overlay.base_pixels = None;
                overlay.base_key = None;
                overlay.render_cache = None;
                overlay.width = buffer_width;
                overlay.height = buffer_height;
                crate::trace_log!(
                    "overlay: buffers {}x{} output={} (logical {}x{}, preferred {})",
                    buffer_width,
                    buffer_height,
                    output_name,
                    logical_width,
                    logical_height,
                    preferred_scale
                );
            }
            Err(err) => {
                crate::trace_log!("overlay: buffers FAILED output={} err={}", output_name, err);
                state.drag = DragState::Cancelled;
                return;
            }
        }
    }
    if let Some(frozen) = frozen {
        let cache_matches = state.overlays[index]
            .render_cache
            .as_ref()
            .is_some_and(|cache| cache.width == buffer_width && cache.height == buffer_height);
        if !cache_matches {
            state.overlays[index].render_cache = Some(build_overlay_render_cache(
                buffer_width,
                buffer_height,
                output_info,
                frozen,
                border_width,
            ));
        }
    }
    // A viewport destination is surface state: it only takes effect on the next
    // commit. The frozen renderer commits only when there is damage, so a
    // reconfigure that changes the logical size without changing the buffer
    // size (a resolution change mid-selection) would otherwise keep the old
    // mapping until something else forces a redraw.
    let mut destination_set = false;
    match viewporter {
        Some(viewporter) if (buffer_width, buffer_height) != (logical_width, logical_height) => {
            let overlay = &mut state.overlays[index];
            if overlay.viewport.is_none() {
                overlay.viewport = Some(viewporter.get_viewport(&overlay.surface, qh, ()));
            }
            if let Some(viewport) = overlay.viewport.as_ref() {
                viewport.set_destination(logical_width, logical_height);
                destination_set = true;
            }
        }
        _ => {
            if let Some(viewport) = state.overlays[index].viewport.as_ref() {
                viewport.set_destination(0, 0);
                destination_set = true;
            }
        }
    }
    if destination_set {
        state.overlays[index].surface.commit();
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
    Ok(OverlayBuffer { size, data: data.cast(), _fd: fd, pool, buffer, available: true, initialized: false, last_selected: None, live_seq: 0 })
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
            "wp_viewporter" => state.viewporter = Some(registry.bind::<WpViewporter, _, _>(name, version.min(1), qh, ())),
            "wp_fractional_scale_manager_v1" => {
                state.fractional_scale_manager = Some(registry.bind::<WpFractionalScaleManagerV1, _, _>(name, version.min(1), qh, ()))
            }
            "wl_seat" => state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ())),
            "wl_output" => {
                let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);
                state.outputs.push(OutputRuntime {
                    info: OutputInfo { global_name: name, logical: LogicalRect { x: 0, y: 0, width: 1, height: 1 }, scale: 1 },
                    output,
                    xdg_output: None,
                    mode_pixels: None,
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
                if let Some(drag_mode) = state.edit_drag {
                    if let Some(rect) = state.drag.selected_rect() {
                        let bounds = selection_drag_bounds(&state.output_infos()).unwrap_or(rect);
                        let updated = match drag_mode {
                            EditDrag::Edge { edge, .. } => apply_edge_drag(rect, edge, x, y, bounds),
                            EditDrag::Move { offset_x, offset_y, .. } => {
                                apply_box_move(rect, offset_x, offset_y, x, y, bounds)
                            }
                        };
                        if updated != rect {
                            if let Some(current) = state.drag.finished_rect_mut() {
                                *current = updated;
                            }
                            invalidate_changed_capture(state);
                            // An in-flight frame prepared from a different
                            // rectangle is already invalid, so its region does
                            // not need to stay in the hole. Dropping it here is
                            // what lets the dim edge follow the box when the
                            // user shrinks: keeping the old region would hold
                            // the hole at the widest point of the drag.
                            drop_invalid_pending(state);
                            invalidate_overlay_bases(state);
                            queue_render(state, false);
                        }
                    }
                } else if state.drag.update(x, y) {
                    queue_render(state, false);
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
                            // Edges have snap targets only in edit mode; the
                            // box interior drags the whole box around.
                            state.edit_drag = state
                                .drag
                                .selected_rect()
                                .and_then(|rect| {
                                    hit_test_edge(rect, x, y, EDGE_SNAP_RANGE)
                                        .map(|edge| EditDrag::Edge { edge, origin: rect })
                                        .or_else(|| rect
                                            .intersection(LogicalRect { x, y, width: 1, height: 1 })
                                            .map(|_| EditDrag::Move {
                                                offset_x: x - rect.x,
                                                offset_y: y - rect.y,
                                                origin: rect,
                                            }))
                                });
                        } else if !state.editing {
                            state.drag.begin(x, y);
                            state.selection_anchor = Some((x, y));
                        }
                    }
                    wl_pointer::ButtonState::Released => {
                        if let Some(drag_mode) = state.edit_drag.take() {
                            // Finished one edit drag; stay in edit mode.
                            // capture_pending retains the old hole until the
                            // worker completes.
                            if let Some(rect) = state.drag.selected_rect() {
                                let settled = settled_edit_rect(
                                    state.long_mode,
                                    rect,
                                    drag_mode.origin(),
                                    &state.output_infos(),
                                );
                                if settled != rect {
                                    // The box went back where it started, so
                                    // nothing is captured from the box the
                                    // drag produced. The frame prepared while
                                    // it moved was already invalidated.
                                    crate::trace_log!(
                                        "edit: drag across outputs reverted to {:?}",
                                        settled
                                    );
                                    if let Some(current) = state.drag.finished_rect_mut() {
                                        *current = settled;
                                    }
                                }
                            }
                            invalidate_overlay_bases(state);
                        } else if !state.editing {
                            state.drag.finish(x, y);
                            // Long capture is single-screen, so a box spanning
                            // outputs is not a selection (that is what the red
                            // border says). Clamping it to one output would
                            // silently capture something the user did not
                            // select, so the drag is discarded instead and the
                            // user drags again.
                            discard_spanning_long_selection(state);
                        }
                    }
                    _ => {}
                }
                // Queue (rather than render directly): when one output's
                // buffers are all busy a direct render leaves that output
                // showing stale content — one screen red, the other white
                // during a spanning drag. queue_render re-renders on buffer
                // release.
                queue_render(state, false);
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
            let action = resolve_key_action(state, key);
            if matches!(action, KeyAction::Ignore) {
                return;
            }
            if let Some(mode) = apply_key_action(state, action) {
                match mode {
                    OverlayInputMode::Interactive => set_overlay_pointer_input_interactive(state),
                    OverlayInputMode::Passthrough => {
                        let _ = set_overlay_pointer_passthrough(state, qh);
                    }
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
            crate::trace_log!("configure: output={} size={}x{}", name, width, height);
            if state.shm.is_none() {
                state.drag = DragState::Cancelled;
                return;
            }
            let Some(overlay) = state.overlays.iter_mut().find(|overlay| overlay.output_name == *name) else {
                return;
            };
            overlay.logical_width = (width as i32).max(1);
            overlay.logical_height = (height as i32).max(1);
            overlay.configured = true;
            refresh_overlay_surface(state, *name, qh);
            render_overlays(state);
        }
    }
}

impl Dispatch<WlBuffer, BufferId> for UiState {
    fn event(state: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, id: &BufferId, _: &Connection, _: &QueueHandle<Self>) {
        crate::trace_log!("wl_buffer: release received output={} index={}", id.output_name, id.index);
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
            wl_output::Event::Mode { flags, width, height, .. } => {
                if matches!(flags, WEnum::Value(flags) if flags.contains(wl_output::Mode::Current)) {
                    output.mode_pixels = Some((width.max(1), height.max(1)));
                }
                // Without xdg_output the mode is the only size there is; it is
                // the pixel size, which is what this fallback always assumed.
                if output.info.logical.width <= 1 && output.info.logical.height <= 1 {
                    output.info.logical.width = width;
                    output.info.logical.height = height;
                }
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
delegate_noop!(UiState: ignore WpViewport);
delegate_noop!(UiState: ignore WpViewporter);
delegate_noop!(UiState: ignore WpFractionalScaleManagerV1);

impl Dispatch<WpFractionalScaleV1, u32> for UiState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        output_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let Some(overlay) = state.overlays.iter_mut().find(|overlay| overlay.output_name == *output_name) else {
            return;
        };
        let scale = scale.max(1) as i32;
        if overlay.preferred_scale == scale {
            return;
        }
        overlay.preferred_scale = scale;
        refresh_overlay_surface(state, *output_name, qh);
        queue_render(state, true);
    }
}
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
    fn overlay_buffer_size_never_shrinks_below_content_or_density() {
        // 1920x1080 output at 1.5 reports a 1280x720 logical size; the frozen
        // capture is 1920x1080 and the hint is 180 (120ths).
        assert_eq!(overlay_buffer_size((1280, 720), Some((1920, 1080)), Some((1920, 1080)), 180, true), (1920, 1080));
        // Same density with no capture yet (the live path): still 1.5x.
        assert_eq!(overlay_buffer_size((1280, 720), None, None, 180, true), (1920, 1080));
        assert_eq!(overlay_buffer_size((1280, 720), None, None, 150, true), (1600, 900));
        assert_eq!(overlay_buffer_size((1280, 720), None, None, 120, true), (1280, 720));
        // Long capture starts live: no frozen frame, and niri does not advertise
        // wp_fractional_scale_v1, so there is no hint either. The output's mode
        // is the grid the live capture arrives at, so the overlay is already at
        // the right density instead of being upscaled by the compositor.
        assert_eq!(overlay_buffer_size((1280, 720), None, Some((1920, 1080)), 120, true), (1920, 1080));
        // A capture is what we display, so its grid wins over the mode.
        assert_eq!(overlay_buffer_size((1280, 720), Some((1280, 720)), Some((1920, 1080)), 120, true), (1280, 720));
        // A capture denser than the hint wins: the image we display must never
        // be resampled on its way to the screen.
        assert_eq!(overlay_buffer_size((1280, 720), Some((2560, 1440)), None, 180, true), (2560, 1440));
        // Without wp_viewporter a larger buffer cannot be mapped back.
        assert_eq!(overlay_buffer_size((1280, 720), Some((1920, 1080)), None, 180, false), (1280, 720));
    }

    /// The mode is the density source only when it describes the same shape as
    /// the logical size; a rotated output reports the unrotated mode.
    #[test]
    fn the_output_mode_is_a_density_source_only_when_the_shape_matches() {
        // 1920x1080 mode, 1280x720 logical: 1.5.
        assert_eq!(mode_pixels_for(Some((1920, 1080)), (1280, 720)), Some((1920, 1080)));
        // Same shape with xdg_output rounding: 1.5 of 1281x721 is not integral.
        assert_eq!(mode_pixels_for(Some((1920, 1080)), (1281, 721)), Some((1920, 1080)));
        // Rotated: the mode is landscape, the logical size is portrait.
        assert_eq!(mode_pixels_for(Some((1920, 1080)), (720, 1280)), None);
        // No mode event (wl_output below version 4).
        assert_eq!(mode_pixels_for(None, (1280, 720)), None);
    }

    #[test]
    fn frozen_overlay_maps_buffer_pixels_through_the_logical_grid() {
        // 4x4 logical output, 6x6 buffer (1.5x). A selection at global
        // (1,1)-(3,3) must be the hole at buffer [2,4), not [1,3).
        let output_info = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 4, height: 4 }, scale: 2 };
        let frozen = frozen_test_output(output_info, 6, 6, [10, 20, 30, 255]);
        let cache = build_overlay_render_cache(6, 6, output_info, &frozen, 1);
        let mut data = vec![0u8; 6 * 6 * 4];
        let selected = LogicalRect { x: 1, y: 1, width: 2, height: 2 };
        draw_overlay(&mut data, &cache, output_info, Some(selected), 1, OVERLAY_BORDER_WHITE_BGRA);

        // Inside the hole the frozen pixel is shown undimmed; outside it is
        // dimmed, and the closed ring is white. Comparing against the raw
        // frozen colour distinguishes the hole from both.
        let frozen = [10, 20, 30, 255];
        // buffer (1,1) is global (0,0) - outside the selection.
        assert_ne!(pixel_at(&data, 6, 1, 1), frozen);
        // buffer (2,2) and (4,4) are global (1,1) and (2,2) - inside.
        assert_eq!(pixel_at(&data, 6, 2, 2), frozen);
        assert_eq!(pixel_at(&data, 6, 4, 4), frozen);
        // buffer (5,5) is global (3,3) - outside again.
        assert_ne!(pixel_at(&data, 6, 5, 5), frozen);
    }

    /// At fractional scale the buffer and the canvas are at the same density,
    /// so the preview must show each stitched pixel exactly once. Mapping
    /// through global logical coordinates samples the canvas at 1x and draws it
    /// at the buffer's density, which is the soft preview this pins against.
    #[test]
    fn a_fractional_buffer_maps_the_preview_one_to_one_with_the_canvas() {
        // 4x4 logical output, 6x6 buffer (1.5x). The selection is (1,1)-(3,3)
        // logical and was captured at 3x3 pixels — the buffer's own grid.
        let output_logical = LogicalRect { x: 0, y: 0, width: 4, height: 4 };
        let mut image = Image {
            width: 3,
            height: 3,
            stride: 12,
            format: Format::Argb8888,
            data: vec![0; 3 * 3 * 4],
        };
        for y in 0..3usize {
            for x in 0..3usize {
                let offset = (y * 3 + x) * 4;
                image.data[offset] = (x * 40) as u8;
                image.data[offset + 1] = (y * 40) as u8;
                image.data[offset + 2] = 128;
                image.data[offset + 3] = 255;
            }
        }
        let preview = LongPreviewSnapshot {
            image,
            current_origin_x: 0,
            current_origin_y: 0,
            viewport_rect: LogicalRect { x: 1, y: 1, width: 2, height: 2 },
            capture_width: 3,
            capture_height: 3,
        };
        let mut data = vec![7u8; 6 * 6 * 4];
        draw_preview_on_overlay(&mut data, 6, 6, &preview, output_logical, None);

        for y in 0..3usize {
            for x in 0..3usize {
                assert_eq!(
                    pixel_at(&data, 6, 1 + x, 1 + y),
                    [(x * 40) as u8, (y * 40) as u8, 128, 255],
                    "buffer ({},{}) must show canvas ({x},{y})",
                    1 + x,
                    1 + y
                );
            }
        }
        // Nothing is drawn outside the selection's buffer extent.
        assert_eq!(pixel_at(&data, 6, 0, 0), [7, 7, 7, 7]);
        assert_eq!(pixel_at(&data, 6, 4, 4), [7, 7, 7, 7]);
    }

    #[test]
    fn preview_fills_a_denser_buffer_without_gaps() {
        let preview = LongPreviewSnapshot {
            image: solid_image(2, 2, &[(0, 2, [255, 255, 255, 255])]),
            current_origin_x: 0,
            current_origin_y: 0,
            viewport_rect: LogicalRect { x: 0, y: 0, width: 2, height: 2 },
            capture_width: 2,
            capture_height: 2,
        };
        let output_logical = LogicalRect { x: 0, y: 0, width: 2, height: 2 };
        let mut data = vec![7u8; 4 * 4 * 4];
        draw_preview_on_overlay(&mut data, 4, 4, &preview, output_logical, None);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(pixel_at(&data, 4, x, y), [255, 255, 255, 255], "buffer pixel ({x},{y})");
            }
        }

        // The hole is in buffer coordinates and stays untouched.
        let mut data = vec![7u8; 4 * 4 * 4];
        let hole = LogicalRect { x: 0, y: 0, width: 2, height: 2 };
        draw_preview_on_overlay(&mut data, 4, 4, &preview, output_logical, Some(hole));
        assert_eq!(pixel_at(&data, 4, 0, 0), [7, 7, 7, 7]);
        assert_eq!(pixel_at(&data, 4, 3, 3), [255, 255, 255, 255]);
    }

    #[test]
    fn preview_skips_transparent_stitched_pixels() {
        // A ragged stitched canvas marks never-captured corners transparent;
        // the preview must leave the dimmed overlay visible beneath them
        // instead of painting them black.
        let preview = LongPreviewSnapshot {
            image: solid_image(
                2,
                2,
                &[(0, 1, [255, 255, 255, 255]), (1, 2, [255, 255, 255, 0])],
            ),
            current_origin_x: 0,
            current_origin_y: 0,
            viewport_rect: LogicalRect { x: 0, y: 0, width: 2, height: 2 },
            capture_width: 2,
            capture_height: 2,
        };
        let output_logical = LogicalRect { x: 0, y: 0, width: 2, height: 2 };

        let mut data = vec![7u8; 2 * 2 * 4];
        draw_preview_on_overlay(&mut data, 2, 2, &preview, output_logical, None);

        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 2 + x) * 4;
            [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]
        };
        // Covered row is drawn opaque.
        assert_eq!(pixel(0, 0), [255, 255, 255, 255]);
        // Transparent row leaves the underlying overlay pixel untouched.
        assert_eq!(pixel(0, 1), [7, 7, 7, 7]);
    }

    #[test]
    fn dim_selection_draws_box_for_offset_outputs() {
        // Second output at global logical x=1920: the selection box must be
        // drawn on that output's buffer (regression: a pseudo (0,0) output
        // rect pushed the box off-buffer and nothing was visible).
        let output = OutputInfo {
            global_name: 2,
            logical: LogicalRect { x: 1920, y: 0, width: 100, height: 100 },
            scale: 1,
        };
        let mut data = vec![0u8; 100 * 100 * 4];
        draw_overlay_dim_with_selection(
            &mut data,
            100,
            100,
            output,
            Some(LogicalRect { x: 1920 + 20, y: 10, width: 30, height: 40 }),
            3,
            OVERLAY_BORDER_WHITE_BGRA,
            true,
        );

        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 100 + x) * 4;
            [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]
        };
        // Border row just above the box top edge (local x 20, y 9) is white.
        assert_eq!(pixel(20, 9), OVERLAY_BORDER_WHITE_BGRA);
        // Inside the box is transparent; outside stays dimmed.
        assert_eq!(pixel(35, 30), OVERLAY_SELECTED_TRANSPARENT_BGRA);
        assert_eq!(pixel(5, 5), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn dim_selection_scales_logical_to_buffer_pixels() {
        // Fractional-free scale 2: a logical 30x40 box at (20,10) must land
        // at buffer (40,20) with size 60x80 on a 200x200 buffer.
        let output = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: 0, y: 0, width: 100, height: 100 },
            scale: 2,
        };
        let mut data = vec![0u8; 200 * 200 * 4];
        draw_overlay_dim_with_selection(
            &mut data,
            200,
            200,
            output,
            Some(LogicalRect { x: 20, y: 10, width: 30, height: 40 }),
            3,
            OVERLAY_BORDER_WHITE_BGRA,
            true,
        );

        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 200 + x) * 4;
            [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]
        };
        // Border above the scaled box top (buffer y 19, x 40) is white.
        assert_eq!(pixel(40, 19), OVERLAY_BORDER_WHITE_BGRA);
        // Interior of the scaled box is transparent.
        assert_eq!(pixel(70, 60), OVERLAY_SELECTED_TRANSPARENT_BGRA);
        // A point inside the unscaled logical box but outside the scaled one
        // stays dimmed, proving the box was scaled rather than left logical.
        assert_eq!(pixel(25, 15), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn dim_selection_without_border_draws_only_the_hole() {
        // During an edit drag the border is suppressed and the hole is the
        // drag-trajectory union: every pixel around the hole stays dimmed,
        // and pixels of a rectangle the drag moved away from become
        // transparent too (in-flight captures must see through them).
        let output = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: 0, y: 0, width: 100, height: 100 },
            scale: 1,
        };
        let union = LogicalRect { x: 10, y: 10, width: 60, height: 40 };
        let mut data = vec![0u8; 100 * 100 * 4];
        draw_overlay_dim_with_selection(
            &mut data,
            100,
            100,
            output,
            Some(union),
            3,
            OVERLAY_BORDER_WHITE_BGRA,
            false,
        );

        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 100 + x) * 4;
            [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]
        };
        // Hole interior stays transparent.
        assert_eq!(pixel(20, 20), OVERLAY_SELECTED_TRANSPARENT_BGRA);
        // The border ring just outside the hole is NOT drawn: it stays dim.
        assert_eq!(pixel(20, 9), OVERLAY_OUTSIDE_MASK_BGRA);
        assert_eq!(pixel(9, 20), OVERLAY_OUTSIDE_MASK_BGRA);
        assert_eq!(pixel(20, 50), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn scaled_output_with_scale_one_overlay_keeps_actual_capture_area_clear() {
        // Layer configure sizes are logical; we do not set wl_surface buffer
        // scale. The old output.scale multiplication put the border in this hole.
        let output = OutputInfo {
            global_name: 2,
            logical: LogicalRect { x: -100, y: 200, width: 100, height: 100 },
            scale: 2,
        };
        let rect = LogicalRect { x: -80, y: 210, width: 30, height: 40 };
        let mut pixels = vec![0; 100 * 100 * 4];
        draw_overlay_dim_with_selection(&mut pixels, 100, 100, output, Some(rect), 3, OVERLAY_BORDER_WHITE_BGRA, true);
        for y in 10..50 {
            for x in 20..50 {
                assert_eq!(&pixels[(y * 100 + x) * 4..(y * 100 + x + 1) * 4], &OVERLAY_SELECTED_TRANSPARENT_BGRA);
            }
        }
        assert_eq!(&pixels[(9 * 100 + 20) * 4..(9 * 100 + 21) * 4], &OVERLAY_BORDER_WHITE_BGRA);
    }

    #[test]
    fn edited_capture_is_rejected_while_current_border_stays_visible() {
        let output = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: 0, y: 0, width: 100, height: 100 },
            scale: 1,
        };
        let original = LogicalRect { x: 10, y: 20, width: 60, height: 60 };
        for settled in [
            LogicalRect { x: 30, y: 40, width: 20, height: 20 }, // shrink
            LogicalRect { x: 50, y: 60, width: 30, height: 30 }, // move
        ] {
            let mut state = ui_state();
            state.capture_pending = Some(original);
            state.drag = DragState::Finished(settled);
            // Release cleared edit_drag, but the capture is still pending.
            invalidate_changed_capture(&mut state);
            assert!(state.capture_invalid);
            assert!(ui_render_border(&state));
            let hole = ui_render_selection(&state);
            let mut pixels = vec![0; 100 * 100 * 4];
            draw_live_base(&mut pixels, 100, 100, hole);
            let preview = LongPreviewSnapshot {
                image: solid_image(100, 100, &[(0, 100, [255, 255, 255, 255])]),
                current_origin_x: 0,
                current_origin_y: 0,
                viewport_rect: output.logical,
                capture_width: 100,
                capture_height: 100,
            };
            draw_live_foreground(&mut pixels, 100, 100, output.logical, hole, ui_render_border_rect(&state), Some(&preview), 3,
                OVERLAY_BORDER_WHITE_BGRA);
            assert_eq!(pixel_at(&pixels, 100, (settled.x - 1) as usize, (settled.y - 1) as usize), OVERLAY_BORDER_WHITE_BGRA);
            for y in settled.y..settled.bottom() {
                for x in settled.x..settled.right() {
                    let offset = ((y * 100 + x) * 4) as usize;
                    assert_eq!(&pixels[offset..offset + 4], &OVERLAY_SELECTED_TRANSPARENT_BGRA);
                }
            }
            // Returning to the old geometry must not make a polluted frame valid.
            state.drag = DragState::Finished(original);
            invalidate_changed_capture(&mut state);
            assert!(state.capture_invalid);
            state.drag = DragState::Finished(settled);
            state.capture_pending = None;
            assert_eq!(ui_render_selection(&state), Some(settled));
            assert!(ui_render_border(&state));
        }
    }

    #[test]
    fn live_border_is_above_preview_on_every_redraw() {
        let output = LogicalRect { x: 0, y: 0, width: 100, height: 100 };
        let hole = LogicalRect { x: 30, y: 30, width: 40, height: 40 };
        let mut base = vec![0; 100 * 100 * 4];
        draw_live_base(&mut base, 100, 100, Some(hole));
        // Reuse the same border-free cache while preview, border color,
        // thickness, and visibility change between renders.
        for (color, thickness) in [(OVERLAY_BORDER_WHITE_BGRA, 3), (OVERLAY_BORDER_WHITE_RED_BGRA, 5)] {
            let preview_color = [40, 80, 120, 255];
            let preview = LongPreviewSnapshot {
                image: solid_image(100, 100, &[(0, 100, preview_color)]),
                current_origin_x: 0,
                current_origin_y: 0,
                viewport_rect: output,
                capture_width: 100,
                capture_height: 100,
            };
            for visible in [true, false, true] {
                let mut data = base.clone();
                draw_live_foreground(&mut data, 100, 100, output, Some(hole), visible.then_some(hole), Some(&preview), thickness, color);
                let pixel = |x: i32, y: i32| {
                    let offset = ((y * 100 + x) * 4) as usize;
                    &data[offset..offset + 4]
                };
                for step in 0..thickness {
                    for (x, y) in [(40, 29 - step), (40, 70 + step), (29 - step, 40), (70 + step, 40)] {
                        assert_eq!(pixel(x, y), if visible { &color } else { &preview_color });
                    }
                }
                assert_eq!(pixel(10, 10), &preview_color);
                for y in hole.y..hole.bottom() {
                    for x in hole.x..hole.right() {
                        assert_eq!(pixel(x, y), &OVERLAY_SELECTED_TRANSPARENT_BGRA);
                    }
                }
            }
        }
    }

    #[test]
    fn frozen_and_live_borders_are_identical_closed_rings_for_all_widths() {
        let output = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: -100, y: 200, width: 100, height: 100 },
            scale: 2,
        };
        let rect = LogicalRect { x: -70, y: 230, width: 40, height: 40 };
        let local = selected_buffer_rect(output, rect, 100, 100).unwrap();
        let frozen = frozen_test_output(output, 100, 100, [20, 30, 40, 255]);
        for thickness in 1..=16 {
            let cache = build_overlay_render_cache(100, 100, output, &frozen, thickness);
            let mut normal = vec![0; 100 * 100 * 4];
            let mut live = normal.clone();
            draw_overlay(&mut normal, &cache, output, Some(rect), thickness, OVERLAY_BORDER_WHITE_BGRA);
            draw_overlay_dim_with_selection(&mut live, 100, 100, output, Some(rect), thickness, OVERLAY_BORDER_WHITE_BGRA, true);
            for y in 0..100 {
                for x in 0..100 {
                    // Independent expected ring, including corner squares.
                    let expected = x >= 30 - thickness && x < 70 + thickness
                        && y >= 30 - thickness && y < 70 + thickness
                        && !(x >= 30 && x < 70 && y >= 30 && y < 70);
                    let n = pixel_at(&normal, 100, x as usize, y as usize);
                    let l = pixel_at(&live, 100, x as usize, y as usize);
                    assert_eq!(n == OVERLAY_BORDER_WHITE_BGRA, expected, "normal {x},{y}, width {thickness}");
                    assert_eq!(l == OVERLAY_BORDER_WHITE_BGRA, expected, "live {x},{y}, width {thickness}");
                }
            }
            // Clipping keeps a border at negative/out-of-output coordinates safe.
            let mut clipped = vec![0; 100 * 100 * 4];
            draw_selection_border(&mut clipped, 100, 100, -5, -5, local.width, local.height, thickness, OVERLAY_BORDER_WHITE_BGRA);
        }
    }

    #[test]
    fn dirty_redraw_erases_old_border_including_corners() {
        let output = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 100 }, scale: 1 };
        let old = LogicalRect { x: 20, y: 20, width: 20, height: 20 };
        let new = LogicalRect { x: 60, y: 60, width: 20, height: 20 };
        let frozen = frozen_test_output(output, 100, 100, [20, 30, 40, 255]);
        let cache = build_overlay_render_cache(100, 100, output, &frozen, 16);
        let mut data = vec![0; 100 * 100 * 4];
        draw_overlay(&mut data, &cache, output, Some(old), 16, OVERLAY_BORDER_WHITE_BGRA);
        let dirty = dirty_selected_region(Some(border_bounds(old, 16)), Some(border_bounds(new, 16)), 100, 100).unwrap();
        draw_overlay_region(&mut data, &cache, output, Some(new), dirty, 16, OVERLAY_BORDER_WHITE_BGRA);
        let mut expected = vec![0; data.len()];
        draw_overlay(&mut expected, &cache, output, Some(new), 16, OVERLAY_BORDER_WHITE_BGRA);
        assert_eq!(data, expected);
    }

    #[test]
    fn render_selection_follows_the_box_in_both_directions() {
        let mut state = ui_state();
        state.drag = DragState::Finished(SELECTED);
        // No drag in progress: the render selection is the live rectangle.
        assert_eq!(ui_render_selection(&state), Some(SELECTED));
        assert!(ui_render_border(&state));

        // During an edit drag the hole is the *current* box, never a union of
        // the trajectory: shrinking must move the dim edge immediately, or the
        // border slides while the dim stays behind (the reported stutter).
        state.edit_drag = Some(EditDrag::Move { offset_x: 1, offset_y: 1, origin: SELECTED });
        let shrunk = LogicalRect { x: 20, y: 20, width: 30, height: 30 };
        state.drag = DragState::Finished(shrunk);
        assert_eq!(ui_render_selection(&state), Some(shrunk));
        assert!(ui_render_border(&state));
        assert_eq!(ui_render_border_rect(&state), Some(shrunk));

        // A pending capture keeps its region in the hole until it completes,
        // so an in-flight frame is never exposed by the moving box.
        let pending = LogicalRect { x: 10, y: 10, width: 60, height: 60 };
        state.capture_pending = Some(pending);
        assert_eq!(ui_render_selection(&state), Some(rect_union(shrunk, pending)));
    }

    #[test]
    fn shrinking_during_an_edit_drag_does_not_leave_a_dim_trail() {
        // Regression: the hole used to be the union of every rectangle the
        // drag had passed through. Enlarging looked continuous because the
        // union grew with the box; shrinking left the dim edge at the widest
        // point, so the border moved while the dim did not.
        let mut state = ui_state();
        let wide = LogicalRect { x: 10, y: 10, width: 80, height: 80 };
        let narrow = LogicalRect { x: 40, y: 40, width: 20, height: 20 };
        state.drag = DragState::Finished(wide);
        state.edit_drag = Some(EditDrag::Edge { edge: SelectionEdge::Right, origin: SELECTED });
        // Widest point, then shrunk: the rendered hole must be the narrow box.
        state.drag = DragState::Finished(narrow);
        assert_eq!(ui_render_selection(&state), Some(narrow));
    }

    #[test]
    fn edit_drag_drops_an_invalid_pending_region_so_the_hole_follows_the_box() {
        // An in-flight capture prepared from the wide rectangle cannot be
        // trusted after the box moved, so the motion handler invalidates it
        // and drops its region. The hole must then be the current box, not
        // the stale union — otherwise shrinking still lags.
        let mut state = ui_state();
        let wide = LogicalRect { x: 10, y: 10, width: 80, height: 80 };
        let narrow = LogicalRect { x: 40, y: 40, width: 20, height: 20 };
        state.drag = DragState::Finished(narrow);
        state.edit_drag = Some(EditDrag::Edge { edge: SelectionEdge::Left, origin: SELECTED });
        state.capture_pending = Some(wide);

        invalidate_changed_capture(&mut state);
        drop_invalid_pending(&mut state);

        assert!(state.capture_invalid, "the in-flight frame must stay discarded");
        assert_eq!(state.capture_pending, None);
        assert_eq!(ui_render_selection(&state), Some(narrow));
    }

    #[test]
    fn edit_drag_keeps_a_matching_pending_region() {
        // Moving a box and returning it to the pending rectangle leaves the
        // pending capture valid; its region stays in the hole.
        let mut state = ui_state();
        let rect = LogicalRect { x: 20, y: 20, width: 40, height: 40 };
        state.drag = DragState::Finished(rect);
        state.edit_drag = Some(EditDrag::Move { offset_x: 1, offset_y: 1, origin: SELECTED });
        state.capture_pending = Some(rect);

        invalidate_changed_capture(&mut state);
        drop_invalid_pending(&mut state);

        assert!(!state.capture_invalid);
        assert_eq!(state.capture_pending, Some(rect));
        assert_eq!(ui_render_selection(&state), Some(rect));
    }

    #[test]
    fn edit_drag_active_reflects_edit_state() {
        let mut state = ui_state();
        assert!(!state.edit_drag.is_some());
        state.edit_drag = Some(EditDrag::Edge { edge: SelectionEdge::Left, origin: SELECTED });
        assert!(state.edit_drag.is_some());
    }

    #[test]
    fn apply_box_move_follows_pointer_and_clamps_to_bounds() {
        let rect = LogicalRect { x: 100, y: 100, width: 40, height: 30 };
        // Grabbed 10 px inside the box; the box origin tracks the pointer.
        let roomy = LogicalRect { x: 90, y: 90, width: 200, height: 200 };
        let moved = apply_box_move(rect, 10, 5, 130, 115, roomy);
        assert_eq!(moved, LogicalRect { x: 120, y: 110, width: 40, height: 30 });
        // Moving past the bounds clamps while keeping the size.
        let bounds = LogicalRect { x: 90, y: 90, width: 60, height: 60 };
        let clamped = apply_box_move(rect, 0, 0, 500, 500, bounds);
        assert_eq!(clamped, LogicalRect { x: 110, y: 120, width: 40, height: 30 });
        let clamped = apply_box_move(rect, 0, 0, -50, -50, bounds);
        assert_eq!(clamped, LogicalRect { x: 90, y: 90, width: 40, height: 30 });
    }

    #[test]
    fn border_color_uses_the_same_warning_color_for_spans_and_stitch_failures() {
        assert_eq!(selection_border_color(true, false), OVERLAY_BORDER_WHITE_RED_BGRA);
        assert_eq!(selection_border_color(false, false), OVERLAY_BORDER_WHITE_BGRA);
        // A stitch warning warns on its own: it is about the content, not
        // about the selection geometry.
        assert_eq!(selection_border_color(false, true), OVERLAY_BORDER_WHITE_RED_BGRA);
        assert_eq!(selection_border_color(true, true), OVERLAY_BORDER_WHITE_RED_BGRA);
        // The warning colour must be visibly distinct from the plain border.
        assert_ne!(OVERLAY_BORDER_WHITE_RED_BGRA, OVERLAY_BORDER_WHITE_BGRA);
    }

    #[test]
    fn only_long_mode_warns_about_a_spanning_selection() {
        // Normal mode composites a spanning selection from per-output pieces,
        // so it is a supported capture and must never be flagged — not while
        // dragging, not after release, not in edit mode.
        let spanning = LogicalRect { x: 10, y: 10, width: 200, height: 20 };
        let inside = LogicalRect { x: 10, y: 10, width: 20, height: 20 };

        assert_eq!(
            border_color_for(Some(spanning), true, false, false),
            OVERLAY_BORDER_WHITE_BGRA,
            "a spanning normal-mode selection is a valid capture"
        );
        assert_eq!(
            border_color_for(Some(inside), false, false, false),
            OVERLAY_BORDER_WHITE_BGRA
        );
        // Long mode cannot capture a spanning selection: it warns.
        assert_eq!(
            border_color_for(Some(spanning), true, true, false),
            OVERLAY_BORDER_WHITE_RED_BGRA
        );
        // A stitch warning warns regardless of mode or geometry: it is about
        // the content, not the selection.
        assert_eq!(
            border_color_for(Some(inside), false, true, true),
            OVERLAY_BORDER_WHITE_RED_BGRA
        );
        assert_eq!(
            border_color_for(Some(inside), false, false, true),
            OVERLAY_BORDER_WHITE_RED_BGRA
        );
        // No rect, nothing to warn about.
        assert_eq!(
            border_color_for(None, true, true, false),
            OVERLAY_BORDER_WHITE_BGRA
        );
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

    /// A long-mode box spanning outputs is not a selection: it is dropped, not
    /// clamped to one output (which would capture a box the user never chose).
    #[test]
    fn a_long_selection_that_spans_outputs_is_discarded_not_clamped() {
        let infos = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let spanning = LogicalRect { x: 90, y: 10, width: 30, height: 20 };
        let inside = LogicalRect { x: 10, y: 10, width: 30, height: 20 };

        assert!(long_selection_must_be_discarded(true, Some(spanning), &infos));
        assert!(!long_selection_must_be_discarded(true, Some(inside), &infos));
        // Normal mode composites a spanning selection from per-output pieces,
        // so the same box is a valid capture there.
        assert!(!long_selection_must_be_discarded(false, Some(spanning), &infos));
        assert!(!long_selection_must_be_discarded(true, None, &infos));
    }

    /// An edit drag may cross outputs (that is how the red warning becomes
    /// visible), and a long-mode drag that ends up spanning is undone.
    #[test]
    fn an_edit_drag_across_outputs_is_undone_on_release() {
        let infos = [
            OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 100, height: 80 }, scale: 1 },
            OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 100, height: 80 }, scale: 1 },
        ];
        let origin = LogicalRect { x: 10, y: 10, width: 40, height: 30 };
        let across = LogicalRect { x: 90, y: 10, width: 40, height: 30 };
        let inside = LogicalRect { x: 20, y: 10, width: 40, height: 30 };

        // Long mode: the box goes back where it was, as if nothing was dragged.
        assert_eq!(settled_edit_rect(true, across, origin, &infos), origin);
        // A drag that stays on one output is kept.
        assert_eq!(settled_edit_rect(true, inside, origin, &infos), inside);
        // Normal mode composites a spanning selection, so it is never undone.
        assert_eq!(settled_edit_rect(false, across, origin, &infos), across);

        // The drag bounds are the whole desktop, so the box really can be
        // dragged over the seam (where the border turns red).
        let bounds = selection_drag_bounds(&infos).unwrap();
        assert_eq!(bounds, LogicalRect { x: 0, y: 0, width: 200, height: 80 });
        let dragged = apply_box_move(inside, 0, 0, 90, 30, bounds);
        assert!(crate::geometry::rect_spans_outputs(dragged, &infos));
    }

    #[test]
    fn discarding_a_selection_returns_to_the_empty_selection_state() {
        let mut state = ui_state();
        state.long_mode = true;
        state.drag = DragState::Finished(SELECTED);
        state.selection_anchor = Some((10, 20));

        discard_selection(&mut state);

        assert_eq!(state.drag, DragState::Idle);
        assert_eq!(state.selection_anchor, None);
        // The selection state is what the next press starts from, so the
        // overlay must be told to repaint without a box.
        assert!(state.long_mode);
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

    fn ui_state() -> UiState {
        UiState {
            compositor: None,
            shm: None,
            layer_shell: None,
            xdg_output_manager: None,
            viewporter: None,
            fractional_scale_manager: None,
            seat: None,
            pointer: None,
            keyboard: None,
            outputs: Vec::new(),
            frozen_outputs: Vec::new(),
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
            stitch_warning: false,
            pending_render: None,
            editing: false,
            edit_drag: None,
            capture_pending: None,
            capture_invalid: false,
            selection_anchor: None,
            border_width: crate::config::DEFAULT_BORDER_WIDTH,
            live_state_seq: 1,
        render_scope: None,
        }
    }

    #[test]
    fn enter_key_resolves_to_confirm_while_editing_normal_selection() {
        let mut state = ui_state();
        state.drag = DragState::Finished(SELECTED);
        state.editing = true;
        assert_eq!(resolve_key_action(&state, KEY_ENTER), KeyAction::ConfirmSelection);
        assert_eq!(resolve_key_action(&state, KEY_SPACE), KeyAction::ConfirmSelection);
        // Confirming leaves edit mode so the selection loop returns the rect.
        apply_key_action(&mut state, KeyAction::ConfirmSelection);
        assert!(!state.editing);
        assert_eq!(state.drag, DragState::Finished(SELECTED));
    }

    #[test]
    fn enter_key_finishes_long_capture_from_edit_mode() {
        let mut state = ui_state();
        state.long_mode = true;
        state.drag = DragState::Finished(SELECTED);
        state.editing = true;
        let action = resolve_key_action(&state, KEY_ENTER);
        apply_key_action(&mut state, action);
        assert!(!state.editing);
        assert!(state.long_finish_requested);
    }

    #[test]
    fn e_key_finalizes_drag_into_editable_rectangle() {
        let mut state = ui_state();
        state.pointer_x = 50;
        state.pointer_y = 40;
        state.drag = DragState::Dragging { origin_x: 10, origin_y: 10, current_x: 50, current_y: 40 };
        assert_eq!(resolve_key_action(&state, KEY_E), KeyAction::ToggleEdit);
        let mode = apply_key_action(&mut state, KeyAction::ToggleEdit);
        // Normal mode never changes overlay input mode.
        assert_eq!(mode, None);
        assert!(state.editing);
        assert_eq!(state.drag, DragState::Finished(LogicalRect::from_points(10, 10, 50, 40)));
    }

    #[test]
    fn e_key_toggle_requests_pointer_input_modes_only_in_long_mode() {
        let mut state = ui_state();
        state.long_mode = true;
        state.drag = DragState::Finished(SELECTED);
        assert_eq!(
            apply_key_action(&mut state, KeyAction::ToggleEdit),
            Some(OverlayInputMode::Interactive)
        );
        assert!(state.editing);
        assert_eq!(
            apply_key_action(&mut state, KeyAction::ToggleEdit),
            Some(OverlayInputMode::Passthrough)
        );
        assert!(!state.editing);
    }

    #[test]
    fn e_key_is_ignored_without_a_selection() {
        let mut state = ui_state();
        assert_eq!(resolve_key_action(&state, KEY_E), KeyAction::Ignore);
        apply_key_action(&mut state, KeyAction::Ignore);
        assert!(!state.editing);
        assert_eq!(state.drag, DragState::Idle);
    }

    #[test]
    fn escape_cancels_and_leaves_edit_mode() {
        let mut state = ui_state();
        state.drag = DragState::Finished(SELECTED);
        state.editing = true;
        state.edit_drag = Some(EditDrag::Edge { edge: SelectionEdge::Left, origin: SELECTED });
        assert_eq!(resolve_key_action(&state, KEY_ESC), KeyAction::Cancel);
        apply_key_action(&mut state, KeyAction::Cancel);
        assert!(!state.editing);
        assert_eq!(state.edit_drag, None);
        assert_eq!(state.drag, DragState::Cancelled);
    }

    #[test]
    fn arrow_keys_hint_directions_during_long_capture_only() {
        let mut state = ui_state();
        state.long_mode = true;
        state.drag = DragState::Finished(SELECTED);
        assert_eq!(resolve_key_action(&state, KEY_LEFT), KeyAction::HintDirection(SearchDirection::Left));
        assert_eq!(resolve_key_action(&state, KEY_RIGHT), KeyAction::HintDirection(SearchDirection::Right));
        apply_key_action(&mut state, KeyAction::HintDirection(SearchDirection::Right));
        assert_eq!(state.long_direction, Some(SearchDirection::Right));

        // Outside a finished long capture the arrows do nothing.
        let plain = ui_state();
        assert_eq!(resolve_key_action(&plain, KEY_LEFT), KeyAction::Ignore);
    }

    #[test]
    fn promote_long_resets_edit_mode() {
        let mut state = ui_state();
        state.drag = DragState::Finished(SELECTED);
        state.editing = true;
        apply_key_action(&mut state, KeyAction::PromoteLong);
        assert!(!state.editing);
        assert!(state.long_mode);
        assert!(state.long_requested);
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
        assert_eq!(overlay_pixel(6, 20, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_BGRA), OVERLAY_OUTSIDE_MASK_BGRA);
        assert_eq!(overlay_pixel(10, 16, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_BGRA), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn overlay_pixel_keeps_selection_interior_transparent() {
        assert_eq!(overlay_pixel(13, 23, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_BGRA), OVERLAY_SELECTED_TRANSPARENT_BGRA);
    }

    #[test]
    fn overlay_pixel_keeps_selection_edge_clear() {
        assert_eq!(overlay_pixel(10, 20, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_BGRA), OVERLAY_SELECTED_TRANSPARENT_BGRA);
    }

    #[test]
    fn overlay_pixel_draws_white_adjacent_border() {
        assert_eq!(overlay_pixel(9, 19, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_BGRA), OVERLAY_BORDER_WHITE_BGRA);
    }

    #[test]
    fn overlay_pixel_honours_the_requested_warning_color() {
        // Regression: the frozen path hardcoded the border colour, so a drag
        // across outputs kept the plain white border.
        assert_eq!(
            overlay_pixel(9, 19, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_RED_BGRA),
            OVERLAY_BORDER_WHITE_RED_BGRA
        );
        assert_eq!(
            overlay_pixel(13, 23, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_RED_BGRA),
            OVERLAY_SELECTED_TRANSPARENT_BGRA
        );
        assert_eq!(
            overlay_pixel(6, 20, Some(SELECTED), 3, OVERLAY_BORDER_WHITE_RED_BGRA),
            OVERLAY_OUTSIDE_MASK_BGRA
        );
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
        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen, 3);

        draw_overlay(&mut data, &cache, output_info, Some(selected), 3, OVERLAY_BORDER_WHITE_BGRA);

        assert_eq!(pixel_at(&data, output.width as usize, 0, 0), OVERLAY_BORDER_WHITE_BGRA);
        // The border is a closed outside ring, including the four corners.
        assert_eq!(pixel_at(&data, output.width as usize, 1, 1), OVERLAY_BORDER_WHITE_BGRA);
        assert_eq!(pixel_at(&data, output.width as usize, 2, 2), frozen_pixel);
        assert_eq!(pixel_at(&data, output.width as usize, 5, 5), frozen_pixel);
    }

    #[test]
    fn draw_overlay_region_restores_old_selection_area() {
        let output = LogicalRect { x: 100, y: 50, width: 8, height: 8 };
        let previous = LogicalRect { x: 101, y: 51, width: 5, height: 5 };
        let current = LogicalRect { x: 103, y: 53, width: 3, height: 3 };
        let dirty = dirty_selected_region(
            selected_local_intersection(output, previous).map(|r| border_bounds(r, 3)),
            selected_local_intersection(output, current).map(|r| border_bounds(r, 3)),
            output.width,
            output.height,
        )
        .unwrap();
        let mut data = vec![0; (output.width * output.height * 4) as usize];

        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let frozen_pixel = [10, 20, 30, 255];
        let frozen = frozen_test_output(output_info, output.width, output.height, frozen_pixel);
        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen, 3);

        draw_overlay(&mut data, &cache, output_info, Some(previous), 3, OVERLAY_BORDER_WHITE_BGRA);
        draw_overlay_region(&mut data, &cache, output_info, Some(current), dirty, 3, OVERLAY_BORDER_WHITE_BGRA);

        assert_eq!(pixel_at(&data, output.width as usize, 1, 1), OVERLAY_BORDER_WHITE_BGRA);
        assert_eq!(pixel_at(&data, output.width as usize, 2, 2), OVERLAY_BORDER_WHITE_BGRA);
    }

    #[test]
    fn build_overlay_render_cache_stores_base_and_dimmed_pixels() {
        let output = LogicalRect { x: 0, y: 0, width: 2, height: 1 };
        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let mut frozen = frozen_test_output(output_info, output.width, output.height, [10, 20, 30, 255]);
        frozen.data[4..8].copy_from_slice(&[40, 50, 60, 255]);

        let cache = build_overlay_render_cache(output.width, output.height, output_info, &frozen, 3);

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

        assert_eq!(frozen_pixel(&frozen, 2, 2, 1, 1), [11, 22, 33, 255]);
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
