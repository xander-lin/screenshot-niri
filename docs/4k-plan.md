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

## Result after the six items

| stage | before | after |
|---|---|---|
| `push` (4K) | 18.7 | **13.55** |
| preview image (4K, one screenful) | 5.9 | **3.24** |
| overlay: base copy | 1.6 | **0** (the base is uniform, item 2) |
| overlay: preview draw | 3.2–10.0 | **0.01–2.1** |
| overlay: border | 0.01 | 0.01 |
| **UI thread (4K)** | **29.4–36.3** | **16.8–18.9** (50–57% of 33.3 ms) |
| capture thread (1080p, real screencopy) | 6.82–6.91 | **1.87–1.93** |

1080p: `push` 4.94 → **3.66 ms**, preview 1.36 → **0.58 ms**. All seven corpora
unchanged at every step (204/97/7/3/7/7/78 accepted, 0 misplaced, 0 uncovered).

The real session converts ~1.7 screenfuls of preview instead of three (see item
6), so its per-frame preview is larger than the bench's one-screenful figure but
still well under what it was.

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

### 2. Stop copying the dim base when it did not change — 2.1 ms → 0

**Problem.** Every committed overlay frame did
`data[..buf_size].copy_from_slice(&base_pixels)` — 33 MB read plus 33 MB
written at 4K, measured 2.1–2.3 ms — to put back pixels the frame was about to
overwrite. The cache itself was another 33 MB per overlay.

**Approach (as built).** The base is one uniform colour per pixel
(`draw_live_base`: `[0,0,0,110]` outside the hole, transparent inside), so the
cache is unnecessary. Each buffer records `base_key` (the dim/hole layout under
its pixels) and `painted` (the rects the last commit drew over that base: the
preview footprint and the border ring). On a commit:

- `base_key` mismatch → redraw the mask in place (`erase_rect`, a row fill) and
  clear `painted`.
- match → `erase_stale` restores only what the new frame will not repaint: the
  old preview footprint when the new one does not contain it (or when the
  preview is ragged, because a ragged draw leaves its never-captured pixels
  untouched), and the old ring when it moved. In the steady state nothing is
  restored at all.
- the erased rect is unioned into the damage reported to the compositor, so a
  restore can never be left off-screen.

`PreviewMap` (buffer ↔ image, separable in x and y) gives the exact rect the
draw paints, and the draw clips itself to it — that rect is what makes
"contains the previous footprint" a sound test.

**Files.** `src/wayland/selection.rs` (`OverlayBuffer`, `render_live_overlays`,
`draw_live_base`, `PreviewMap`, `Painted`, `erase_stale`, `erase_rect`,
`erase_ring`, `for_each_border_pixel`).

**Measured.** 4K, release, 8 runs: the old copy 2.08–2.27 ms, steady-state
restore **0.00 ms**, a moved footprint 0.74–1.03 ms, a full mask rebuild
1.18–1.27 ms (was 1.6 ms). Memory: the 33 MB base cache per overlay is gone.
UI thread at 4K: 26.2–28.3 → **24.0–26.1 ms**.

**Acceptance.** Existing overlay tests pass unchanged; new tests pin the
uniform base, "unchanged footprint restores nothing", "moved footprint restores
what it covered", "ragged preview always restores", "a moved ring restores only
the old band", and "no pixel is drawn outside the footprint".

### 3. Reuse the capture buffer instead of a fresh memfd per frame — 6.9 → 1.9 ms (1080p)

**Problem.** `capture_region_frame_without_cursor` set `shm_image = None` at the
start of every frame, dropping the pool, buffer and mapping; the `Buffer` event
then created a new 33 MB memfd. The syscalls are free (0.01 ms) — the cost is
the first touch of a fresh set of pages, which the `ready` read pays: 19.3 ms
measured for a 33 MB mapping at 4K, against 1.2 ms to refill the same mapping.

**Approach (as built).** `ShmImage` carries a `BufferSpec` (width, height,
stride, format) and the `Buffer` handler returns early when it matches — the
pool, `wl_buffer` and mapping stay alive for the whole session. Safe because a
session only starts a frame after the previous one reached `ready`/`failed`,
and wlr-screencopy allows reusing a buffer after `ready`. A cancelled frame
(stop flag) drops the buffer instead of keeping it, because the compositor may
still be writing into it when the frame is destroyed. A geometry change
reallocates, pinned by `a_capture_buffer_is_reused_only_for_the_same_geometry`.

**Not done: recycling the `to_vec` destination.** A fresh 33 MB `Vec` costs
1.42 ms and the copy into it dominates; getting the buffer back from the UI
thread would need a second channel across the app/wayland boundary for ~0.5 ms
on a thread that is not the bottleneck. Left out deliberately.

**Measured, real screencopy on niri, 30 frames of 1920x1080, p50:** before
6.82–6.91 ms, after **1.87–1.93 ms** (p95 8.1 → 3.2 ms). At 4K the page cost is
4x, so the per-frame saving is ~19 ms: the capture thread stops allocating a
gigabyte per second of fresh pages at 30 fps.

### 4. Read canvas rows, not pixels, for the two checks — 3.9 ms → ~1.1 ms

**Problem.** The near-duplicate check and the contrast check sample every 4th
pixel of every 4th row. Through `RgbSource::pixel_rgb` each sample paid a
virtual call and, on the canvas, `CanvasStore::pixel`'s tile lookup,
working-set touch and `RefCell` borrow: `pixel` 1.28 + `touch_tile` 1.02 + the
two loops 1.64 = 3.9 ms of a 4K frame.

**Approach (as built).** `RgbSource::row_rgb_sampled_into(x, y, step, out)`
reads the samples of one row in one pass. The default stays per-pixel (mocks,
the legacy wrapper); `RgbFrame` and `ImageRgbView` gather with a stride from the
row slice; `CroppedRgbSource` forwards with the crop offset; `CanvasRegion`
calls the new `CanvasStore::sampled_row_into`, which pays one `with_row` per
sampled row instead of one per sample, and reports black for a column outside
the capture exactly as `pixel` does. The sample set is unchanged, so every
decision is unchanged.

**Files.** `src/stitch/rgb.rs` (trait method, `RgbFrame`, `ImageRgbView`,
`CroppedRgbSource`, the two checks), `src/stitch/store.rs`
(`sampled_row_into`), `src/stitch/engine.rs` (`CanvasRegion`).

**Measured.** 4K push 18.76 → **15.97 ms** (median of 3); 1080p push
4.94 → **4.17 ms**. Preview unchanged. UI thread at 4K (push + preview)
24.64 → **21.85 ms**.

**Acceptance.** All seven corpora unchanged (the sampled values are identical,
so any decision change is a bug); two tests compare the row accessor against
`pixel_rgb` for `RgbFrame`, `ImageRgbView` and `CanvasRegion`.

### 5. Build the RGB frame and the boundary descriptor in one pass — done

**Problem.** The RGBA→RGB conversion reads the 33 MB capture and writes 24.9 MB
(5.5–6.0 ms); `EdgeLadder::from_source_on` then re-read the same content to
build the 48-bin descriptor (4.3 ms). Two passes over the same pixels, and the
second one started after the first had pushed 25 MB out of the cache.

**Approach (as built).** The descriptor's evidence is now accumulated by the
conversion pass:

- `BinLayout` (`engine.rs`) owns where the bins sit across the axis
  (`margin = 5%`, 48 bins, the last one absorbing the remainder) and
  `means_into(line, out)` is the single definition of one line's bin means. The
  two-pass builder and the fused path both call it, so they cannot drift.
- `EdgeLadder::from_means(&means, along, layout, gain)` quantises and picks the
  scoring rows. The *gain* stays in the engine: it belongs to the session and
  the canvas was written on that scale, so a caller that only has the raw
  evidence cannot produce a ladder that disagrees with the canvas.
- `rgb_frame_from_source_reusing_with(source, buffer, row_hook)` hands each
  converted row to the caller while it is still hot. The plain
  `rgb_frame_from_source_reusing` is the same function with an empty hook.
- `push_frame_sources` computes the layout from the source width, accumulates
  the means in the hook, and passes `PrebuiltMeans` in `PlacementRequest`. The
  engine verifies the prebuilt describes the frame (`along`, layout, length)
  and otherwise falls back to reading it, so a mismatch is never a wrong ladder.
- The fusion is only exact when the active crop is the whole frame (the shipped
  configuration; the fixed-band detector is trace-only). A real crop keeps the
  old path — the ladder describes the cropped frame, the conversion the
  uncropped source.
- The horizontal axis is unchanged: its ladder is built from the previous
  frame's rectangle in the canvas, not from the frame being converted.

**Files.** `src/stitch/rgb.rs` (`_with` variant), `src/stitch/engine.rs`
(`BinLayout`, `PrebuiltMeans`, `from_means`, `place`), `src/stitch.rs`
(`push_frame_sources`, `place_on_axis`).

**Measured (median of 3).** 4K `push` 15.97 → **13.55 ms** (13.55 / 13.67 /
13.51); 1080p `push` 4.17 → **3.70 ms**. Preview unchanged. 4K UI thread
(push + preview) 21.85 → **19.59 ms** (59% of the 33.3 ms budget).

**Acceptance.** All seven corpora unchanged. A unit test builds a ladder from
fused means and compares it with `EdgeLadder::from_source` for two gains —
levels and scoring rows must be equal, not merely equivalent placements.

### 6. Stop converting pixels the overlay never reads — done

**Problem.** `image_from_stitched_window_reusing` converted a window of the
canvas to RGBA for the overlay. Three separate wastes:

1. **The window was three viewports tall whatever the box position.** The
   overlay maps each buffer pixel to the canvas pixel under it, so it can only
   address the capture plus the margin between the box and the output edge. The
   old window was `origin - reach .. origin + 2*reach`; at 4K, once the canvas
   had grown, that is 25 MP converted per accepted frame of which the overlay
   reads one screen.
2. **Every row was copied into a scratch buffer first.** 25 MB written plus
   25 MB read per 4K screenful, on top of the 25 MB read and 33 MB write the
   expansion already does — for a row the tile store can hand out directly.
3. **The expansion was one pixel at a time.** Measured in isolation on this
   machine (4K screenful): the `as_chunks::<3>` / `as_chunks_mut::<4>` zip
   6.7 ms, the same expansion writing 16 bytes per four pixels 3.2 ms, a plain
   memcpy of the same volume 1.6 ms.

**Approach (as built).** No new trait and no ownership change: the snapshot is
still an owned image, because the overlay renders on its own schedule and
cannot borrow the canvas across the session boundary. What changed is what gets
converted and how:

- `preview_reach` computes the exact canvas margin the overlay can address
  (`origin ± (box margin × capture density)`, plus one pixel of slack for the
  overlay's flooring), and `preview_window` is that rectangle clipped to the
  canvas. A box that fills the output needs no margin at all; the old window
  still converted three screens.
- A whole row is expanded straight out of the tile that holds it
  (`CanvasStore::with_row`), no scratch copy. The scratch is still there for
  ragged (T/L/arch) rows, where the per-column coverage test needs it.
- `expand_row` writes 16 bytes per four pixels.

**Files.** `src/stitch/canvas.rs` (`image_from_stitched_window_reusing`,
`expand_row`), `src/app.rs` (`preview_window`, `preview_reach`).

**Measured (median of 3).** 4K preview 6.0 → **3.24 ms**; 1080p 1.36 →
**0.58 ms**. Push unchanged (13.55). 4K UI thread (push + preview) 19.59 →
**16.80 ms** (50% of the 33.3 ms budget). The real session converts ~1.7
screenfuls instead of 3 (a 4K box of 3000×1600 at (400,300): 4917×2918 vs
3840×6480), so the real per-frame saving is larger than the bench's.

**Acceptance.** Overlay tests unchanged. A test pins the expansion against the
per-pixel form for widths that are not multiples of four; a test pins
`preview_reach` for a box inside the output, a box filling it, a second output
and a fractional scale. Corpora unchanged.

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
| 2 overlay base copy | **done** | the 33 MB cache is gone (uniform base): steady-state restore **0.00 ms**, old copy 2.08–2.27 ms; moved footprint 0.74–1.03 ms; mask rebuild 1.18–1.27 ms (was 1.6). UI thread at 4K 26.2–28.3 → **24.0–26.1 ms**. 1080p bench unchanged. Corpora unchanged. clippy 41 (one fewer than baseline). |
| 3 capture buffer reuse | **done** | real screencopy 1080p p50 6.82–6.91 → **1.87–1.93 ms** (p95 8.1 → 3.2); at 4K the page cost is 4x. `to_vec` recycling skipped: a fresh 33 MB Vec is 1.42 ms and needs a return channel. Corpora unchanged. clippy 41. |
| 4 canvas row sampling | **done** | 4K push 18.76 → **15.97 ms** (median of 3), 1080p 4.94 → **4.17**; preview unchanged. Corpora unchanged. Two equivalence tests. clippy 41. |
| 5 fused conversion + descriptor | **done** | 4K push 15.97 → **13.55 ms** (median of 3), 1080p 4.17 → **3.70**; preview unchanged. UI thread at 4K (push + preview) 21.85 → **19.59 ms** (59%). Corpora unchanged. Ladder-equality test. clippy 41. |
| 6 preview window + expansion | **done** | 4K preview 6.0 → **3.24 ms** (median of 3), 1080p 1.36 → **0.58**; push unchanged. UI thread at 4K (push + preview) 19.59 → **16.80 ms** (50%). Window is the exact addressable margin, not three screens. Corpora unchanged. Two tests. clippy 41. |
