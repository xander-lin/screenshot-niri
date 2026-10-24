use std::error::Error;
use std::time::Duration;
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
const KEY_H: u32 = 35;
/// `0` (evdev keycode): capture every screen as one image. Sits next to the
/// `1`-`9` full-output keys, which start at `KEY_DIGIT_FIRST`.
const KEY_ZERO: u32 = 11;
const KEY_DIGIT_FIRST: u32 = 2;
const KEY_DIGIT_LAST: u32 = 10;
/// Buffers per overlay: two — one on screen, one committed and awaiting display.
///
/// The overlay paints a dim mask, a hole and a border, and it commits only when
/// the box changes. A third buffer used to be reserved for a compositor that
/// holds a buffer for a whole frame while the client produces the next one,
/// which would starve a two-buffer rotation into a self-sustaining stall (no
/// free buffer → no commit → no swap → no release). Measured on niri 26.04 with
/// two outputs a drag never needs it. If a compositor is ever found that does
/// hold a frame, this is the constant to raise — the cost is a full-resolution
/// RGBA set per overlay (8.1 MB at 1920x1080).
const OVERLAY_BUFFER_COUNT: usize = 2;
const OVERLAY_OUTSIDE_MASK_BGRA: [u8; 4] = [0, 0, 0, 110];
const OVERLAY_SELECTED_TRANSPARENT_BGRA: [u8; 4] = [0, 0, 0, 0];
const OVERLAY_BORDER_WHITE_BGRA: [u8; 4] = [255, 255, 255, 255];
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    Selected(SelectedViewport),
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
    /// Where the compositor placed this output, from both sources that report
    /// it. Kept as a field so the resolution can be tested without standing up
    /// Wayland proxies.
    position: OutputPositionSource,
}

impl OutputRuntime {
    /// Copies whatever position is currently known into `info.logical`.
    fn resolve_logical_position(&mut self) {
        if let Some((x, y)) = self.position.resolve() {
            self.info.logical.x = x;
            self.info.logical.y = y;
        }
    }
}

/// Where an output sits, from the two events that can say so.
///
/// `logical_position` (`zxdg_output_v1`) wins: it is the layout coordinate the
/// session's rectangles are expressed in. `geometry` (`wl_output`) is the
/// fallback for a compositor without the xdg output manager; without it every
/// output would keep the `(0, 0)` placeholder it was seeded with, collapsing a
/// multi-screen desktop into one rectangle at the origin — and any selection
/// there would then look like it spans outputs. The protocol calls `geometry`
/// "partial information" and steers clients to `logical_position` instead,
/// which is why it is only ever the fallback: on a compositor that scales
/// outputs it may report pixels where the layout is in logical units.
///
/// Both arrive as independent events in no fixed order, so neither handler
/// decides on its own — each records what it saw and the position is resolved
/// from both. That is what stops a `geometry` landing *after* a
/// `logical_position` from overwriting it.
#[derive(Debug, Clone, Copy, Default)]
struct OutputPositionSource {
    logical: Option<(i32, i32)>,
    geometry: Option<(i32, i32)>,
}

impl OutputPositionSource {
    fn resolve(self) -> Option<(i32, i32)> {
        self.logical.or(self.geometry)
    }

    fn set_logical(&mut self, x: i32, y: i32) {
        self.logical = Some((x, y));
    }

    fn set_geometry(&mut self, x: i32, y: i32) {
        self.geometry = Some((x, y));
    }
}

struct OverlaySurface {
    output_name: u32,
    /// Declared before the surface so these are destroyed first: both are
    /// bound to the wl_surface they were created from.
    viewport: Option<WpViewport>,
    /// Kept alive for the surface's lifetime so `preferred_scale` events keep
    /// arriving; nothing reads the proxy itself after construction.
    #[cfg_attr(not(feature = "trace-logs"), allow(dead_code))]
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
}

/// Marks an overlay as being on the frozen-image path, and how big its buffer is.
///
/// It used to hold two whole buffers of pixels — `base` (the frozen frame) and
/// `dimmed` (that frame composited under the dim mask) — 8.1 MB each per output
/// at 1920x1080. Neither is storage: `base` is the frozen capture itself
/// (sampled through `frozen_pixel`, an identity when the buffer and the capture
/// share a grid) and `dimmed` is `base` composited under one constant mask. On a
/// two-output session that was 33 MB of duplication, and `H` rebuilt all of it on
/// every press. Both are now computed per pixel from the frozen frame, which the
/// session already holds once.
struct OverlayRenderCache {
    width: i32,
    height: i32,
}

struct FrozenOutput {
    info: OutputInfo,
    width: i32,
    height: i32,
    stride: i32,
    /// The frame as captured **without** the compositor's cursor overlay, which
    /// is what the selection overlay shows by default (`H` flips it).
    data: Vec<u8>,
    /// What the with-cursor capture adds to it.
    cursor_delta: CursorDelta,
}

/// Pixels where the with-cursor capture differs from the without-cursor one.
///
/// The two frames are captured back to back and differ only where the
/// compositor drew the cursor: measured 1 440–2 706 pixels of 2 073 600 on a
/// 1920x1080 output (0.07–0.13%), so 8.3 MB → 11–22 KB. Storing the second
/// capture whole — which is what this replaces — cost 16.6 MB on a two-output
/// session for a patch the size of a mouse pointer.
///
/// The patch is deliberately *not* assumed to be only the cursor: it records
/// every pixel that differs, so a caret that blinked or a clock that ticked
/// between the two captures is reproduced exactly rather than being dropped
/// from the frame the user is about to cut. The bounding box is kept because it
/// is the cursor's footprint — it is what the two captures differ by — and it
/// bounds the work when a frame is needed for one region only.
/// Differing pixels on one capture row, sorted by x.
type RowPatch = Vec<(u32, [u8; 4])>;

struct CursorDelta {
    /// Differing pixels per capture row, sorted by x. Rows with none are
    /// omitted, which is almost all of them.
    rows: Vec<(u32, RowPatch)>,
    /// Bounding box of every recorded pixel: `(x, y, width, height)`.
    ///
    /// The cursor's footprint, and what makes the patch's size legible in a
    /// trace or a failure message. Nothing reads it to decide anything.
    #[cfg_attr(not(test), allow(dead_code))]
    bounds: Option<(i32, i32, i32, i32)>,
    /// Bytes this patch occupies, for the memory report.
    #[cfg_attr(not(test), allow(dead_code))]
    bytes: usize,
}

impl CursorDelta {
    const EMPTY: CursorDelta = CursorDelta { rows: Vec::new(), bounds: None, bytes: 0 };

    /// Records every pixel where `other` differs from `base`.
    ///
    /// `base` and `other` must share a geometry; a mismatch records nothing and
    /// leaves the caller on the base frame, which is the safe direction (the
    /// cursor is not in the picture, rather than the picture being wrong).
    fn between(base: &FrozenOutput, other: &FrozenOutput) -> CursorDelta {
        if (base.width, base.height, base.stride) != (other.width, other.height, other.stride) {
            return CursorDelta::EMPTY;
        }
        let mut rows: Vec<(u32, RowPatch)> = Vec::new();
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        let mut bytes = 0usize;
        for y in 0..base.height as u32 {
            let row_start = y as usize * base.stride as usize;
            let mut pixels: RowPatch = Vec::new();
            for x in 0..base.width as u32 {
                let offset = row_start + x as usize * 4;
                let (Some(a), Some(b)) = (
                    base.data.get(offset..offset + 4),
                    other.data.get(offset..offset + 4),
                ) else {
                    continue;
                };
                if a == b {
                    continue;
                }
                let pixel = [b[0], b[1], b[2], b[3]];
                pixels.push((x, pixel));
                bytes += std::mem::size_of::<(u32, [u8; 4])>();
                min_x = min_x.min(x as i32);
                min_y = min_y.min(y as i32);
                max_x = max_x.max(x as i32);
                max_y = max_y.max(y as i32);
            }
            if !pixels.is_empty() {
                rows.push((y, pixels));
            }
        }
        let bounds = (min_y != i32::MAX).then(|| (min_x, min_y, max_x - min_x + 1, max_y - min_y + 1));
        CursorDelta { rows, bounds, bytes }
    }

    /// The pixels this patch replaces on capture row `y`.
    fn row(&self, y: u32) -> Option<&[(u32, [u8; 4])]> {
        if self.rows.is_empty() {
            return None;
        }
        self.rows
            .binary_search_by_key(&y, |(row, _)| *row)
            .ok()
            .map(|index| self.rows[index].1.as_slice())
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
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
    /// Without it the frozen capture still sizes the frozen overlay correctly.
    fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    outputs: Vec<OutputRuntime>,
    /// One frozen frame per output: the without-cursor capture, plus the patch
    /// that turns it into the with-cursor one. `screenshot_cursor_hidden` picks
    /// which to draw; see `frozen_pixel`.
    frozen_outputs: Vec<FrozenOutput>,
    overlays: Vec<OverlaySurface>,
    pointer_output_name: Option<u32>,
    screenshot_cursor_hidden: bool,
    pointer_x: i32,
    pointer_y: i32,
    drag: DragState,
    pending_render: bool,
    /// A redraw asked for during this event-loop turn.
    ///
    /// Pointer motion arrives far faster than the screen refreshes, and
    /// `blocking_dispatch` returns once per event, so drawing on every turn meant
    /// thousands of synchronous repaints a second. Requests are coalesced for the
    /// turn and then rate-limited to one frame per refresh; intermediate pointer
    /// positions only update `UiState`, and the frame that is drawn shows the
    /// latest one.
    render_requested: bool,
    /// When the overlay last committed, for the rate limit above.
    last_render: Option<std::time::Instant>,
    /// Visible selection-border thickness in logical pixels (configurable).
    border_width: i32,
}

impl UiState {
    /// A session with no compositor objects bound and no overlays yet.
    ///
    /// Every field that is *not* a parameter starts at its "nothing has
    /// happened yet" value, and the list lives here once, so adding a field
    /// cannot silently give tests a different initial state than the product.
    fn unbound(frozen_outputs: Vec<FrozenOutput>, border_width: i32) -> Self {
        Self {
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
            overlays: Vec::new(),
            pointer_output_name: None,
            screenshot_cursor_hidden: true,
            pointer_x: 0,
            pointer_y: 0,
            drag: DragState::Idle,
            pending_render: false,
            render_requested: false,
            last_render: None,
            border_width: border_width.clamp(
                crate::config::BORDER_WIDTH_MIN,
                crate::config::BORDER_WIDTH_MAX,
            ),
        }
    }

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

/// The rectangle the overlay currently draws: the box under the pointer.
fn ui_render_selection(state: &UiState) -> Option<LogicalRect> {
    state.drag.current_rect()
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
fn selection_border_pixel(x: i32, y: i32, rect: LogicalRect, thickness: i32) -> Option<[u8; 4]> {
    let outer = border_bounds(rect, thickness);
    let inside = |r: LogicalRect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
    (!rect.is_empty() && inside(outer) && !inside(rect)).then_some(OVERLAY_BORDER_WHITE_BGRA)
}

pub struct SelectionSession {
    conn: Connection,
    event_queue: wayland_client::EventQueue<UiState>,
    state: UiState,
}

impl SelectionSession {
    pub fn with_frozen_pair(
        frozen_outputs: &[CapturedOutput],
        frozen_outputs_without_cursor: &[CapturedOutput],
        border_width: i32,
    ) -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()?;
        let mut event_queue = conn.new_event_queue::<UiState>();
        let qh = event_queue.handle();
        // The pair is folded into one list here: the with-cursor capture
        // becomes the patch on the without-cursor frame, not a second frame.
        let frozen_outputs = frozen_outputs_from_captures(frozen_outputs, frozen_outputs_without_cursor)?;
        let mut state = UiState::unbound(frozen_outputs, border_width);

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
        Ok(Self { conn, event_queue, state })
    }

    pub fn run_selection(&mut self) -> Result<SelectionOutcome, Box<dyn Error>> {
        while selection_loop_active(&self.state) {
            // Wait for events normally, but wake at the frame interval while a
            // redraw is pending: a request deferred by the rate limit must still
            // be drawn if the pointer stops right then.
            if self.state.render_requested {
                let timeout = OVERLAY_FRAME_INTERVAL.as_millis() as i32;
                self.dispatch_poll(timeout)?;
            } else {
                self.event_queue.blocking_dispatch(&mut self.state)?;
            }
            flush_render_request(&mut self.state);
        }
        self.event_queue.dispatch_pending(&mut self.state)?;
        if self.state.drag == DragState::Cancelled {
            Ok(SelectionOutcome::Cancelled)
        } else {
            Ok(SelectionOutcome::Selected(self.state.selected_viewport()?))
        }
    }

    /// The frozen frames the session is displaying, in the form the PNG
    /// compositor wants.
    ///
    /// The session stored them as the without-cursor capture plus a cursor
    /// patch, and the caller already handed over the two full-frame sets it
    /// captured. Materialising the shown frame here — with the patch applied
    /// when the user asked for the cursor — keeps one source of truth for what
    /// was on screen, and costs one frame's pixels once, at the end.
    pub fn frozen_captures_for_output(&self) -> Vec<CapturedOutput> {
        let with_cursor = !self.state.screenshot_cursor_hidden;
        self.state
            .frozen_outputs
            .iter()
            .map(|frozen| CapturedOutput {
                output_name: frozen.info.global_name,
                image: frozen.to_image(with_cursor),
            })
            .collect()
    }

    pub fn dispatch_poll(&mut self, timeout_ms: i32) -> Result<(), Box<dyn Error>> {
        if self.event_queue.dispatch_pending(&mut self.state)? > 0 {
            // The events just dispatched can have asked for a redraw (a pointer
            // drag, a key); this is the only place that can honour the request
            // while `run_selection` is waiting for a timeout.
            flush_render_request(&mut self.state);
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
        flush_render_request(&mut self.state);
        Ok(())
    }

    pub fn close(self) -> Result<(), Box<dyn Error>> {
        drop(self.state);
        drop(self.event_queue);
        let _ = self.conn.flush();
        Ok(())
    }
}

/// The interactive selection loop runs until the drag is finished or cancelled.
fn selection_loop_active(state: &UiState) -> bool {
    !state.drag.is_terminal()
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

fn digit_key_index(key: u32) -> Option<usize> {
    if (KEY_DIGIT_FIRST..=KEY_DIGIT_LAST).contains(&key) {
        Some((key - KEY_DIGIT_FIRST) as usize)
    } else {
        None
    }
}

/// Decisions the keyboard can make over pure selection state. The Wayland
/// dispatch impl turns these into side effects; keeping the decision pure
/// makes the transitions unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Cancel,
    ToggleCursorFrame,
    CaptureOutputByIndex(usize),
    /// `0`: the whole desktop, every output in one image.
    CaptureAllOutputs,
    Ignore,
}

fn resolve_key_action(state: &UiState, key: u32) -> KeyAction {
    if key == KEY_ESC {
        KeyAction::Cancel
    } else if key == KEY_H {
        KeyAction::ToggleCursorFrame
    } else if state.drag == DragState::Idle {
        if key == KEY_ZERO {
            KeyAction::CaptureAllOutputs
        } else {
            digit_key_index(key).map_or(KeyAction::Ignore, KeyAction::CaptureOutputByIndex)
        }
    } else {
        KeyAction::Ignore
    }
}

fn apply_key_action(state: &mut UiState, action: KeyAction) {
    match action {
        KeyAction::Cancel => state.drag = DragState::Cancelled,
        KeyAction::ToggleCursorFrame => toggle_frozen_frame(state),
        KeyAction::CaptureOutputByIndex(index) => {
            if let Some(rect) = full_output_rect_by_index(&state.output_infos(), index) {
                state.drag = DragState::Finished(rect);
            }
        }
        KeyAction::CaptureAllOutputs => {
            if let Some(rect) = desktop_bounds(&state.output_infos()) {
                state.drag = DragState::Finished(rect);
            }
        }
        KeyAction::Ignore => {}
    }
}

/// The bounding box of every output, i.e. "the whole desktop".
///
/// `0` uses it as the selection rectangle; `SelectedViewport` then splits it
/// into per-output pieces, which are laid out end to end.
fn desktop_bounds(infos: &[OutputInfo]) -> Option<LogicalRect> {
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

fn full_output_rect_by_index(output_infos: &[OutputInfo], index: usize) -> Option<LogicalRect> {
    let mut ordered: Vec<&OutputInfo> = output_infos.iter().collect();
    ordered.sort_by_key(|info| (info.logical.x, info.logical.y));
    Some(ordered.get(index)?.logical)
}

fn toggle_frozen_frame(state: &mut UiState) {
    state.screenshot_cursor_hidden = !state.screenshot_cursor_hidden;
    for index in 0..state.overlays.len() {
        // Only the overlay's buffers need invalidating: the frozen pixels are
        // read from the stored capture on every draw, so switching frames is
        // switching whether `frozen_pixel` applies the cursor patch. There is
        // no second frame to rebuild, which is what used to make `H` cost a
        // whole 8.3 MB rebuild per output.
        let overlay = &mut state.overlays[index];
        for buffer in &mut overlay.buffers {
            buffer.initialized = false;
            buffer.last_selected = None;
        }
    }
    queue_render(state);
}

/// Renders now, and remembers the request when an overlay had no free buffer.
///
/// The debt is what makes a redraw survive a busy buffer set: it is retried
/// from `wl_buffer.release` (`flush_pending_render`), which is the next moment
/// a redraw can succeed.
/// One frame per display refresh.
const OVERLAY_FRAME_INTERVAL: Duration = Duration::from_micros(16_000);

/// Ask for a redraw without drawing it yet.
fn request_render(state: &mut UiState) {
    state.render_requested = true;
}

/// Draw at most one frame per [`OVERLAY_FRAME_INTERVAL`]. A request that is too
/// early stays pending, so the next frame shows the latest position rather than
/// being dropped.
fn flush_render_request(state: &mut UiState) {
    if !state.render_requested {
        return;
    }
    let now = std::time::Instant::now();
    if state
        .last_render
        .is_some_and(|last| now.duration_since(last) < OVERLAY_FRAME_INTERVAL)
    {
        return;
    }
    state.last_render = Some(now);
    state.render_requested = false;
    queue_render(state);
}

/// Renders now, and remembers a shortfall when an overlay had no free buffer.
///
/// The debt is what makes a redraw survive a busy buffer set: it is retried
/// from `wl_buffer.release` (`flush_pending_render`), which is the next moment
/// a redraw can succeed.
fn queue_render(state: &mut UiState) {
    let committed = render_overlays(state);
    state.pending_render = committed < state.overlays.len();
}

/// Retries a queued render. Called on every buffer release so a render that
/// could not commit — every buffer of that output still held by the
/// compositor — goes out as soon as one comes back.
fn flush_pending_render(state: &mut UiState) {
    if !state.pending_render {
        return;
    }
    let committed = render_overlays(state);
    state.pending_render = committed < state.overlays.len();
    crate::trace_log!("flush_pending: committed={}/{}", committed, state.overlays.len());
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
    let frozen_capture = &state.frozen_outputs;
    // The stored frame *is* the without-cursor capture, so
    // `screenshot_cursor_hidden` (the default, and what `H` toggles) means "do
    // not apply the patch".
    let with_cursor = !state.screenshot_cursor_hidden;
    for overlay in &mut state.overlays {
        let Some(render_cache) = overlay.render_cache.as_ref() else {
            continue;
        };
        let Some(frozen) = frozen_from(frozen_capture, overlay.output_name) else {
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
        // The bands that actually changed, not their bounding box: a 1-pixel
        // step of an 800x500 box changes ~16 000 pixels, while the union of the
        // two rings is ~408 000. Compositing the box measured 93% of a drag's
        // CPU, almost all of it re-deriving pixels that were already correct.
        // The bands are derived from the box mapped into buffer pixels (its
        // edges floored), while the renderer classifies a buffer pixel by the
        // *logical* pixel it covers. On a fractionally scaled output the two
        // roundings can disagree by one buffer pixel at the box's edge, and a
        // band one pixel too small leaves a line of the previous box on screen
        // — the reported residue after moving the box. One buffer pixel of
        // outset costs the perimeter and makes the two agree; at an identity
        // scale the two mappings cannot disagree.
        let damage_outset = i32::from(
            overlay.width != output.info.logical.width || overlay.height != output.info.logical.height,
        );
        let damage: Vec<LogicalRect> = if buffer.initialized {
            changed_selection_rects(
                buffer.last_selected,
                current_selected,
                border_buffer_width,
                overlay.width,
                overlay.height,
            )
            .into_iter()
            .map(|band| outset_rect(band, damage_outset))
            .filter_map(|band| {
                band.intersection(LogicalRect {
                    x: 0,
                    y: 0,
                    width: overlay.width,
                    height: overlay.height,
                })
            })
            .collect()
        } else {
            vec![LogicalRect { x: 0, y: 0, width: overlay.width, height: overlay.height }]
        };
        if damage.is_empty() {
            continue;
        }
        if buffer.initialized {
            for band in &damage {
                draw_overlay_region(data, render_cache, output.info, frozen, with_cursor, rect, *band, border_width);
            }
        } else {
            draw_overlay(data, render_cache, output.info, frozen, with_cursor, rect, border_width);
            buffer.initialized = true;
        }
        buffer.last_selected = current_selected;
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        for band in &damage {
            overlay.surface.damage_buffer(band.x, band.y, band.width, band.height);
        }
        overlay.surface.commit();
        buffer.available = false;
        committed += 1;
    }
    committed
}

/// Builds the session's frozen frames from a with-cursor / without-cursor pair.
///
/// One capture of each output is kept whole — the **without-cursor** one, which
/// is what the overlay shows until `H` — and the with-cursor capture is reduced
/// to the pixels where the two differ. The pair is taken microseconds apart, so
/// the difference is the cursor overlay plus whatever blinked in between; both
/// are reproduced exactly.
///
/// A missing without-cursor capture falls back to keeping the with-cursor one
/// whole, which is worse for memory and identical on screen. A pair that does
/// not share a geometry does the same, per output: a wrong picture is a much
/// worse failure than a duplicated one.
fn frozen_outputs_from_captures(
    captures: &[CapturedOutput],
    without_cursor: &[CapturedOutput],
) -> Result<Vec<FrozenOutput>, Box<dyn Error>> {
    let mut frozen_outputs = Vec::with_capacity(captures.len());
    for capture in captures {
        let base = without_cursor
            .iter()
            .find(|other| other.output_name == capture.output_name);
        let (base_image, cursor_image) = match base {
            Some(base) => (&base.image, &capture.image),
            None => (&capture.image, &capture.image),
        };
        let frozen_base = frozen_output_from_image(capture.output_name, base_image)?;
        let cursor_frame = frozen_output_from_image(capture.output_name, cursor_image)?;
        let cursor_delta = CursorDelta::between(&frozen_base, &cursor_frame);
        frozen_outputs.push(FrozenOutput {
            cursor_delta,
            ..frozen_base
        });
    }
    Ok(frozen_outputs)
}

/// One capture, as the frozen frame the overlay draws from.
fn frozen_output_from_image(output_name: u32, image: &Image) -> Result<FrozenOutput, Box<dyn Error>> {
    if !matches!(image.format, Format::Xrgb8888 | Format::Argb8888) {
        return Err(format!("unsupported frozen output format for output {output_name}: {:?}", image.format).into());
    }
    if image.width == 0 || image.height == 0 {
        return Err(format!("frozen output {output_name} dimensions must be positive").into());
    }
    let minimum_stride = image.width.checked_mul(4).ok_or("frozen output stride overflow")?;
    if image.stride < minimum_stride {
        return Err(format!("frozen output {output_name} stride is smaller than width * 4").into());
    }
    let minimum_len = usize::try_from(image.stride)?.checked_mul(usize::try_from(image.height)?).ok_or("frozen output data length overflow")?;
    if image.data.len() < minimum_len {
        return Err(format!("frozen output {output_name} data is shorter than stride * height").into());
    }
    let width = i32::try_from(image.width)?;
    let height = i32::try_from(image.height)?;
    let stride = i32::try_from(image.stride)?;
    Ok(FrozenOutput {
        info: OutputInfo {
            global_name: output_name,
            logical: LogicalRect { x: 0, y: 0, width, height },
            scale: 1,
        },
        width,
        height,
        stride,
        data: image.data.clone(),
        cursor_delta: CursorDelta::EMPTY,
    })
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

impl FrozenOutput {
    /// This frame as an `Image`, optionally with the cursor patch applied.
    ///
    /// Allocates one frame, which is what the final composite needs anyway.
    fn to_image(&self, with_cursor: bool) -> Image {
        let bytes = usize::try_from(self.stride.max(0))
            .unwrap_or(0)
            .saturating_mul(usize::try_from(self.height.max(0)).unwrap_or(0));
        let mut data = vec![0u8; bytes];
        let keep = bytes.min(self.data.len());
        data[..keep].copy_from_slice(&self.data[..keep]);
        if with_cursor && !self.cursor_delta.is_empty() {
            for (y, pixels) in &self.cursor_delta.rows {
                let row_start = *y as usize * self.stride as usize;
                for (x, pixel) in pixels {
                    let offset = row_start + *x as usize * 4;
                    if offset + 4 <= data.len() {
                        data[offset..offset + 4].copy_from_slice(pixel);
                    }
                }
            }
        }
        Image {
            width: self.width as u32,
            height: self.height as u32,
            stride: self.stride as u32,
            format: Format::Argb8888,
            data,
        }
    }
}

/// The frozen frame for one output, from the session's single stored list.
fn frozen_from(captures: &[FrozenOutput], output_name: u32) -> Option<&FrozenOutput> {
    captures
        .iter()
        .find(|frozen| frozen.info.global_name == output_name)
}

/// One pixel of the frozen frame, as the overlay should show it.
///
/// `with_cursor` selects between the two captures the session was started with:
/// the stored frame is the without-cursor one, and the with-cursor capture is
/// its `cursor_delta`. Applying the patch here rather than storing a second
/// frame is what makes `H` free and the second capture almost free in memory.
fn frozen_pixel(
    frozen: &FrozenOutput,
    overlay_width: i32,
    overlay_height: i32,
    local_x: i32,
    local_y: i32,
    with_cursor: bool,
) -> [u8; 4] {
    let pixel = frozen_base_pixel(frozen, overlay_width, overlay_height, local_x, local_y);
    if !with_cursor || frozen.cursor_delta.is_empty() {
        return pixel;
    }
    // The patch is indexed in capture pixels, so it has to be sampled the same
    // way the base frame was — through the same buffer-to-capture map.
    let source_x = ((local_x as i64 * frozen.width as i64) / overlay_width.max(1) as i64) as i32;
    let source_y = ((local_y as i64 * frozen.height as i64) / overlay_height.max(1) as i64) as i32;
    let Some(rows) = frozen.cursor_delta.row(source_y.max(0) as u32) else {
        return pixel;
    };
    match rows.binary_search_by_key(&(source_x.max(0) as u32), |(x, _)| *x) {
        Ok(index) => rows[index].1,
        Err(_) => pixel,
    }
}

fn frozen_base_pixel(frozen: &FrozenOutput, overlay_width: i32, overlay_height: i32, local_x: i32, local_y: i32) -> [u8; 4] {
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

fn build_overlay_render_cache(width: i32, height: i32) -> OverlayRenderCache {
    OverlayRenderCache { width, height }
}

fn composited_overlay_pixel(base: [u8; 4], global_x: i32, global_y: i32, selected: Option<LogicalRect>, border_width: i32) -> [u8; 4] {
    let overlay = overlay_pixel(global_x, global_y, selected, border_width);
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

fn draw_overlay(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, frozen: &FrozenOutput, with_cursor: bool, selected: Option<LogicalRect>, border_width: i32) {
    draw_overlay_region(data, cache, output, frozen, with_cursor, selected, LogicalRect { x: 0, y: 0, width: cache.width, height: cache.height }, border_width);
}

/// Recomposites the damaged region of a frozen overlay.
///
/// Everything that depends on only one axis is hoisted out of the inner loop:
/// the two coordinate mappings, the capture column the buffer column samples,
/// and the capture row a buffer row samples. They are what made this function
/// dominate a drag — `perf` put 28% of a drag's CPU here, split between two
/// 64-bit divisions, two `clamp`s and a bounds-checked slice per *pixel*, all of
/// which are the same value for a whole row or column.
///
/// The cursor patch is looked up once per row rather than binary-searched per
/// pixel. It touches 1 440–2 706 pixels of 2 073 600 in practice, so almost every
/// row has no patch at all and skips that work entirely.
#[allow(clippy::too_many_arguments)]
fn draw_overlay_region(data: &mut [u8], cache: &OverlayRenderCache, output: OutputInfo, frozen: &FrozenOutput, with_cursor: bool, selected: Option<LogicalRect>, dirty: LogicalRect, border_width: i32) {
    if cache.width <= 0 || cache.height <= 0 {
        return;
    }
    let width = cache.width as usize;
    // Buffer pixels are not logical pixels on a fractionally scaled output: the
    // buffer is the frozen capture's pixel grid, the selection lives in global
    // logical coordinates. Map before comparing, otherwise the hole and the
    // border land at `scale` times their position.
    let logical = output.logical;
    let dirty = dirty.intersection(LogicalRect { x: 0, y: 0, width: cache.width, height: cache.height });
    let Some(dirty) = dirty else { return };
    if dirty.width <= 0 || dirty.height <= 0 {
        return;
    }

    let source_stride = frozen.stride.max(0) as usize;
    let classifier = SelectionClassifier::new(selected, border_width);

    // The two mappings are identities in the normal case — the buffer is the
    // capture's own grid, and the surface is the output's logical size — and that
    // is worth branching for once per frame rather than paying two 64-bit
    // divisions per pixel: `perf` put 14% of a drag in `sample_index` alone.
    let identity_scale = frozen.width == cache.width
        && frozen.height == cache.height
        && logical.width == cache.width
        && logical.height == cache.height;

    for local_y in dirty.y..dirty.bottom() {
        let global_y = if identity_scale {
            logical.y + local_y
        } else {
            logical.y + (i64::from(local_y) * i64::from(logical.height.max(1)) / i64::from(cache.height)) as i32
        };
        let source_row = if identity_scale {
            local_y as usize
        } else {
            sample_index(local_y, cache.height, frozen.height)
        };
        let row_start = source_row * source_stride;
        // The whole capture row, when it exists. `None` means the stored frame
        // is too short — read as black rather than panicking, as before.
        let source_row_bytes = frozen.data.get(row_start..row_start + source_stride);
        // The cursor patch for this row, sorted by x. `None` on almost every
        // row, which is what keeps the common path free of the search.
        let patch = if with_cursor && !frozen.cursor_delta.is_empty() {
            frozen.cursor_delta.row(source_row as u32)
        } else {
            None
        };
        let dst_row = local_y as usize * width;
        for local_x in dirty.x..dirty.right() {
            let (global_x, source_column) = if identity_scale {
                (logical.x + local_x, local_x as usize * 4)
            } else {
                (
                    logical.x + (i64::from(local_x) * i64::from(logical.width.max(1)) / i64::from(cache.width)) as i32,
                    sample_index(local_x, cache.width, frozen.width) * 4,
                )
            };
            let offset = (dst_row + local_x as usize) * 4;
            if offset + 4 > data.len() {
                return;
            }
            let pixel = match (source_row_bytes, patch) {
                (Some(row), None) => match row.get(source_column..source_column + 4) {
                    Some(pixel) => [pixel[0], pixel[1], pixel[2], 255],
                    None => [0, 0, 0, 255],
                },
                _ => frozen_pixel(frozen, cache.width, cache.height, local_x, local_y, with_cursor),
            };
            match classifier.classify(global_x, global_y) {
                OVERLAY_SELECTED_TRANSPARENT_BGRA => {
                    data[offset..offset + 4].copy_from_slice(&pixel);
                }
                OVERLAY_OUTSIDE_MASK_BGRA => {
                    let dimmed = composited_overlay_pixel(pixel, global_x, global_y, None, border_width);
                    data[offset..offset + 4].copy_from_slice(&dimmed);
                }
                border => {
                    data[offset..offset + 4].copy_from_slice(&border);
                }
            }
        }
    }
}

/// Capture index that buffer index `local` samples along one axis.
///
/// The same expression `frozen_base_pixel` uses, in the form that can be hoisted
/// out of a pixel loop: one division, one clamp, no `Option` chain.
fn sample_index(local: i32, buffer_extent: i32, capture_extent: i32) -> usize {
    if buffer_extent <= 0 || capture_extent <= 0 {
        return 0;
    }
    let index = ((i64::from(local) * i64::from(capture_extent)) / i64::from(buffer_extent)) as i32;
    index.clamp(0, capture_extent - 1) as usize
}

/// Per-frame classification of a pixel against the selection.
///
/// `overlay_pixel` recomputes `border_bounds` on every call — four saturating
/// adds that depend only on the box — and it is called once per pixel of the
/// damaged region. `perf` put 12% of a drag there. The bounds are the same for
/// the whole frame, so they belong here, computed once.
struct SelectionClassifier {
    selected: Option<LogicalRect>,
    outer: LogicalRect,
}

impl SelectionClassifier {
    fn new(selected: Option<LogicalRect>, border_width: i32) -> Self {
        Self {
            selected,
            outer: selected.map(|rect| border_bounds(rect, border_width)).unwrap_or(LogicalRect { x: 0, y: 0, width: 0, height: 0 }),
        }
    }

    #[inline]
    fn classify(&self, x: i32, y: i32) -> [u8; 4] {
        let Some(rect) = self.selected else {
            return OVERLAY_OUTSIDE_MASK_BGRA;
        };
        let inside = |r: LogicalRect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        // Outside the box but inside its ring: border.
        if !rect.is_empty() && inside(self.outer) && !inside(rect) {
            return OVERLAY_BORDER_WHITE_BGRA;
        }
        if !inside(rect) {
            return OVERLAY_OUTSIDE_MASK_BGRA;
        }
        OVERLAY_SELECTED_TRANSPARENT_BGRA
    }
}

fn overlay_pixel(global_x: i32, global_y: i32, selected: Option<LogicalRect>, border_width: i32) -> [u8; 4] {
    let Some(rect) = selected else {
        return OVERLAY_OUTSIDE_MASK_BGRA;
    };
    if let Some(border) = selection_border_pixel(global_x, global_y, rect, border_width) {
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
/// A global rectangle as output-local coordinates, clipped to that output.
fn selected_local_intersection(output: LogicalRect, selected: LogicalRect) -> Option<LogicalRect> {
    selected.intersection(output).map(|intersection| LogicalRect {
        x: intersection.x - output.x,
        y: intersection.y - output.y,
        width: intersection.width,
        height: intersection.height,
    })
}

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

/// `a` minus `b`, as up to four disjoint rectangles.
///
/// Used to turn "the border ring moved" into the handful of bands that actually
/// changed instead of their bounding box. A 1-pixel step of an 800x500 box
/// changes about 2 000 pixels; the bounding box of the two rings is 408 000 —
/// 200 times the work for the same result, and it was 93% of a drag's CPU.
fn rect_difference(a: LogicalRect, b: LogicalRect) -> Vec<LogicalRect> {
    if a.is_empty() {
        return Vec::new();
    }
    let Some(overlap) = a.intersection(b) else {
        return vec![a];
    };
    let mut parts = Vec::with_capacity(4);
    let mut push = |x: i32, y: i32, width: i32, height: i32| {
        if width > 0 && height > 0 {
            parts.push(LogicalRect { x, y, width, height });
        }
    };
    // Above, below, left, right of the overlap. Disjoint by construction, and
    // together they are exactly `a` outside `b`.
    push(a.x, a.y, a.width, overlap.y - a.y);
    push(a.x, overlap.bottom(), a.width, a.bottom() - overlap.bottom());
    push(a.x, overlap.y, overlap.x - a.x, overlap.height);
    push(overlap.right(), overlap.y, a.right() - overlap.right(), overlap.height);
    parts
}

/// The ring of a box: its outer bounds, minus the box itself, as four bands.
///
/// Four disjoint rectangles rather than "outer minus inner", so a difference
/// between two frames can be computed band by band without an area sweep.
fn ring_bands(rect: LogicalRect, thickness: i32) -> [LogicalRect; 4] {
    let outer = border_bounds(rect, thickness);
    let top = (outer.y..rect.y).len() as i32;
    let bottom_start = rect.bottom();
    [
        // Above the box.
        LogicalRect { x: outer.x, y: outer.y, width: outer.width, height: top },
        // Below the box.
        LogicalRect {
            x: outer.x,
            y: bottom_start,
            width: outer.width,
            height: outer.bottom() - bottom_start,
        },
        // Left of the box.
        LogicalRect { x: outer.x, y: rect.y, width: rect.x - outer.x, height: rect.height },
        // Right of the box.
        LogicalRect {
            x: rect.right(),
            y: rect.y,
            width: outer.right() - rect.right(),
            height: rect.height,
        },
    ]
}

/// Everything that can have changed on screen between two selection states.
///
/// Two kinds of pixels differ: the ring is drawn in a new place (so both its old
/// and its new position change), and where the box edge swept, the hole's
/// inside/outside flipped — the capture inside it is the same, so only the strip
/// between the old and the new edge changes.
///
/// The result is a handful of thin bands, not a bounding box: proportional to the
/// border's perimeter plus the distance moved, instead of to the box's area.
fn changed_selection_rects(
    previous: Option<LogicalRect>,
    current: Option<LogicalRect>,
    thickness: i32,
    width: i32,
    height: i32,
) -> Vec<LogicalRect> {
    let clamp = LogicalRect { x: 0, y: 0, width, height };
    let mut changed = Vec::new();
    match (previous, current) {
        (Some(previous), Some(current)) => {
            for band in ring_bands(previous, thickness) {
                changed.extend(rect_difference(band, current));
            }
            for band in ring_bands(current, thickness) {
                changed.extend(rect_difference(band, previous));
            }
            // The hole: what was inside the old box and is outside the new one,
            // and the reverse. Identical pixels, only the covering flag differs.
            changed.extend(rect_difference(previous, current));
            changed.extend(rect_difference(current, previous));
        }
        (Some(only), None) | (None, Some(only)) => {
            for band in ring_bands(only, thickness) {
                changed.push(band);
            }
            changed.push(only);
        }
        (None, None) => {}
    }
    changed
        .into_iter()
        .filter_map(|rect| rect.intersection(clamp))
        .filter(|rect| !rect.is_empty())
        .collect()
}

/// Grows a rectangle by `by` pixels on every side.
fn outset_rect(rect: LogicalRect, by: i32) -> LogicalRect {
    LogicalRect {
        x: rect.x - by,
        y: rect.y - by,
        width: rect.width + by * 2,
        height: rect.height + by * 2,
    }
}

/// Overlay buffer size in pixels.
///
/// Hard lower bounds, in the order they are known:
///
/// - the frozen image that is about to be displayed (we must not resample our
///   own screenshot before the compositor shows it);
/// - the output's density from `wp_fractional_scale_v1`, when the compositor
///   advertises it (180 = 1.5), otherwise the compositor upscales the overlay.
///
/// The capture wins: it is what we display, so its grid is the one to match.
/// Without `wp_viewporter` a buffer larger than the surface cannot be mapped
/// back, so the logical size wins.
fn overlay_buffer_size(
    logical: (i32, i32),
    capture: Option<(i32, i32)>,
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
    let (capture_width, capture_height) = capture.unwrap_or((0, 0));
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
    if !state.outputs.iter().any(|output| output.info.global_name == output_name) {
        return;
    }
    let frozen = frozen_from(&state.frozen_outputs, output_name);
    let shm = state.shm.clone();
    let viewporter = state.viewporter.clone();
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
        preferred_scale,
        viewporter.is_some(),
    );
    let resized = (buffer_width, buffer_height)
        != (state.overlays[index].width, state.overlays[index].height)
        || state.overlays[index].buffers.len() != OVERLAY_BUFFER_COUNT;
    if resized {
        let Some(shm) = shm else {
            return;
        };
        match create_overlay_buffers(&shm, output_name, buffer_width, buffer_height, qh) {
            Ok(buffers) => {
                let overlay = &mut state.overlays[index];
                overlay.buffers = buffers;
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
                #[cfg(not(feature = "trace-logs"))]
                let _ = &err;
                state.drag = DragState::Cancelled;
                return;
            }
        }
    }
    // The cache is geometry-only now, so it is built whenever there is a frozen
    // frame to draw and the size changed — no pixels are copied to build it.
    if frozen.is_some() {
        let cache_matches = state.overlays[index]
            .render_cache
            .as_ref()
            .is_some_and(|cache| cache.width == buffer_width && cache.height == buffer_height);
        if !cache_matches {
            state.overlays[index].render_cache =
                Some(build_overlay_render_cache(buffer_width, buffer_height));
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
    // `wl_shm.create_pool` takes an i32 byte count. Above 2 GiB (~537
    // megapixels) the cast would wrap to a negative length, so refuse
    // rather than hand the compositor a bogus size.
    let pool_size = i32::try_from(size).map_err(|_| "shm buffer exceeds 2 GiB")?;
    let pool = shm.create_pool(fd.as_fd(), pool_size, qh, ());
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
                    position: OutputPositionSource::default(),
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
                if state.drag.update(x, y) {
                    request_render(state);
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
                    wl_pointer::ButtonState::Pressed => state.drag.begin(x, y),
                    wl_pointer::ButtonState::Released => state.drag.finish(x, y),
                    _ => {}
                }
                // Queue (rather than render directly): when one output's
                // buffers are all busy a direct render leaves that output
                // showing stale content — one screen fresh, the other stale
                // during a spanning drag. queue_render re-renders on buffer
                // release. Coalesced with any motion in the same turn.
                request_render(state);
            }
            // Wheel events are ignored: the frozen screen is what is captured,
            // and the overlay is not a scroll surface.
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
            let _ = qh;
            apply_key_action(state, action);
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
            wl_output::Event::Geometry { x, y, .. } => {
                output.position.set_geometry(x, y);
                output.resolve_logical_position();
            }
            // Without xdg_output the mode is the only size there is; it is
            // the pixel size, which is what this fallback always assumes.
            wl_output::Event::Mode { width, height, .. }
                if output.info.logical.width <= 1 && output.info.logical.height <= 1 =>
            {
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
                output.position.set_logical(x, y);
                output.resolve_logical_position();
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
        queue_render(state);
    }
}
delegate_noop!(UiState: ignore WlRegion);
delegate_noop!(UiState: ignore WlShmPool);

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTED: LogicalRect = LogicalRect { x: 10, y: 20, width: 30, height: 40 };


    #[test]
    fn overlay_buffer_size_never_shrinks_below_content_or_density() {
        // 1920x1080 output at 1.5 reports a 1280x720 logical size; the frozen
        // capture is 1920x1080 and the hint is 180 (120ths).
        assert_eq!(overlay_buffer_size((1280, 720), Some((1920, 1080)), 180, true), (1920, 1080));
        // A capture at the logical size still gets the density hint's pixels.
        assert_eq!(overlay_buffer_size((1280, 720), Some((1280, 720)), 150, true), (1600, 900));
        assert_eq!(overlay_buffer_size((1280, 720), Some((1280, 720)), 120, true), (1280, 720));
        // A capture denser than the hint wins: the image we display must never
        // be resampled on its way to the screen.
        assert_eq!(overlay_buffer_size((1280, 720), Some((2560, 1440)), 180, true), (2560, 1440));
        // Without wp_viewporter a larger buffer cannot be mapped back.
        assert_eq!(overlay_buffer_size((1280, 720), Some((1920, 1080)), 180, false), (1280, 720));
    }

    #[test]
    fn frozen_overlay_maps_buffer_pixels_through_the_logical_grid() {
        // 4x4 logical output, 6x6 buffer (1.5x). A selection at global
        // (1,1)-(3,3) must be the hole at buffer [2,4), not [1,3).
        let output_info = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 4, height: 4 }, scale: 2 };
        let frozen = frozen_test_output(output_info, 6, 6, [10, 20, 30, 255]);
        let cache = build_overlay_render_cache(6, 6);
        let mut data = vec![0u8; 6 * 6 * 4];
        let selected = LogicalRect { x: 1, y: 1, width: 2, height: 2 };
        draw_overlay(&mut data, &cache, output_info, &frozen, false, Some(selected), 1);

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

    #[test]
    fn the_render_selection_is_the_current_box() {
        let mut state = ui_state();
        assert_eq!(ui_render_selection(&state), None);
        state.drag = DragState::Finished(SELECTED);
        assert_eq!(ui_render_selection(&state), Some(SELECTED));
        state.drag = DragState::Cancelled;
        assert_eq!(ui_render_selection(&state), None);
    }

    #[test]
    fn digit_key_maps_to_zero_based_index() {
        assert_eq!(digit_key_index(KEY_DIGIT_FIRST), Some(0));
        assert_eq!(digit_key_index(KEY_DIGIT_LAST), Some(8));
        assert_eq!(digit_key_index(KEY_DIGIT_LAST + 1), None);
        assert_eq!(digit_key_index(KEY_H), None);
    }

    /// `0` is the whole desktop: the union of every output rectangle. The
    /// digit keys next to it stay per-output, and both only work before a
    /// selection exists (a finished box is not replaced by a key press).
    #[test]
    fn zero_key_selects_every_screen_as_one_image() {
        let mut state = ui_state();
        assert_eq!(resolve_key_action(&state, KEY_ZERO), KeyAction::CaptureAllOutputs);
        // Without outputs there is no rectangle to select; the state is left
        // alone rather than finishing an empty selection.
        apply_key_action(&mut state, KeyAction::CaptureAllOutputs);
        assert_eq!(state.drag, DragState::Idle);
        // The rect is the bounding box of the outputs.
        let left = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 20, width: 100, height: 80 }, scale: 1 };
        let right = OutputInfo { global_name: 2, logical: LogicalRect { x: 100, y: 0, width: 120, height: 90 }, scale: 1 };
        assert_eq!(
            desktop_bounds(&[right, left]),
            Some(LogicalRect { x: 0, y: 0, width: 220, height: 100 })
        );
        // An in-progress or finished selection rejects it, exactly like the
        // 1-9 keys.
        state.drag = DragState::Finished(SELECTED);
        assert_eq!(resolve_key_action(&state, KEY_ZERO), KeyAction::Ignore);
        state.drag = DragState::Dragging { origin_x: 1, origin_y: 1, current_x: 2, current_y: 2 };
        assert_eq!(resolve_key_action(&state, KEY_ZERO), KeyAction::Ignore);
    }

    fn ui_state() -> UiState {
        UiState::unbound(Vec::new(), crate::config::DEFAULT_BORDER_WIDTH)
    }

    /// `zxdg_output_v1` is the layout authority; `wl_output.geometry` only
    /// covers a compositor that does not speak it.
    #[test]
    fn an_output_falls_back_to_geometry_position_without_xdg_output() {
        // niri and anything else with the xdg output manager: logical wins.
        let mut position = OutputPositionSource::default();
        position.set_logical(1920, 0);
        position.set_geometry(3840, 0);
        assert_eq!(position.resolve(), Some((1920, 0)));

        // Without xdg_output, geometry is all there is — and the alternative is
        // leaving every output at the (0, 0) placeholder, which makes a
        // two-screen desktop one rectangle and every selection look spanning.
        let mut position = OutputPositionSource::default();
        position.set_geometry(1920, 0);
        assert_eq!(position.resolve(), Some((1920, 0)));

        // Neither has arrived yet: leave the seeded placeholder alone.
        assert_eq!(OutputPositionSource::default().resolve(), None);
    }

    /// The order the two events arrive in must not decide the answer: a
    /// `geometry` landing *after* a `logical_position` must not overwrite it.
    #[test]
    fn a_later_geometry_event_does_not_override_the_logical_position() {
        let mut position = OutputPositionSource::default();
        position.set_logical(1920, 0);
        assert_eq!(position.resolve(), Some((1920, 0)));
        // …then wl_output.geometry, reporting a different (pixel) origin.
        position.set_geometry(3840, 0);
        assert_eq!(
            position.resolve(),
            Some((1920, 0)),
            "geometry must stay the fallback, not become a second authority"
        );
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
        assert_eq!(overlay_pixel(6, 20, Some(SELECTED), 3), OVERLAY_OUTSIDE_MASK_BGRA);
        assert_eq!(overlay_pixel(10, 16, Some(SELECTED), 3), OVERLAY_OUTSIDE_MASK_BGRA);
    }

    #[test]
    fn overlay_pixel_keeps_selection_interior_transparent() {
        assert_eq!(overlay_pixel(13, 23, Some(SELECTED), 3), OVERLAY_SELECTED_TRANSPARENT_BGRA);
    }

    #[test]
    fn overlay_pixel_keeps_selection_edge_clear() {
        assert_eq!(overlay_pixel(10, 20, Some(SELECTED), 3), OVERLAY_SELECTED_TRANSPARENT_BGRA);
    }

    #[test]
    fn overlay_pixel_draws_white_adjacent_border() {
        assert_eq!(overlay_pixel(9, 19, Some(SELECTED), 3), OVERLAY_BORDER_WHITE_BGRA);
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

    /// The band decomposition must cover exactly the pixels that changed.
    ///
    /// Checked against a brute-force per-pixel reference over a small canvas:
    /// for every pixel, "did the drawn result differ between the two states" is
    /// computed directly from `overlay_pixel`, and the bands must cover all of
    /// those and nothing else that is unnecessary (they may cover pixels that
    /// happen to be equal, but not miss any that differ).
    #[test]
    fn changed_selection_rects_covers_exactly_what_differs() {
        let (width, height, thickness) = (40, 30, 2);
        let states = [
            Some(LogicalRect { x: 4, y: 4, width: 12, height: 8 }),
            Some(LogicalRect { x: 5, y: 4, width: 12, height: 8 }),
            Some(LogicalRect { x: 5, y: 5, width: 12, height: 8 }),
            Some(LogicalRect { x: 5, y: 5, width: 6, height: 4 }),
            Some(LogicalRect { x: 0, y: 0, width: 39, height: 29 }),
            None,
        ];
        for pair in states.windows(2) {
            let (previous, current) = (pair[0], pair[1]);
            let bands = changed_selection_rects(previous, current, thickness, width, height);
            for y in 0..height {
                for x in 0..width {
                    let before = overlay_pixel(x, y, previous, thickness);
                    let after = overlay_pixel(x, y, current, thickness);
                    if before == after {
                        continue;
                    }
                    let covered = bands.iter().any(|band| {
                        x >= band.x && x < band.right() && y >= band.y && y < band.bottom()
                    });
                    assert!(
                        covered,
                        "pixel ({x},{y}) changed from {previous:?} to {current:?} but no band covers it"
                    );
                }
            }
        }
    }

    /// The point of the decomposition: a 1-pixel step must cost the perimeter,
    /// not the area.
    #[test]
    fn a_one_pixel_step_damages_a_strip_not_the_whole_box() {
        let thickness = 3;
        let previous = LogicalRect { x: 400, y: 300, width: 800, height: 500 };
        let current = LogicalRect { x: 401, y: 300, width: 800, height: 500 };
        let bands = changed_selection_rects(Some(previous), Some(current), thickness, 1920, 1080);
        let area: i64 = bands.iter().map(|band| band.area()).sum();
        let a = border_bounds(previous, thickness);
        let b = border_bounds(current, thickness);
        let union_rect = (a.right().max(b.right()) - a.x.min(b.x)) as i64
            * (a.bottom().max(b.bottom()) - a.y.min(b.y)) as i64;
        // Measured: 15 672 px against 408 342 px, a 26x reduction. The floor is
        // the ring's own perimeter (2 * (800 + 500) * 3 px), which is the work a
        // correct redraw of a moved border cannot avoid.
        assert!(
            area < union_rect / 20,
            "bands cover {area} px against {union_rect} px for the bounding box"
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
        let cache = build_overlay_render_cache(output.width, output.height);

        draw_overlay(&mut data, &cache, output_info, &frozen, false, Some(selected), 3);

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
        let bands = changed_selection_rects(
            selected_local_intersection(output, previous),
            selected_local_intersection(output, current),
            3,
            output.width,
            output.height,
        );
        let mut data = vec![0; (output.width * output.height * 4) as usize];

        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let frozen_pixel = [10, 20, 30, 255];
        let frozen = frozen_test_output(output_info, output.width, output.height, frozen_pixel);
        let cache = build_overlay_render_cache(output.width, output.height);

        draw_overlay(&mut data, &cache, output_info, &frozen, false, Some(previous), 3);
        for band in &bands {
            draw_overlay_region(&mut data, &cache, output_info, &frozen, false, Some(current), *band, 3);
        }

        assert_eq!(pixel_at(&data, output.width as usize, 1, 1), OVERLAY_BORDER_WHITE_BGRA);
        assert_eq!(pixel_at(&data, output.width as usize, 2, 2), OVERLAY_BORDER_WHITE_BGRA);
    }

    /// The cache holds geometry, not pixels.
    ///
    /// It used to keep two whole buffers — the frozen frame and its dimmed
    /// version — which was 8.1 MB per output per layer at 1920x1080 and rebuilt
    /// in full on every `H`. Both are derived from the one frozen capture, so
    /// this pins that a draw produces exactly the same pixels without them.
    #[test]
    fn the_render_cache_holds_no_pixels_but_draws_the_frozen_frame() {
        let output = LogicalRect { x: 0, y: 0, width: 2, height: 1 };
        let output_info = OutputInfo { global_name: 1, logical: output, scale: 1 };
        let mut frozen = frozen_test_output(output_info, output.width, output.height, [10, 20, 30, 255]);
        frozen.data[4..8].copy_from_slice(&[40, 50, 60, 255]);

        let cache = build_overlay_render_cache(output.width, output.height);
        // Nothing pixel-sized is retained.
        assert_eq!(cache.width, output.width);
        assert_eq!(cache.height, output.height);
        assert_eq!(std::mem::size_of::<OverlayRenderCache>(), 8);

        // A selection covering nothing leaves the frozen frame untouched.
        let mut data = vec![0u8; (output.width * output.height * 4) as usize];
        draw_overlay(&mut data, &cache, output_info, &frozen, false, None, 3);
        // `None` means the whole output is dimmed; with no selection the border
        // is drawn over it, so compare the inside against the dimmed value the
        // cache used to precompute.
        let dimmed_left = composited_overlay_pixel([10, 20, 30, 255], 0, 0, None, 3);
        assert_eq!(pixel_at(&data, 2, 0, 0), dimmed_left);
        let dimmed_right = composited_overlay_pixel([40, 50, 60, 255], 1, 0, None, 3);
        assert_eq!(pixel_at(&data, 2, 1, 0), dimmed_right);
    }

    /// The with-cursor capture is stored as a patch on the without-cursor one.
    ///
    /// Measured on a real 1920x1080 output the two differ by 1 440–2 706 pixels
    /// of 2 073 600 (0.07–0.13%), so this is a mouse-pointer-sized patch instead
    /// of a second 8.3 MB frame — and the second output, where the cursor is not,
    /// costs nothing at all.
    #[test]
    fn the_cursor_frame_is_stored_as_a_patch_on_the_frozen_one() {
        let info = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: 0, y: 0, width: 8, height: 4 },
            scale: 1,
        };
        let base = frozen_test_output(info, 8, 4, [10, 10, 10, 255]);
        let mut cursor = frozen_test_output(info, 8, 4, [10, 10, 10, 255]);
        // A 2x2 cursor at (3,1), which is all the two captures differ by.
        for y in 1..3 {
            for x in 3..5 {
                let offset = (y * 8 + x) * 4;
                cursor.data[offset..offset + 4].copy_from_slice(&[200, 200, 200, 255]);
            }
        }

        let delta = CursorDelta::between(&base, &cursor);
        assert_eq!(delta.bounds, Some((3, 1, 2, 2)));
        assert_eq!(delta.rows.len(), 2, "only the two cursor rows carry a patch");
        assert!(
            delta.bytes <= 4 * std::mem::size_of::<(u32, [u8; 4])>(),
            "the patch is the cursor's pixels, got {} bytes",
            delta.bytes
        );

        // Drawing with the patch reproduces the with-cursor frame exactly…
        let base_pixels = base.data.clone();
        let with_patch = FrozenOutput {
            cursor_delta: delta,
            ..base
        };
        for y in 0..4 {
            for x in 0..8 {
                assert_eq!(
                    frozen_pixel(&with_patch, 8, 4, x, y, true),
                    frozen_base_pixel(&cursor, 8, 4, x, y),
                    "with-cursor pixel ({x},{y})"
                );
                // …and without it, the frame the session stored.
                let offset = (y * 8 + x) as usize * 4;
                let stored = &base_pixels[offset..offset + 4];
                assert_eq!(
                    frozen_pixel(&with_patch, 8, 4, x, y, false),
                    [stored[0], stored[1], stored[2], stored[3]],
                    "without-cursor pixel ({x},{y})"
                );
            }
        }
    }

    /// A pair that does not share a geometry records no patch at all.
    ///
    /// The overlay then keeps showing the base frame, which is what the user
    /// selected minus the cursor. Showing a mis-mapped patch would put garbage
    /// on screen; a missing cursor is not a wrong capture.
    #[test]
    fn a_cursor_patch_needs_a_matching_geometry() {
        let info = OutputInfo {
            global_name: 1,
            logical: LogicalRect { x: 0, y: 0, width: 4, height: 4 },
            scale: 1,
        };
        let base = frozen_test_output(info, 4, 4, [10, 10, 10, 255]);
        let other = frozen_test_output(info, 2, 2, [90, 90, 90, 255]);
        let delta = CursorDelta::between(&base, &other);
        assert!(delta.is_empty());
        assert_eq!(delta.bounds, None);
        assert_eq!(delta.bytes, 0);
        // Reading through an empty patch is the base frame, not a panic.
        let with_patch = FrozenOutput {
            cursor_delta: delta,
            ..base
        };
        let _ = &with_patch;
        assert_eq!(frozen_pixel(&with_patch, 4, 4, 1, 1, true), [10, 10, 10, 255]);
    }

    /// The frame handed to the PNG compositor is the one that was displayed.
    ///
    /// The session stores the without-cursor capture plus a patch, so the
    /// with-cursor frame has to be materialised at the end — once, not held.
    #[test]
    fn the_capture_handed_to_the_compositor_matches_what_was_shown() {
        let info = OutputInfo {
            global_name: 7,
            logical: LogicalRect { x: 0, y: 0, width: 4, height: 2 },
            scale: 1,
        };
        let base = frozen_test_output(info, 4, 2, [10, 10, 10, 255]);
        let mut cursor = frozen_test_output(info, 4, 2, [10, 10, 10, 255]);
        // One cursor pixel at (2,1).
        cursor.data[(4 + 2) * 4..(4 + 2) * 4 + 4].copy_from_slice(&[250, 0, 0, 255]);
        let delta = CursorDelta::between(&base, &cursor);
        assert_eq!(delta.bounds, Some((2, 1, 1, 1)));
        let stored = FrozenOutput { cursor_delta: delta, ..base };

        // Without the cursor: the stored frame, byte for byte.
        let without = stored.to_image(false);
        assert_eq!((without.width, without.height, without.stride), (4, 2, 16));
        assert_eq!(without.data, stored.data);
        // With the cursor: the patch applied, and only there.
        let with = stored.to_image(true);
        assert_eq!(
            &with.data[(4 + 2) * 4..(4 + 2) * 4 + 4],
            &[250, 0, 0, 255]
        );
        assert_eq!(&with.data[0..4], &[10, 10, 10, 255]);
        assert_eq!(with.data.len(), stored.data.len());
    }

    #[test]
    fn frozen_pixel_samples_scale_two_outputs() {
        let output_info = OutputInfo { global_name: 1, logical: LogicalRect { x: 0, y: 0, width: 2, height: 2 }, scale: 2 };
        let mut frozen = frozen_test_output(output_info, 4, 4, [0, 0, 0, 255]);
        frozen.data[(2 * frozen.stride + 2 * 4) as usize..(2 * frozen.stride + 3 * 4) as usize].copy_from_slice(&[11, 22, 33, 255]);

        assert_eq!(frozen_pixel(&frozen, 2, 2, 1, 1, false), [11, 22, 33, 255]);
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
        FrozenOutput {
            info,
            width,
            height,
            stride: width * 4,
            data,
            cursor_delta: CursorDelta::EMPTY,
        }
    }
}
