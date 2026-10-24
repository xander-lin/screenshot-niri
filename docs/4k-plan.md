# 4K plan

Working plan for making a 4K long capture fit its frame budget. Six items, done
one at a time, each with the measurement that motivated it and the approach
agreed for it. **Keep this file current**: it is the hand-off between work
sessions, and an item is finished only when its acceptance numbers have been
re-measured on this machine.

- **Decision: the frame-rate cap does not change.** 4K must fit 30 fps by
  making the work smaller, not by capturing less often.
- Machine: niri 26.04, Ryzen, two 1920x1080 outputs. `--release`
  (`lto = "thin"`, `codegen-units = 1`).
- 4K numbers come from `stitch-bench --synthetic 3840x2160x60`, which is the
  only way to measure 4K without a 4K output. The overlay and capture figures
  come from temporary release-mode timing tests (deleted after each item; the
  numbers are recorded here and in `docs/performance.md`).

## Baseline, 4K, per accepted frame (2026-10-24)

The UI thread pays all of this except the capture row:

| stage | ms | how measured |
|---|---|---|
| RGBA→RGB conversion | 5.5–6.0 | perf, `rgb_frame_from_source_reusing` + `row_rgb_into` |
| boundary descriptor | 4.3 | perf, `EdgeLadder::from_source_on` |
| canvas reads (duplicate + colour checks) | 3.9 | perf, `CanvasStore::pixel` 1.28 + `touch_tile` 1.02 + `average_difference` 1.10 + `luminance_range` 0.54 |
| canvas write + packing | ~1.3 | perf, `write_spans` + tile writes |
| page faults / allocation | ~1.2 | perf, kernel `clear_highpages` |
| other (memcpy, loops, bounds) | ~2.5 | remainder |
| **`push` total** | **18.7** | `stitch-bench` |
| preview image (canvas RGB → RGBA) | 5.9 | `stitch-bench` |
| overlay: copy cached dim base (33 MB) | 1.6 | release timing test |
| overlay: draw preview | 3.2–10.0 | release timing test |
| overlay: border | 0.01 | release timing test |
| **UI thread total** | **29.4–36.3** | budget is 33.3 ms at 30 fps |
| capture: fresh memfd + 33 MB first touch | 16.1 | release timing test |
| capture: `bytes.to_vec()` | 1.9 | release timing test |

The `stitch-bench` figure (24.6 ms) is *not* the UI cost: it covers `push` and
`preview` only. The overlay render is the third piece and the capture thread is
a fourth (it does not block the UI directly, but it burns a core and 33 MB of
page zeroing per frame in the same memory system).

Memory at 4K, one output: overlay buffers 2 × 33.2 MB, preview image 33.2 MB,
capture frame 33.2 MB (Vec) + 33.2 MB (shm mmap), channel queue up to
4 × 33.2 MB, canvas packed separately.

## The six items

Order is by (value ÷ risk), not by size. Each item is a separate commit with its
own before/after measurement.

### 1. Rewrite the overlay preview draw — 3.2–10.0 ms → ~0.5–1.5 ms

**Problem.** `draw_preview_on_overlay` iterates every column of every row and
asks three questions per pixel (inside the hole? inside the image? alpha zero?),
then copies 4 bytes at a time. A 4K selection that fills the screen draws
nothing and still costs 3.2 ms; the worst geometry costs 10.0 ms where a plain
`memcpy` of the same 24.6 MB costs 1.4 ms.

**Approach.**
- Skip a row outright when the hole covers the full width (`hole.x <= 0 &&
  hole.x + hole.width >= overlay_w`) and the row is inside the hole.
- Iterate the two *visible segments* of a row (`[0, hole.x)` and
  `[hole.x+hole.width, overlay_w)`) instead of testing every column.
- Fast path when the capture and the buffer share a density
  (`cw == vp_buffer_w && ch == vp_buffer_h`, the normal case): source columns are
  contiguous with buffer columns, so one `copy_from_slice` per segment.
- The alpha test (ragged T/L/arch canvases must leave the dimmed base visible) is
  only needed when the image actually has transparent pixels. Add
  `opaque: bool` to `LongPreviewSnapshot`, computed exactly by
  `image_from_stitched_window_reusing` (it already decides `have_row` and
  `whole_row` per row): return a small `WindowImage { image, opaque }`.
- Keep the per-pixel path for a non-1:1 density, with the column table built
  once (it is the fractional-scale case, rare).

**Files.** `src/wayland/selection.rs` (`draw_preview_on_overlay`,
`LongPreviewSnapshot`), `src/stitch/canvas.rs` (`WindowImage`),
`src/app.rs`, `src/bin/stitch-bench.rs`, `src/stitch.rs` tests.

**Acceptance.** Existing overlay tests pass unchanged
(`preview_draws_grown_stitch_above_selection_and_keeps_hole_clear`,
`preview_fills_a_denser_buffer_without_gaps`,
`preview_skips_transparent_stitched_pixels`,
`a_fractional_buffer_maps_the_preview_one_to_one_with_the_canvas`,
`a_preview_is_not_drawn_on_a_disjoint_output`). New tests: a full-width hole
draws nothing, and a ragged preview still leaves the base visible in the gaps.
Timing at 4K: full-screen hole < 0.2 ms, 24.6 MB history < 2 ms.

### 2. Stop copying the dim base when it did not change — 1.6 ms → 0

**Problem.** Every committed overlay frame does
`data[..buf_size].copy_from_slice(&base_pixels)` — 33 MB at 4K — even when the
buffer already holds exactly that base from its previous commit.

**Approach.** Record the base key on the *buffer*, not only on the overlay:
`OverlayBuffer` gets `base_key: Option<(i32, i32, Option<LogicalRect>)>`. On
commit, if `buffer.base_key == cache_key` the copy is skipped (the buffer still
holds its own copy of the same mask); otherwise copy and record. The overlay's
`base_pixels` cache stays the source of truth for rebuilding.

**Files.** `src/wayland/selection.rs` (`OverlayBuffer`, `render_live_overlays`).

**Acceptance.** Existing dirty-region tests pass; a test that two consecutive
commits with an unchanged box copy nothing (observable through the recorded key)
and a changed box copies again. Timing at 4K: the per-frame base copy
disappears from the profile.

### 3. Reuse the capture buffer instead of a fresh memfd per frame — 16.1 ms → ~2 ms

**Problem.** `CaptureSession::capture_region_frame_without_cursor` sets
`shm_image = None` at the start of every frame, dropping the previous pool,
buffer and mapping; the `Buffer` event then creates a new 33 MB memfd. The
system calls cost 0.01 ms — the 16.1 ms is first-touch page faults on fresh
shmem (kernel zeroing 33 MB), plus a 1.9 ms `bytes.to_vec()` on `Ready`.

**Approach.** Keep the `ShmImage` across frames: if the compositor offers the
same `(format, width, height, stride)` as the live one, reuse the pool/buffer/
mapping (re-requesting a `copy` into the same `wl_buffer`). Only reallocate when
the geometry changes. Keep the `to_vec()` into a *recycled* `Vec<u8>` carried by
the session, so the UI still owns its frame and the capture thread stops
allocating.

Two rules to respect: a `wl_buffer` may not be re-used for a new `copy` while
the compositor still holds it (wlroots sends `wl_buffer.release`; wait for it,
or use a two-buffer pool), and `y_inverted` must still flip into the recycled
buffer rather than allocating a new one.

**Files.** `src/wayland/screencopy.rs` (`State`, `ShmImage`, the `Buffer`/
`Ready`/`release` handlers, `CaptureSession`).

**Acceptance.** `capture-frames` still produces byte-identical frames on
`testdata/frames/real-1` (compare the decoded pixels, not the PNG bytes);
`cargo test` green; a release timing test shows the per-frame capture cost
without the first-touch spike after the first frame.

### 4. Read canvas rows, not pixels, for the two checks — 3.9 ms → ~2.0 ms

**Problem.** The near-duplicate check (`CanvasRegion` → `CanvasStore::pixel`)
and the colour verification sample every 4th pixel through `store.pixel`, which
does a tile lookup, a working-set touch and a bounds check per sample. At 4K
that is 6.83% + 5.25% + 2.60% of the frame.

**Approach.** Add a strided row accessor to `RgbSource`
(`row_rgb_into` already exists) and sample by row: for the every-4th-row grid,
read the row's sampled columns from one `with_row` call. Keep the same sample
set (every 4th pixel of every 4th row) so the decisions do not change. Apply to
`average_difference_same_source` and `luminance_range_source` (and
`pixel_difference_source` if it is the shared path).

**Files.** `src/stitch/rgb.rs` (`CanvasRegion` sampling), `src/stitch/store.rs`
(a `with_row`-based sampled accessor if needed), `src/stitch/engine.rs`.

**Acceptance.** All corpora unchanged (204/97/7/3/7/7/78 accepted, 0 misplaced,
0 uncovered) — this is a pure access-path change and any decision change is a
bug. Perf: `CanvasStore::pixel`/`touch_tile` no longer in the top 10 at 4K.

### 5. Build the RGB frame and the boundary descriptor in one pass — 10.0 ms → ~7.0 ms

**Problem.** The RGBA→RGB conversion reads the 33 MB capture and writes 24.9 MB
(5.5–6.0 ms); `EdgeLadder::from_source_on` then re-reads the same content to
build the 48-bin descriptor (4.3 ms). Two passes over the same pixels.

**Approach.** Fuse them: while converting each row, accumulate the per-row
`|dL/dy|` bin sums the ladder needs (it is a luminance derivative between
adjacent rows, which the converter can keep as the previous row's luminance).
`EdgeLadder::from_source_on` then consumes the precomputed bins instead of
re-reading. The ladder is built on the *compose* frame (possibly cropped), so
the fusion must live where the RGB frame is built from the same crop — likely
`rgb_frame_from_source_reusing` returning `(RgbFrame, Option<RowBins>)`, or a
new `frame_and_bins_from_source` used by `push_frame_sources`.
The horizontal axis builds its ladder from a *canvas region* (previous frame's
rect), which is not the frame being converted — keep the existing path there.

**Files.** `src/stitch/rgb.rs`, `src/stitch/engine.rs`
(`EdgeLadder::from_bins`), `src/stitch.rs` (`push_frame_sources`).

**Acceptance.** Corpora unchanged. Perf: `EdgeLadder::from_source_on` drops out
of the top 3 at 4K; `push` falls by ~2.5–3 ms. A unit test pins that the fused
bins equal the bins built by the old path for the same frame.

### 6. Draw the preview from the canvas instead of through an RGBA image — 5.9 ms → ~2.0 ms

**Problem.** Every accepted frame builds a 33 MB RGBA image of the visible
canvas (`image_from_stitched_window_reusing`, 5.9 ms), and the overlay then
reads it back to paint (item 1's copy). The intermediate image exists only to
carry pixels from the canvas to the overlay: 24.9 MB read + 33 MB written, then
33 MB read again.

**Approach.** Give the overlay a pixel source it can sample directly
(`trait PreviewPixels { fn width/height/stride; fn row_into(&self, y, x, dst) }`
or an equivalent), implemented by the canvas window (RGB, converted to BGRA in
the draw loop) and by the test mock. `LongPreviewSnapshot` holds that source
instead of an `Image`. The draw then converts and writes in one pass: canvas →
overlay, no intermediate buffer. The overlay still owns the density mapping,
the hole, the border and the opacity fast path from item 1.

This is the one structural change: it crosses the `stitch` → `wayland::selection`
boundary, so the interface goes in a neutral place (e.g. `src/stitch/canvas.rs`
exports a `CanvasWindow`-backed `PreviewPixels`), and the overlay only ever sees
the trait. Mock implementation required by the project's architecture rules.

**Files.** new trait + canvas impl, `src/wayland/selection.rs`
(`draw_preview_on_overlay`, `LongPreviewSnapshot`), `src/app.rs`,
`src/bin/stitch-bench.rs` (preview measurement moves to the draw).

**Acceptance.** Overlay tests unchanged in behaviour (they can use a mock or an
in-memory implementation). Preview correctness on the corpora (the stitched PNG
path is untouched). Perf at 4K: the 5.9 ms preview stage disappears; the draw
grows by ~2 ms; net ~4 ms.

## Verification for every item

- `cargo test` (204 + 225), plus `stitch-legacy`, `trace-logs`,
  `canvas-raw-tiles`.
- Seven corpora via `replay-frames`: accepted 204/97/7/3/7/7/78, 0 misplaced,
  0 uncovered.
- `stitch-bench --synthetic 3840x2160x60` three runs, median; record push,
  preview, allocations, UI thread.
- 1080p regression check (`--synthetic 1920x1080x60`): push must stay ≈5 ms.
- clippy: no new warnings (42 at baseline), 0 errors.
- No temporary probe left in the tree (`git status` clean apart from the item's
  own files).

## Progress log

| item | state | result |
|---|---|---|
| 1 overlay preview draw | **done** | full-screen hole 3.22 → **0.01 ms**; 24.6 MB history 10.04 → **2.1 ms**; 80% box 5.06 → **0.28 ms**; side column ~1.95 ms. UI thread at 4K: 29.4–36.2 → **26.2–28.3 ms**. 1080p unchanged (push 4.93, preview 1.36). Corpora unchanged. |
| 2 overlay base copy | not started | |
| 3 capture buffer reuse | not started | |
| 4 canvas row sampling | not started | |
| 5 fused conversion + descriptor | not started | |
| 6 preview direct from canvas | not started | |
