# Mechanism: from selection to pixels on disk

One screenshot, end to end. Normal mode and long mode share everything up to the
`L` handover; after that they diverge, and the divergence is the interesting
part. This document follows the data, not the file layout: each stage says what
it owns, what must stay true across the handover, and why the design is shaped
the way it is.

Companion documents: `AGENTS.md` (contracts and invariants per module),
`found.md` (verified incidents), `docs/edge-stitch-design.md` (the matcher's
design and measurements).

---

## 0. The shape of the program

Two processes, three threads, four coordinate spaces.

```
main thread ──┬─ cli::parse ── config::load
              │
              ├─ [normal] screencopy::capture_outputs ×2  (with / without cursor)
              │           selection::SelectionSession     (layer-shell overlays)
              │           image::composite_captured_region
              │           image::write_png  ── clipboard::serve_path_detached
              │
              └─ [long]   selection::SelectionSession
                          + LongCaptureWorker thread ── screencopy::CaptureSession
                            (own Wayland connection, own event queue)
                          + stitcher (main thread)
                          + image::write_png  ── clipboard::serve_path_detached

detached `screenshot --clipboard-provider`   (third process, outlives the run)
```

The three **coordinate spaces** are the single most common source of bugs here,
so they are named up front and used consistently below:

| Space | Units | Who produces it | Who consumes it |
|---|---|---|---|
| **Global logical** | logical px, origin at the top-left of the whole desktop | pointer events (`pointer_global_position`), selection rectangles | `geometry`, the overlay's hole/border |
| **Output-local logical** | logical px, origin at that output's top-left | `SelectedViewport::from_outputs` | `zwlr_screencopy` requests (they take output-local logical units) |
| **Buffer pixel** | device px, origin at the capture's top-left | the compositor's `zwlr_screencopy_frame_v1.buffer` | the stitcher, the overlay's buffer writes, the PNG |

The mapping between logical and buffer pixels is **always taken from the capture
itself** (`image.width / output.logical.width`), never from `wl_output.scale`.
That integer is a hint: an output at 1.5 scale reports `scale = 2` and delivers
1920 px for 1280 logical. Trusting it asks the crop for pixels that do not exist
(a whole-output capture on a scaled screen once produced no image at all).

---

## 1. Freeze: capturing the screen before showing anything

`app::run_normal` captures every output **twice**:

```rust
capture_outputs_with_cursor()?      // overlay_cursor = 1
capture_outputs_without_cursor()?   // overlay_cursor = 0
```

Each capture opens its own connection (`screencopy::capture_output`), binds the
registry, requests `zwlr_screencopy_frame_v1.capture_output` on exactly one
`wl_output`, and blocks until `Ready`. Two captures are needed because the user
may press `H` at any moment to choose whether the cursor appears in the output —
and the choice must not require re-capturing a screen that has since changed.

The two frames differ only where the compositor drew the cursor. Measured on
1920x1080: **1 440–2 706 pixels of 2 073 600 (0.07–0.13%)**, and exactly zero on
an output the pointer is not on. So the session stores **one** frame plus a
sparse patch:

```
FrozenOutput { data: Vec<u8>,          // the without-cursor capture, whole
               cursor_delta: CursorDelta }  // (row, [(x, [u8;4])]) sorted by x
```

*Invariants.* `CursorDelta::between` refuses when the two frames disagree on
geometry and returns `EMPTY` — a missing cursor is a much smaller failure than a
wrong picture. It records **every** differing pixel rather than assuming "only
the cursor", so a caret that blinked between the two captures survives into the
output.

After the session is built, `app` drops both full-frame sets immediately. They
were 33 MB of dead weight during the drag.

---

## 2. Overlay: one layer-shell surface per output

`selection::SelectionSession::new_internal` binds `wl_compositor`,
`zwlr_layer_shell_v1`, `wl_shm`, `zxdg_output_manager_v1` (position),
`wp_viewporter` (fractional-scale round trip), `wp_fractional_scale_manager_v1`
(preferred scale), `wl_seat`.

For **every advertised output** it creates a `wl_surface` + layer surface
anchored to all four edges with `exclusive_zone = -1`, keyboard-exclusive.

Two facts drive most of the code here:

1. **A layer surface needs a buffer before it receives input.** Until the
   surface has been committed with a buffer the compositor will not map it, so
   no `wl_pointer.enter` arrives. There is no way to "wait for a selection
   rectangle and then allocate": the overlay must own a full-surface buffer from
   the start. (Measured: clearing the buffers mid-session stops pointer input.)
2. **A buffer must not be attached before the first `configure`.** The logical
   size comes from `ZwlrLayerSurfaceV1::Configure`; `wp_fractional_scale_v1`
   arrives on the surface's own first commit and can be dispatched *earlier*.
   So `refresh_overlay_surface` records `preferred_scale` and returns early
   until `configured` is set.

`OVERLAY_BUFFER_COUNT = 2` — one on screen, one committed and awaiting display.
The overlay commits **only when its content changes** (`live_state_seq` +
`last_selected` on each buffer), so the commit loop is not a frame stream and
does not need the third buffer that a continuous renderer would. Measured on
niri: a complete `--long` session, live preview included, runs on two.

Buffer sizing (`overlay_buffer_size`) is the max of three lower bounds: the
frozen capture's pixel size (we must not resample our own screenshot), the
output's current mode (the grid the compositor scans out), and
`preferred_scale × logical` in 120ths. Without `wp_viewporter` a buffer larger
than the surface cannot be mapped back, so the logical size wins.

### The two render paths

An overlay is on exactly one of them at a time:

* **Frozen path** (`render_overlay`): draws the stored capture, dimmed outside
  the hole. It recomposites only the bands whose appearance can have changed —
  see §2.1. `overlay_pixel()` decides per pixel between
  `OUTSIDE_MASK` (dim), `TRANSPARENT` (the hole) and the border ring; both pixel
  layers are **derived** from the one stored frame on every draw — the cache
  holds geometry only. Damage is the union of the previous and current border
  ring, so a shrinking box erases its old border; the live path reports the
  damage it actually rewrote (`foreground_damage`) rather than always claiming
  the whole buffer.
* **Live path** (`render_live_overlays`): fills the mask, punches the hole from
  the current (unioned) rectangle, composites the stitched preview, draws the
  border. Used from `L` onward.



### 2.1 What a drag actually costs

A frozen overlay redraw is bounded by three separate things, and getting any one
wrong makes a drag burn a core (`found.md`, 2026-10-24):

1. **Which pixels.** The changed set is *not* the bounding box of the old and new
   border rings: a 1-pixel step of an 800x500 box changes ~2 000 pixels while that
   bounding box is 408 000. `changed_selection_rects` decomposes it into thin
   bands (each ring as four strips, `rect_difference` against the other box), so
   the work is proportional to the border's perimeter plus the distance moved.
2. **Cost per pixel.** `frozen_pixel` does two 64-bit divisions, two `clamp`s and
   a bounds-checked slice, plus a binary search when the cursor patch applies.
   Every one of those is separable per row or column, and the mapping is the
   *identity* whenever the buffer matches the capture's grid — the normal case —
   so the identity path skips the division entirely. `overlay_pixel`'s
   `border_bounds` is likewise precomputed once per frame
   (`SelectionClassifier`).
3. **How many frames.** Pointer motion arrives far faster than the display
   refreshes and `blocking_dispatch` returns once per event. Requests are
   coalesced per event-loop turn and rate-limited to one frame per refresh
   (`request_render` / `flush_render_request`), and the loop wakes at the frame
   interval while a request is pending so the last position of a drag is never
   dropped.

Measured on a fast back-and-forth drag: 100% of a core → 20-31%.

`render_scope: Option<u32>` narrows the live path to one output once long
capture starts, and `queue_render`/`flush_pending_render` count the pending-render
debt against that same scope — the other outputs' overlays are static frozen
images whose canvases turn over independently, and counting them would make the
debt clearable only when every output happens to turn over in one pass.

---

## 3. Selection: drag, edit, and the two holes

`DragState` is the whole interaction state machine: `Idle → Dragging →
Finished`, plus `Cancelled`. Pointer position is received **surface-local**
(`wl_pointer::Motion { surface_x, surface_y }`), so it is turned into global
logical by adding the output origin — which requires knowing which output the
pointer is on, hence `pointer_output_name` set on `wl_pointer::Enter`.

Two different rectangles exist while a capture is in flight, and keeping them
distinct is what makes shrinking behave:

* **`ui_render_selection`** = current rect **∪** `capture_pending`. The hole
  must still cover whatever a pending frame was prepared from, otherwise the
  overlay would paint over content the compositor is copying.
* **`ui_render_border_rect`** = current rect only. The visible box follows the
  pointer; the hole may lag.

Correctness of captured frames is *not* trusted to the hole. `capture_pending`
is set in `prepare_capture_clean`, and `capture_invalid` is **sticky** until
`complete_capture`: a rectangle that moves away *and back* still rejects the
frame. Edits also `drop_invalid_pending`, which is what lets the dim edge follow
the box when the user shrinks — keeping the pending region would hold the hole at
the widest point of the drag.

**Edit mode (`E`)** turns the finished rectangle into a draggable box:
`hit_test_edge` with an 8 px snap range picks one of four edges, the interior
moves the whole box (`apply_box_move`, pointer offset preserved). In long mode
edit mode also **grabs pointer input** (`set_input_region(None)`), which stops
wheel events at the overlay so the content cannot scroll while the box is
adjusted.

**Spanning selections.** Normal mode supports them: the rectangle is split per
output by `SelectedViewport::from_outputs`, each piece is cropped from its own
frozen capture, and `composite_captured_region` stitches them at accumulated
`dst_x`/`dst_y` offsets (edges, not widths, so adjacent pieces share one
boundary even at fractional scale). Long mode cannot: it captures one output by
construction, so a spanning box turns **white-red** and is *discarded* on release
rather than clamped — clamping would silently capture something the user did not
select. `L` promotion is the exception: there the box belongs to the output the
drag started on, so it is clamped to that.

Press `1`–`9` instead of dragging to take a whole screen (ordered left to right,
top to bottom — sorted by `(x, y)`, not by index).

---

## 4. The `L` handover

`set_selected_viewport_passthrough` is the transition. It is worth listing
exactly what changes, because each line exists for a reason:

```rust
render_scope = Some(selected output)   // long capture is single-screen
frozen_outputs.clear()                 // a frozen frame must not survive into
                                       // live capture — the user asked for the
                                       // real screen
overlays[*].render_cache = None        // leave the frozen path
for other outputs: buffers.clear()     // see below
render_live_overlays(Some(selected))   // retry until one buffer commits
set_overlay_keyboard_exclusive()
set_overlay_pointer_passthrough()      // empty input region: wheel goes through
```

The `buffers.clear()` on unselected outputs is 8.1 MB per screen. Those surfaces
are never committed to again — capture is scoped, the pointer is grabbed — so
their buffers are pure cost. Measured: 4 buffers / 32.4 MB `RssShmem` during
selection → 2 buffers / 16.2 MB while capturing. **Known caveat:** the buffers
are released but the *surface* keeps whatever the compositor last took from it,
which is the intended "the other screen shows the dim frame as of the moment `L`
was pressed". A later `configure` or `preferred_scale` on that output
must not re-create the buffers — an empty `buffers` vec reads as "needs
buffers" to the resize test, so `refresh_overlay_surface` consults
`output_is_render_scoped` and returns early for an out-of-scope output.

---

## 5. Long capture: the handshake

This is the part with a genuine concurrency contract, and it exists because of a
specific observed bug: **white borders inside stitched long captures**.

The cause was a race between two things that must agree:

* the overlay's hole (what is *visible*, therefore what the compositor's
  screencopy will and will not include), and
* the rectangle the worker asks `zwlr_screencopy` for.

They were computed at different moments on different threads. An edit drag
between the two produced frames captured through a box that no longer matched
the hole, and the border pixels landed *inside* the capture.

`LongCaptureWorker` is a dedicated thread with **its own Wayland connection**
(`screencopy::CaptureSession`), because screencopy needs a roundtrip per frame
and cannot share the UI's event queue without stalling input. Each frame is a
four-step handshake:

```
worker                                   UI thread
  │  PrepareCapture { index, ack }  ──────▶
  │                                        prepare_capture_clean():
  │                                          · bail if edit_drag active
  │                                          · bail if status != Running
  │                                          · capture_pending = rect
  │                                          · render_live_overlays(Some(out))
  │                                          · roundtrip()  ← orders requests
  │                                          · return rect
  │  ◀────── ack: Capture(region) | Skip
  │  capture_region_frame_without_cursor(region, wait_for_damage=true)
  │  Frame { image, region }  ────────────▶
  │                                        complete_capture():
  │                                          · invalidate_changed_capture()
  │                                          · return !capture_invalid
```

Why the roundtrip inside `prepare_capture_clean`: the region is derived by the
UI thread **from the same rectangle it just rendered**, and a `flush()` alone
does not order requests across two connections. The roundtrip is what guarantees
the compositor has the new overlay state before the worker's `capture_region`
arrives on the other connection. The worker must not capture with a region the
overlay no longer shows, and the UI must not report a rectangle it did not
render.

`PrepareAck::Skip` covers both refusal reasons (editing, clean render failed);
the worker sleeps 8 ms and starts a new attempt rather than capturing stale
geometry.

`wait_for_damage = true` uses `copy_with_damage`, so a static screen produces no
frame at all rather than the same frame at `--fps`. The capture loop polls with a
bounded 20 ms wait so `stop` is observed promptly: a static screen must not make
the worker unjoinable.

Frame rate is `--fps` (default 30, range 1–240) or the `max_fps` setting.
Refusals do **not** stop capture — recovery depends on noticing that content
overlaps the canvas again — but they drop it to `LONG_WARNING_FRAME_RATE = 5`,
because each refused frame costs a full anchor scan and the user's recovery is a
seconds-long action.

---

## 6. Stitching

`RawStitcher::push_frame_with_offset` → `PlacementEngine` (the shipped
`WindowedEdgeMatcher`). The design is documented in
`docs/edge-stitch-design.md`; the parts that matter for the pipeline are:

* **Four searches, then refusal**: impulse (where the last step points), guard
  (does this frame still overlap the previous one — the whole `pos ± rows` band
  is searched and the prediction only breaks ties, so a runaway prediction
  cannot empty it), anchor (does it appear anywhere the canvas can see — tried
  whenever the guard has nothing, because a frame that shares no rows with the
  previous one has no local answer at all; a placement outside the band must
  compare as many boundary rows as any plausible candidate in its window (a
  relative demand — one-row pages are held to their one row), be colour
  verified, and leave the runner-up refuted by the pixels, else the frame is
  refused rather than written at a repeated patch), and thin continuation (the
  overlap carries fewer boundary rows than the `min(3, rows)` floor — a nearly
  blank page; the placement is searched in the guard band and needs a perfect
  structural cost, a strictly worse runner-up, pixels agreeing to noise over
  the whole overlap, and at least half the frame overlapping the previous one).
  `REFUSAL_PATIENCE = 5` governs the border and the frame-rate drop, not the
  search.
* **Every acceptance gets a colour verification** against the pixel canvas,
  because the boundary descriptor is colour-blind by design.
* **Axis routing**: the axis is the capture's impulse, not the arrow key; the
  key only seeds the first frame. A horizontal fallback runs only when the user
  indicated a horizontal direction.

### The decision tree, and what is actually a branch

`place` is four searches followed by refusal; each search feeds the *same* gate.
Written as the code's nesting:

```
ladder = prebuilt means ? from_means : from_source      (uniform → NoBoundaries)
prediction = pos + last_delta + trend

last_delta?  → impulse window  pred ± max(24, 1.05|Δ|) ─┐
              guard band       pos ± rows               ├→ gate → far-support → accept
              anchor rings     from anchor, ×4, minus guard band ┘
              thin             guard band, floor 1, perfect + unique + pixels
              else             refusal
```

The gate (`decide_from_costs`) has six branches, in cost order: sort; winner
cost ≤ 0.08, or the `second/best ≥ 2.5` margin escape; ambiguity `second/best <
1.5` → colour check → nearest to the window centre; colour verification; the
boundary-direction check when the winner compared ≤ 8 rows; and, outside the
guard band only, relative completeness (`used ≥ window_used`) plus pixel
refutation of the runner-up.

Every search answers a *different* question, which is why the tree is not
collapsed further:

* **impulse** is the cheap common case, and its narrow candidate set is itself
  evidence: the runner-up ratio it computes is what decides most frames, and a
  wider window changes that ratio (measurements in `docs/edge-stitch-design.md`
  §12: 1.05× is the widest impulse that does not start catching jump frames).
* **guard** is continuity: only positions that still overlap the previous frame
  can be "the next step". Merging it into the anchor would let a far match be
  accepted as if it were local.
* **anchor** is recovery, nearest-first. A single full-canvas scan finds the
  same winner but scores every position on every refusal (≈3-5× the work on the
  corpora's 4 600-row canvases); the ring ladder is what makes this path
  affordable. `ScoreCache` exists for the same reason — the impulse and guard
  windows overlap, and re-scoring a position costs ~9 k bin comparisons.
* **thin** is the only stage that may accept fewer than the evidence floor, and
  it charges for that with exactness, uniqueness and whole-overlap pixels.

Three simplifications were **measured and rejected** while this was written:
running thin before the guard (its uniqueness test only looks outside the
basin, so a terminal's repeated line inside the basin becomes a "unique"
answer — `real-4-4k` went from 2 to 196 far placements), replacing the
direction check with a colour ceiling scaled to the frame's own luminance range
(fractional-scale resampling on `real-4-4k` produces colour differences of the
same order as a band's tone range, so the ceiling rejects correct placements),
and dropping the margin escape for thin evidence (real motion blur legitimately
needs it — 116-190 compared rows on `real-long`). What *was* simplified is the
code, not the decision: all three scored searches and the thin stage now share
one scoring path (`window_costs` → `score_tops`), `decide` is no longer a
duplicate of `decide_ring`, and the scoring path is the one that parallelises
(see `docs/performance.md`). The remaining complexity is calibration — the
numbers in the gate — not structure.

The canvas is `StitchedFrame`, and its storage is the memory-critical part:

```
CanvasStore
  origin_row: i32            ← the tile grid's anchor, in canvas rows
  tiles:      RefCell<Vec<Tile::Empty | Resident | Packed>>
  written:    RefCell<Vec<bool>>   ← one flag per grid row
  compressor: TileCompressor (lz4 | raw under canvas-raw-tiles)
  budget:     max(4 × frame_bytes, 4 MB)
```

Three rules that must not be collapsed:

1. **A tile is allocated whole**, so a band can hold rows no frame ever wrote.
   `written` is what distinguishes *never captured* from *a row of zeros*; without
   it the colour check compares a frame against black and the acceptance ceiling
   becomes meaningless.
2. **Growing upward relabels, it does not re-cut.** `relabel(delta)` moves
   `origin_row` by the shift; band headers do not move, and no pixel is copied.
   `written` is indexed relative to `origin_row`, so it moves with it for free.
   Re-cutting to line up with the new row 0 would copy the whole canvas on every
   prepend — which is what the contiguous buffer did, and why it peaked at twice
   the canvas.
3. **Only a width change re-cuts** (`reflow_store`). That is a horizontal step or
   a selection-box width edit — rare, not per-frame.

Cold tiles are packed by an LRU sweep every `EVICT_INTERVAL = 32` touches, taking
the working set back to `budget × 3/4`. The budget is sized against what a frame
actually *reads* (the previous frame's rectangle, ≈0.5 MB on `real-1`) rather
than against the canvas, which is what bounds a capture of unbounded length.

Measured on `real-1`: resident 40.3 → 6.9 MB, peak 62.1 → 11.1 MB, accepted-frame
time 3.01 → 1.25 ms. Compression is 5.5% (`real-1`) / 9.1% (`real-long`) of raw.

---

## 7. Preview: what the user sees while capturing

The overlay can display at most one screenful of canvas — its buffer is one
output in size, and it samples the canvas through the capture's own density. So
`app` hands it a **window**, not the canvas (`preview_window`, one viewport of
reach in each direction). Handing over the whole canvas cost a full RGBA copy per
accepted frame: 22 MB on `real-1`, 45 MB on `real-long`.

`draw_preview_on_overlay` maps **buffer pixel → canvas pixel in one step**, via
the capture's density ratio. Mapping through global logical coordinates first
quantises the phase to logical pixels and re-expands it — a 1.5× upscale of a 1×
sample, i.e. a visibly soft preview on any fractionally scaled output. The loop is
**buffer-major**: iterating the stitched image instead leaves gaps as soon as the
buffer is denser than the logical grid, and the live screen shows through them.

Rows whose canvas alpha is 0 (never captured — the corners of a T/L/arch) are
skipped, leaving the dim mask visible beneath.

---

## 8. Finish: assembling and writing

```
Enter/Space ─▶ long_finish_requested ─▶ loop breaks
             ─▶ worker.stop()  (stop flag + join)
             ─▶ selection_session.close()
             ─▶ stitcher.finish()      ─▶ StitchedFrame
             ─▶ image_from_stitched_frame
             ─▶ image::write_png
             ─▶ clipboard::serve_path_detached
```

`image_from_stitched_frame` walks the canvas **one row at a time** through
`row_into`, not per pixel: a per-pixel accessor would put a working-set lookup
and a borrow check on every one of them (measured: 3× the cost of the whole rest
of the frame). Rows the canvas never captured stay RGB-zero and **alpha 0**, so
ragged corners are see-through in the PNG rather than black.

`write_png` converts to RGBA (`Xrgb8888` → alpha 255, `Argb8888` passthrough),
because PNG has no XRGB.

`serve_path_detached` spawns `screenshot --clipboard-provider` as a **third
process**. Wayland requires the data source to stay alive for pastes to succeed,
so the provider deliberately outlives the capture; a lingering `screenshot` in
`ps` after a screenshot is expected, not a leak.

---

## 9. Normal mode: the same ending, a different canvas

Normal mode never enters sections 5–7. After the selection ends:

```
viewport.capture_size(&captures)        ← pixel size of the composite
viewport.capture_regions(&captures)     ← one crop per covered output
image::composite_captured_region        ← blit each crop at its dst_x/dst_y
```

The captures come from `session.frozen_captures_for_output()`, which materialises
the stored without-cursor frame plus the `cursor_delta` patch when the user asked
for the cursor. That keeps **one source of truth** for what was on screen: the
pixels written are the pixels the overlay displayed, including the `H` decision.

---

## 10. Where the invariants live

| Invariant | Enforced by | Broken by |
|---|---|---|
| A captured frame matches the hole it was captured through | `prepare_capture_clean` + `complete_capture` (sticky) | computing the region anywhere else |
| Logical↔pixel mapping comes from the capture | `capture_pixel_scale` | using `wl_output.scale` |
| Never-captured rows read as missing, not zeros | `CanvasStore::written` | dropping the flag, or allocating tiles lazily by row |
| Growing upward copies no pixels | `relabel` + `origin_row`-relative indexing | re-cutting bands per prepend |
| The engine's page rows survive an edit and a re-basing | `last_engine_top + delta_y`, `EdgeCanvas::write`'s empty-canvas rebase | deriving the top from the pixel rect, or skipping every row of a negative first commit |
| A placement is verified by its content, not by a recorded row | `replay-frames`' `locate_in_canvas` (whole-canvas search) | `last_engine_top() - canvas_origin()` across recoveries/edits |
| The overlay commits only on change | `live_state_seq`, `last_selected` | committing unconditionally |
| Spanning long selections are refused, not clamped | `discard_spanning_long_selection` | clamping in the selection path |
| `raw_rgb` reads only captured pixels | `RowCoverage` + colour check | reading the bounding box |

---

## 11. Known gaps

* **Mirrored / clone outputs**: every rectangle intersects every output, so
  `rect_spans_outputs` is always true and long capture refuses every selection.
  Normal mode composites correctly. Recorded in `AGENTS.md`, not fixed.
* **`wl_output` `global_remove` is not handled**: an unplugged output keeps its
  entry in `state.outputs` for the session's lifetime. Not a scenario this tool
  is used in (the capture is seconds long), so it is a known gap rather than a bug.
* **Output position without `zxdg_output_manager_v1`** falls back to
  `wl_output.geometry`; on a compositor that scales outputs, `geometry` reports
  pixels where the layout is in logical units, so the fallback can be offset.
