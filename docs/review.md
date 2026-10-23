# Code review: current architecture

Not a style review. Each item is a place where the current code can produce a
wrong result, lose a resource, or silently do the wrong amount of work — ordered
by how much it would cost if it fired. Every claim below was checked against the
code, and the ones that were fixed say so.

Companion: `docs/mechanism.md` (the pipeline this reviews), `AGENTS.md`
(invariants), `found.md` (incidents that led to them).

---

## Correctness

### 1. `refresh_overlay_surface` can re-create buffers the `L` handover released — **fixed**

The handover releases the unselected outputs' buffers to save 8.1 MB per screen
(`set_selected_viewport_passthrough`). But `refresh_overlay_surface` computes

```rust
let resized = (buffer_width, buffer_height) != (overlay.width, overlay.height)
    || overlay.buffers.len() != OVERLAY_BUFFER_COUNT;
```

so an empty `buffers` is indistinguishable from "needs buffers", and it
re-creates them. Two events reach it during a long session:

* `ZwlrLayerSurfaceV1::Configure` — a compositor may re-configure on output
  changes, surface adoption, or a resolution change;
* `wp_fractional_scale_v1::PreferredScale` — e.g. after a monitor hotplug, or a
  scale change on that output.

On niri with a stable desk neither fires between `L` and Enter, which is why the
released 8.1 MB has held in testing. If one does fire, the memory comes back
silently and the "released" claim becomes false. Long capture is scoped to one
output, so the fix is to skip allocation for out-of-scope outputs:

```rust
if state.render_scope.is_some_and(|scope| scope != output_name) {
    return; // never drawn on again; do not re-allocate
}
```

The surface keeps whatever the compositor last took from it, which is exactly
the intended "other screen stays as it was when `L` was pressed".

### 2. The live overlay over-reports damage — efficiency, not correctness — **fixed**

`render_live_overlays` restores the mask/hole from a cached base and then
repaints only `draw_live_foreground` (preview + border), but it reports

```rust
overlay.surface.damage_buffer(0, 0, overlay.width, overlay.height);
```

The whole buffer. **This was not a bug**: in Wayland, damage means "this region
needs repainting", so over-reporting is always safe and only costs the compositor
a repaint it did not need. An earlier draft of this review called it a
correctness problem; that was wrong, and the correction is recorded here rather
than quietly dropped.

It is still worth narrowing, because the overlay commits on every pointer move
during a drag and up to `--fps` during a live capture, and each commit was asking
for a full 1920x1080 repaint of the overlay. The accurate region is:

* **whole buffer** when the base was rebuilt — `rebuild` means the selection
  changed, and `draw_live_base` writes every pixel;
* **whole buffer** when a stitched preview is up — it paints the dimmed area
  *outside* the hole by design, so nothing smaller bounds it;
* **the border ring** otherwise, since with an unchanged `base_key` the mask and
  hole are byte-identical to what the compositor already has.

`foreground_damage` computes exactly that, and falls back to the whole buffer on
any unknown, which is the safe direction. Note the condition that makes this
sound: `base_key` includes the selection, so "not rebuilt" implies the mask and
hole did not change.

### 3. `with_row`'s re-entrancy rule is a comment with no test — **fixed**

`CanvasStore::with_row` documents: *"`f` must not call back into the same store
— every accessor below is written to respect that."* The rule matters because
`touch_tile` unpacking mutates the tile through a `RefCell` while the caller
holds a borrow of that same tile. A violation panics (`already borrowed`) rather
than misbehaving, so it is not silent — but nothing pins the rule, and the
accessors that do respect it carry no test that would notice a future one that
does not. Fixed: `a_nested_store_read_is_rejected_rather_than_silently_wrong` calls a
nested accessor and asserts it fails loudly, so the contract is now executable.

### 4. `OverlayBuffer` holds a raw mmap pointer with only `Drop` as the safety net

```rust
struct OverlayBuffer { data: *mut u8, size: usize, pool: WlShmPool, buffer: WlBuffer, ... }
unsafe impl? // no — it is never sent across threads, so it is fine today
```

Two independent `unsafe { slice::from_raw_parts_mut(buffer.data, buffer.size) }`
sites (`render_live_overlays`, `render_overlays`) create a `&mut [u8]` over the
mapping. Sound as long as no two are alive at once — which holds because both
sites run inside a single `&mut UiState` borrow and never re-enter. But the
invariant is invisible to the compiler: a future change that renders two
overlays from one call (e.g. "redraw output A into output B's buffer") would
alias. Worth a comment on the field, or a debug assertion that the pointer is
not already lent out.

### 5. `final_drift`-style dead reads: cleaned, but the pattern recurs

`replay-frames` had a `final_drift` that was assigned on every accepted frame and
read only for the last one — genuinely needed, but shaped so the compiler warned.
Fixed by making the assignment local to the read. The general lesson: the
project's warnings are now at zero, so **the next warning is a signal again**,
which is the actual value of the cleanup, not the warning count itself.

---

## Resource handling

### 6. `size as i32` when creating an shm pool — **fixed**

```rust
let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
```

`size` is `usize`. The cast is unchecked at both overlay creation and capture. It
overflows above 2 GiB, i.e. ~537 megapixels — no current output approaches that,
so this is not a live bug. It is, however, the kind of cast that would become one
silently on a large canvas or a future tiled capture. Both sites now use
`i32::try_from` and refuse above 2 GiB with a clear error.

### 7. Three `pub fn` on `SelectionSession` exist only for tests — **fixed**

`dispatch_blocking`, `dispatch_pending`, `wait_configured` are `#[allow(dead_code)]`
public API with no production caller (`dispatch_poll` is what the capture loop
uses). They are cheap, but they widen the type's surface for no reason and make
"what does the session actually expose" harder to answer. Deleted: no caller existed, and `dispatch_poll` is what the capture loop
actually uses.

---

## Clarity / maintainability

### 8. A doc comment attached to the wrong function — **fixed**

```rust
/// Convert logical screen offsets into the returned frame's pixel space.
/// Fast raw-frame similarity check. …
fn raw_frame_nearly_identical(…)
```

The first line is a leftover from `frame_screen_offset`, which moved to
`geometry`. Two functions' documentation ended up stacked on one. Deleted.

### 9. Two identical `ImageRgbView` constructions per frame — **fixed**

```rust
let compose  = match ImageRgbView::new(current_frame) { … };
let analysis = match ImageRgbView::new(current_frame) { … };
```

`ImageRgbView` is a borrowed view (`{ image: &'a Image, crop }`), so this is not a
copy — but it reads as one, and it validates the same image twice. One binding,
borrowed twice.

### 10. A duplicated doc paragraph in `evict_if_needed` — **fixed**

The "Packs the least-recently-used…" paragraph appeared twice, in slightly
different wording. Symptom of an edit that added the new text without removing
the old.

### 11. `EVICT_INTERVAL` is a `const` inside a `fn` — **fixed**

Fine, but it is the *only* tuning constant in the tile store that is not next to
the others at module scope, and the module's documentation refers to it by name.
Moved to module scope with the rest, so the knobs are in one place.

---

## Deliberately not changed

* **`prepare_capture_clean`'s 6-attempt / 4 ms retry loop.** It looks arbitrary;
  it is bounded work against a compositor that is holding both overlay buffers.
  The bound is what keeps the UI thread from stalling, and 6 × 4 ms is well under
  one frame at 30 fps. Left as measured.
* **Two Wayland connections (UI + capture worker).** Screencopy needs a
  roundtrip per frame; sharing the UI queue would stall pointer input for the
  duration. The cost is that request ordering must be enforced explicitly
  (`roundtrip` inside `prepare_capture_clean`), which is documented at the call.
* **`OVERLAY_BUFFER_COUNT = 2`.** Measured to complete a full `--long` session on
  niri. The constant is documented as the thing to raise if a compositor is ever
  found that holds a buffer for a whole frame.
* **Tile store `!Clone`.** Deliberate: a canvas that could be cloned would be
  cloned somewhere as a convenience, and the whole point is that it is the only
  copy.
