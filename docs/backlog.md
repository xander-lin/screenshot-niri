# Improvement backlog

A durable, prioritised list of what is still worth changing. It exists so that a
context reset does not lose the findings; every item names the evidence and the
next concrete step. Keep it current: move finished items to the "done" section
with the commit or measurement that closed them.

## Delivered (context, so it is not re-opened)

- **Normal-mode drag residue on fractional scale** — the user's "4K striping"
  report was this, not stitched output: normal mode, *before the first release*,
  while the box shrinks under fast pointer motion. Fixed by the dirty-band
  outset in `changed_selection_rects` (found.md lesson 36); user-confirmed gone
  on 0.1.0-40.
- **Low-contrast band pages** — magnitude-only descriptor + absolute colour
  ceiling accepted a one-period tone swap; now `boundary_directions_agree` on
  thin evidence. Tests: `a_continuous_low_contrast_band_pattern_stitches`,
  `a_band_page_with_one_readable_edge_still_stitches`.
- **Two parallel passes** — fused conversion+descriptor and scoring windows
  (`src/stitch/parallel.rs`), bit-identical, four helpers.
- **4K-wide tile thrash** — `CanvasStore::default_budget` 4× → 8× a frame:
  `real-5-4k` push mean 181 → 2.56 ms, replay 180 → 30 s, canvases unchanged.
- **Normal mode `0` = all screens**; **`R` debug canvas view**.
- **Verification speed** — one test target, `[profile.test] opt-level = 2`,
  content-keyed truth caches (`--refresh`), `stitch-bench --dir` now uses the
  manifest's offsets/direction.

## P0 — worth doing next

### 1. Preview path: the last single-threaded per-frame pass

Measured (`stitch-bench`, synthetic 3840x2160): `preview` 2.9 ms of a 9 ms
UI-thread frame, on top of push 6.0 ms. On `real-5-4k` it is 0.39 ms. Two
separate costs, only one is `image_from_stitched_window_reusing`:

- **Canvas window → RGB image** (`src/stitch/canvas.rs`): row loop calling
  `store.with_row`, then `expand_row` (RGB→BGRA, ~42 GB/s). Memory-bound, so
  parallel rows would need concurrent store reads.
- **Overlay preview draw** (`draw_preview_on_overlay` in
  `src/wayland/selection.rs`): per-buffer-pixel copy from the snapshot, executed
  inside `render_live_overlays`. Not covered by any offline benchmark; measure
  it first (a probe that calls the renderer on a 4K buffer, or tracing in a live
  session).

Next step: (a) measure the overlay draw; (b) if it dominates, parallelise it —
it is a pure write into the overlay buffer from a read-only snapshot, the same
shape as the two passes already parallelised; (c) only then consider the canvas
window, whose blocker is `CanvasStore`'s `RefCell` (every read mutates the
working set through `touch_tile`). A concurrency-safe read path means unpacking
under a write lock and converting outside it, or a per-tile read handle; a plain
`Mutex<Vec<Tile>>` would serialise the lz4 decode and is not worth it.

### 2. `replay-frames` reprojection check: 26 s of the 30 s 4K replay

`locate_in_canvas` scans every canvas row for every accepted frame because the
engine's recorded row is not trustworthy (found.md lesson 34). This is the
*independent measurement*, so it is not cacheable across stitcher changes.

Options, in order of preference:
1. Leave it as the gold standard and accept 30 s on `real-5-4k` (it is one run
   per verification pass, and the corpora can be run in parallel).
2. Add an opt-in `--fast` that searches ±2 frame heights around the recorded row
   first and falls back to the full scan when the local best is poor. It can
   differ from the gold standard on repeated content, so it must be documented
   as a *different* verdict, never the default.

### 3. Overlay commit/damage cost at 4K

Not measured offline. The live path recomposites the preview then the border on
every redraw (`draw_live_foreground`). At 3840x2160 that is up to 8 MP written
per accepted frame. If a live profile shows it, the border could be damaged
separately from the preview, and the preview damage could be the changed rows
only.

## P1 — measured, smaller, or risky

### 4. `CanvasStore::write_spans` per-pixel loop

`src/stitch/store.rs` copies three bytes per column with bounds checks per
pixel. A span copy (`copy_from_slice` per `(start,end)`, clamped once) is
semantically identical for valid spans and should cut the write path at 4K
(5.3 MB per accepted frame on `real-5`). Measure with `stitch-bench`; the store
tests (`a_write_into_a_packed_tile…`, `a_written_row_reads_back_exactly`) pin
the behaviour.

### 5. `truth-frames` consensus stage: two `walk` passes per candidate

`Mosaic::loo_cost` walks the frame twice (fit gain/offset, then median). Fusing
would need an approximate gain first. Only the first run pays it now (verdicts
are cached), so this is low value; do it only if truth-frames is used on new
corpora often.

### 6. `capture-frames` rewrites `session.json` every frame

O(n²) bytes written (813 frames ≈ 10 MB). Negligible today; if corpora get much
longer, flush every N frames plus on exit.

### 7. 1.2× corpora and the 45 band flags on `real-4-4k`

A 1-logical-px scroll is 1.2 physical px, so integer-row matching cannot align
every step; `real-4-4k` records 45 displaced-band flags with 0 far placements.
This is a property of fractional scaling, not a defect in the current model.
Sub-pixel/phase matching is the only real fix and would invalidate every gate
calibration; do not attempt without a concrete user report that needs it.

## P2 — known limitations, documented

- **Mirrored/clone outputs**: `rect_spans_outputs` sees every rectangle as
  spanning, so long capture refuses on a clone pair. Normal mode composites
  correctly.
- **A capture that starts on a featureless screen**: the scroll through it is
  unobservable; the engine refuses rather than guessing
  (`a_capture_that_starts_on_a_featureless_screen_is_not_guessed`).
- **Sticky headers/footers** are out of scope by product decision (use `E`).
- **`stitch-bench --synthetic` has no geometry changes** by construction; use
  `--dir` with a real corpus for the production path.

## Measurement debt

- The overlay draw and commit are only measurable in a live session; a probe
  that renders one 4K frame into a buffer and times `draw_preview_on_overlay`
  would make item 1/3 decidable offline.
- `stitch-bench` reports push and canvas→preview conversion, not the overlay
  draw. Say which one a number covers when quoting it.

## Reproduce the current baselines

```
cargo test                     # 257 tests, ~1.3 s warm
./target/release/replay-frames --dir testdata/frames/real-1
./target/release/replay-frames --dir testdata/frames/real-5-4k   # 30 s
./target/release/stitch-bench --dir testdata/frames/real-5-4k     # push p50 0.89 ms
```

Expected acceptance counts are in `AGENTS.md` and
`testdata/frames/README.md`; `MISPLACED`, `UNCOVERED` and displaced bands must
stay at their recorded values.
