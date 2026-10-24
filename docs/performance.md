# Performance

How to measure the stitcher, what the numbers were before the 2026-10-24 pass,
and which costs remain.

Everything here is measured on the reference machine (niri 26.04, Ryzen, two
1920x1080 outputs, `--release` with `lto = "thin"`, `codegen-units = 1`). Absolute
numbers move with the CPU; the *ratios* and the *shape* of the profile are the
durable part.

## The tool

`src/bin/stitch-bench.rs` is the measurement harness, and it is a permanent
asset rather than a scratch probe. Two modes:

```bash
./target/release/stitch-bench --dir testdata/frames/real-long
./target/release/stitch-bench --synthetic 1920x1080x60
./target/release/stitch-bench --synthetic 1920x1080x60 --step 120
```

It reports the three numbers that matter for a long capture, all of which the UI
thread pays per frame:

| field | what it is |
|---|---|
| `push` | the whole `push_frame_with_offset` call |
| `preview` | the overlay's window image (`image_from_stitched_window_reusing`) |
| `allocations` | bytes and count allocated by one frame, worst and steady state |

`--synthetic WxHxN` builds a deterministic scrolling page cropped into `N`
frames, so resolution scaling can be measured without a 4K capture. It is a
*page*, not noise: a page that does not genuinely scroll makes the stitcher
refuse, and the profile then measures the refusal path.

`--step PIXELS` is how far the page moves between frames (default 40). It is the
one input that decides how much *new canvas* a frame writes, and therefore the
allocation figure:

| step | push | steady-state allocation |
|---|---|---|
| 40 (default, 1 200 px/s at 30 fps) | 5.0 ms | 0.9 MB |
| 120 (a wheel notch per frame) | 5.5 ms | 9.4 MB |
| 456 (`real-scroll`'s median delta) | 7.4 ms | 14.7 MB |

The allocation growth is the canvas itself — new rows plus the packed form of
tiles that just went cold — which is retained, not churn, and is bounded by the
content captured. `canvas-raw-tiles` (no packing) measures 1.6 MB / 5.2 MB at the
same two steps with the same CPU, so the difference is storage, not work. Read
the `0.9 MB` figure as "per-frame churn at a slow scroll", not as a general claim.

`--no-preview` isolates the stitcher from the preview conversion.

The oracle is deliberately absent. `replay-frames` answers "is the placement
correct"; this answers "what does it cost", and the exhaustive search would cost
orders of magnitude more than the thing being measured.

## Profiling

```bash
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
sudo perf record -F 999 -g -p "$(pgrep -x screenshot | head -1)" -- sleep 8
sudo perf report -i perf.data --stdio --no-children -f | head -20
```

`--no-children` is what identifies the hot function; `--children` then shows
which caller is responsible. Symbols require the debug-info build — a plain
`--release` build profiles as one anonymous blob.

## Before and after

`1920x1080`, synthetic scrolling page, 30 fps budget = 33.3 ms.

| | before | after |
|---|---|---|
| push | 11.75 ms | 4.8–5.8 ms |
| preview | 3.59 ms | 1.4–1.6 ms |
| UI thread per frame | 15.3 ms (46%) | 6.2–7.4 ms (19–22%) |
| steady-state allocation | 28.1 MB / frame | 0.9 MB / frame (40 px step; see below) |

Resolution scaling after the passes (synthetic, steady state; push p50 plus
preview, plus the caller's conversion):

| frame | UI thread per frame | share of a 30 fps budget |
|---|---|---|
| 1280x900 | 3.08 ms | 9% |
| 1920x1080 | 3.77 ms | 11% |
| 2560x1440 | 6.62 ms | 20% |
| 3840x2160 | 12.55 ms | 38% |

Real corpora:

| corpus | push mean | UI thread per frame |
|---|---|---|
| `real-1` | 0.28 ms | 0.36 ms (1%) |
| `real-long` | 2.51 ms | 2.95 ms (9%) |
| `real-scroll` | 3.23 ms | 3.72 ms (11%) |
| `lowcontrast-2b` | 0.26 ms | 0.34 ms (1%) |

## What was wrong, and why

Five separate defects, all found by profiling rather than reasoning. Each hid the
others: fixing the top of the profile only reveals the next one.

### 1. The perceptual frame was dead code in a default build

`PerceptualFrame` is a `f32` luminance image — 8.3 MB on 1080p — rebuilt on every
push. Two things read it: the legacy pipeline's perceptual stage
(`stitch-legacy`) and the deep-profile trace. The shipped `WindowedEdgeMatcher`
reads its boundary descriptor instead, so a default build built and dropped it
unread. Measured 18.7% of a frame.

Now gated on `RawStitcher::perceptual_frame_needed()`. Nothing else changed.

### 2. The descriptor builder called a trait method per pixel

`EdgeLadder` sums 48 bins across a whole row, and it did so through
`RgbSource::pixel_rgb`: a virtual call, a bounds check and a four-byte stride,
per pixel, to add one luminance to an accumulator. `row_rgb_into` on `RgbSource`
reads a row at a time; the default implementation falls back to per-pixel reads
(so test mocks stay correct), and `ImageRgbView` / `RgbFrame` /
`CroppedRgbSource` override it with a slice copy. The column case stays
per-pixel on purpose — gathering a column through a row buffer reads `across`
rows to build one line.

### 3. Three conversions were written per-pixel

| conversion | before | after |
|---|---|---|
| capture BGRA → canvas RGB (`rgb_frame_from_source`) | 2.82 ms | 0.85 ms |
| canvas RGB → preview BGRA (`image_from_stitched_window`) | 3.41 ms | 1.61 ms |

The cost was not the channel swap (0.24 ms of it): it was the per-pixel
`data[i * 4 + 2]` indexing, worth 1.97 ms. The preview loop also asked
`RowCoverage::contains` once per pixel; coverage is now decided once per row,
which settles the whole row at once for the common rectangular canvas.

### 4. Two full-frame buffers were materialised per push

`rgb_frame_from_source` built the whole frame, then `crop_rgb_frame` copied a
cropped duplicate of it — and in the shipped configuration the crop is the whole
frame (the fixed-band detector is trace-only, so its `top`/`bottom` are zero).
`crop_rgb_frame_if_needed` returns `None` in that case and the caller keeps
borrowing the original.

Separately, the near-duplicate check materialised the previous viewport out of
the canvas (6.2 MB per push) only to sample every fourth pixel of it.
`CanvasRegion` already implements `RgbSource`, so the check now reads the canvas
in place.

### 5. Nothing was recycled between frames

Every push allocated ~28 MB and freed it immediately, so each frame got fresh
pages and took a kernel page fault on first touch — 4.7% of the frame, plus the
allocations. The stitcher keeps exactly two compose frames alive, so they can
cycle:

* `spare_compose_buffer` holds the allocation of the frame `commit_accepted`
  just replaced. It is deliberately *not* `previous_compose_frame`: a refused
  frame must leave the reference state untouched, and `take()`ing the previous
  frame broke exactly that (caught by
  `rejected_frame_does_not_advance_the_reference_state`).
* the preview buffer is carried by `app` across frames and passed back in. It is
  a high-water mark, not an exact fit: the overlay window grows with the canvas
  and shrinks after a selection-box edit, and reallocating on every change would
  defeat the purpose. Only `size` bytes are used; the rest stays allocated.
  The buffer comes back through `update_long_capture_preview`, which returns the
  `Image` it displaced — the session holds exactly one preview, and returning the
  old one is the only way the allocation can outlive the snapshot it belonged to.

Steady-state allocation is 0.9 MB per frame at a 40 px step, all small. It is
higher at faster scrolls (see the step table above) because the canvas grows
faster; that part is retained content, not churn.

## 2026-10-24, second pass: the descriptor inner loop

The first pass left `EdgeLadder::from_source_on` (descriptor building) as the
largest item, at 23%. Profiling the same 1080p synthetic page again showed where
it actually goes: the descriptor is accumulated *inside* the frame conversion
(`BinLayout::means_into` called from the `rgb_frame_from_source_reusing_with`
row hook), and that inner loop was 30% of the frame on its own.

Two changes:

* **`BinLayout::means_into` reads slices, not indices.** It walked each bin with
  `a in a0..a1` and recomputed `a * 3` plus two bounds checks per pixel. A
  slice per bin with `as_chunks::<3>()` removes both; the sum is integer and
  order-free, so the means are bit-identical. Alone: 1080p push p50
  3.61 → 3.16 ms.
* **The near-duplicate check and the duplicate threshold share one pass.**
  `average_difference_same_source` (every 4th pixel of every 4th row, against
  the canvas) and `luminance_range_source` (the same samples, for the
  relative threshold) walked the frame separately. Both are now
  `sampled_difference_and_luminance_range`, with identical answers. Alone:
  3.65 → 3.61 ms (the pass was 3% of the frame; the fused loop costs slightly
  more than the difference alone, so the net is 1%).

`push`, median of three runs, synthetic scrolling page:

| frame | before | after | change |
|---|---|---|---|
| 1280x900 | 2.11 ms | 1.84 ms | −13% |
| 1920x1080 | 3.65 ms | 3.17 ms | −13% |
| 2560x1440 | 6.19 ms | 5.48 ms | −11% |
| 3840x2160 | 13.50 ms | 11.82 ms | −12% |

`preview` (1920x1080) 0.60 → 0.57 ms; UI thread per frame 4.32 → 3.77 ms (11%
of a 30 fps budget). Real corpora `push mean`: `real-1` 0.31 → 0.28 ms,
`real-long` 2.66 → 2.51 ms, `real-scroll` 3.35 → 3.23 ms, `lowcontrast-2b`
0.31 → 0.26 ms.

Correctness: `real-1`'s stitched canvas is **byte-identical** before and after
(same PNG sha256 — the descriptor arithmetic is unchanged, only its shape), all
nine corpora replay to the same counts, and all four feature combinations plus
clippy are unchanged.

After this pass the conversion is memory-bound: `row_rgb_into` moves 14.5 MB per
1080p frame at ~34 GB/s and the preview's `expand_row` moves 14 MB at ~42 GB/s.
The remaining CPU is the descriptor's arithmetic (~26% of a frame), which is
already vectorised at roughly 1.3 cycles per pixel; the rest of the profile is
canvas reads, page zeroing for the growing canvas, and the colour check.

## 2026-10-24, third pass: 4K column pooling

At 4K a descriptor bin is 80 px wide (48 bins across 3840), and the per-row bin
mean was still the descriptor's dominant cost. `BinLayout::pooled_means_into`
samples one pixel in two per bin and scales the sum back, so the mean keeps the
sampled pixels' weight. Only 4K takes that path: `EDGE_POOL_MIN_BIN_WIDTH = 64`
leaves 1080p (40 px bins) and 1440p (53 px) on the exact sum, so their output is
bit-identical.

| frame | before | after | change |
|---|---|---|---|
| 1920x1080 | 3.17 ms | 3.18 ms | — (unpooled) |
| 3840x2160 | 11.82 ms | 9.86 ms | −17% |

All 59 synthetic 4K frames still place. Two shape rules, both measured:

* **The two bodies must stay monomorphic.** Choosing `step` inside the per-bin
  loop cost 1080p 3.17 → 4.20 ms: the branch defeated vectorisation of the
  common path.
* **Only the across axis is pooled.** The along axis *is* the placement
  resolution — a one-row shift is the basin — so halving it would move every
  answer.

Forced-pooling stress test (threshold temporarily 0, i.e. every corpus pooled):
real-1 204/0, real-3 65/0, real-long 97, lowcontrast-2b 78 all unchanged; only
`real-2` moved (230 → 176 accepted, different chain). That is why the threshold
is 4K-only rather than applied everywhere.

What that corpus's movement means was clarified later by `truth-frames`: 249 of
`real-2`'s 1290 steps have **no single answer** (several displacements fit
equally well), so a small perturbation to the descriptor changes which member of
the tie set is taken and the accept count is not a quality signal. The earlier
write-up of this experiment also quoted a misplaced-count improvement (200 → 127)
that turned out to be an artefact of `replay-frames`'s row bookkeeping (found.md
lesson 34) — do not use it as evidence. The decision itself stands: with no real
4K corpus, pooling stays gated at `EDGE_POOL_MIN_BIN_WIDTH = 64`.

## 2026-10-24, fourth pass: two cache-local passes on four helpers

The user's constraint is power, not latency: on parts that must raise every core
to serve one core's boost clock, a 13 ms single-threaded burst per frame is
expensive even though it fits the budget. The change parallelises only the two
passes whose work splits into independent bands with no shared mutable state:

* **conversion + descriptor** (`rgb::rgb_frame_from_source_fused`): rows are
  split into contiguous bands; each helper reads its own rows of the frame and
  writes its own `data` and `means` slices. The descriptor arithmetic is the same
  `BinLayout::means_into`, so the output is bit-identical.
* **scoring windows** (`engine::score_tops`): each candidate position is an
  independent row-window comparison against the same read-only descriptor
  canvas; results are concatenated in input order, so tie handling sees exactly
  the serial order. The per-call `ScoreCache` is consulted before the split and
  filled afterwards.

Both stay serial below their work floor (250 k px per helper for conversion,
2 M bin comparisons and 32 positions per helper for scoring), which is why the
small corpora (`real-1` 644x254) and every 1080p impulse window are unchanged.
`MAX_HELPER_THREADS = 4`: more helpers do not shorten the memory-bound part and
would spread the shared last-level cache further.

Measured, three runs each, same machine, release build:

| input | push p50 | preview | UI thread/frame |
|---|---|---|---|
| 3840x2160, before | 10.12 ms | 2.77 ms | 12.93 ms (39% of 33.3) |
| 3840x2160, after | 6.07 ms | 2.91 ms | 8.99 ms (27%) |
| 1920x1080, before | 3.25 ms | 0.61 ms | 3.90 ms (12%) |
| 1920x1080, after | 1.88 ms | 0.60 ms | 2.50 ms (8%) |
| `real-5-4k`, before | 5.87 ms | 1.82 ms | 7.75 ms (23%) |
| `real-5-4k`, after (same budget) | 5.76 ms | 1.92 ms | 6.31 ms (19%) |
| `real-5-4k`, after the working-set fix below | 0.89 ms (p50), 2.56 ms (mean) | 0.39 ms | 2.96 ms (9%) |

`real-5-4k`'s p50 barely moves because its frames are lz4-packed canvas reads,
not descriptor arithmetic; its **p95** falls 16.1 → 7.3 ms, which is the anchor
scan on refusals — the part that parallelises best. Small frames are unchanged
(`real-1` push 0.29-0.32 ms, serial path).

Power measured at the package RAPL counter during a saturated 600-frame 4K run:
serial 72 W / 11.7 s (840 J), four helpers 80 W / 9.4 s (750 J). Instantaneous
power is *higher* — four cores are running — but the burst is 30% shorter, so
energy per frame falls ~20% (0.93 → 0.74 J). For a real 30 fps session the duty
cycle drops 39% → 27%, which is the number the user's power argument rests on.
Two and three helpers were measured too (79.5 W / 10.3 s and 80 W / 9.6 s):
package power is already at the same plateau with two, so the extra helpers buy
time and energy, not frequency.

Bit-identical check: `real-4-4k` and `real-5-4k` canvases replay **byte for
byte identical** with the helpers forced serial and parallel (PNG sha256
`c6ddb92c…` / `aad70178…`), and every corpus keeps its acceptance count
(`real-4-4k` 438 / `real-5-4k` 542).

## 2026-10-24, fifth pass: the tile working set was smaller than one frame's read pattern

`replay-frames` on `real-5-4k` took **three minutes** for 812 frames. `perf` put
66% in `Lz4Tiles::unpack` and 20% in `lz4_flex` compression: the capture was
thrashing its own tile store. `CanvasStore::default_budget` was four frames'
worth of pixels (21 MB for a 2 131x835 frame), while the canvas grows to 30 MB;
every frame's duplicate check, colour check and write crossed the budget, so
every sweep packed tiles the next frame immediately unpacked.

The budget is now eight frames' worth (8 MB floor) with the reasoning written on
the constant: the working set is the previous frame's rectangle plus one
screenful of preview, and eight frames leaves slack for the sweep. A selection
that *widens* raises it too (`raise_budget_for` on the reflow path), because
`real-5-4k` starts at 499 px (budget 10 MB) and grows to 2131 px — the old
budget then packed a 29 MB canvas every frame. `stitch-bench` now prints the
finished canvas footprint (`memory_report`), which is how this is measured:

| corpus | logical | resident | packed |
|---|---|---|---|
| `real-1` (644x254) | 15.8 MB | 7.1 MB | 0.4 MB |
| `real-long` | 33.4 MB | 12.8 MB | 1.8 MB |
| `real-5-4k` (starts 499, grows to 2131) | 28.9 MB | 28.9 MB | 0.0 MB |
 Measured on
the same corpus: push mean **181 → 2.4 ms** (p50 5.87 → **0.94 ms** in
`stitch-bench`), preview 1.83 → 0.29 ms, the whole replay 180 → 30 s, and 26 s
of those 30 are the independent reprojection check. Canvases are byte-identical
(`real-4-4k` `c6ddb92c…`, `real-5-4k` `aad70178…`), every corpus keeps its
acceptance count, and synthetic 4K (a 59-frame canvas that never crossed the old
budget) is unchanged at 6.0 ms push.

The trade-off is memory and it is deliberate: the 30 MB canvas is now resident
instead of packed to ~9 MB while it fits the budget. A capture still reaches a
bounded footprint, only later — `CanvasStore::report` measures it, and the
budget is one constant to lower if a machine needs the memory back. The old
figure was not "packing works"; it was "packing ran more often than the capture
read the same tiles".

## 2026-10-24, sixth pass: the preview draw on four helpers

`stitch-bench` measures the canvas→window conversion, not the overlay draw that
happens afterwards. An in-module probe (run once, then removed; the test that
remains compares pixels, not time) rendered a 3840x2160 overlay with a
2000x1200 box:

| mapping | serial | four helpers |
|---|---|---|
| 1:1 (capture density == buffer density, the normal case) | 6.29 ms | 2.40 ms |
| scaled (per-column source lookup) | 4.27 ms | 1.90 ms |

The draw is a pure write into the overlay buffer from a read-only snapshot, so
destination rows are independent: `draw_preview_on_overlay_with_threads` splits
`footprint.y..bottom()` into bands, each with its own `&mut [u8]` slice starting
at its first row. `split_preview_draw_matches_the_serial_pixels` renders the
same inputs serially and four-ways and asserts byte equality across the 1:1 and
scaled mappings, an opaque and a ragged snapshot, and holes covering whole rows
or part of one. The auto count needs 500 k footprint pixels (small overlays stay
serial), and a hole covering the whole footprint returns before deciding
threads — the full-screen-box redraw paints nothing and must not pay a fan-out.

## What remains

* **The descriptor's arithmetic — 26% at 1080p.** Fused into the conversion row
  loop and vectorised at ~1.3 cycles per pixel; 4K halves the pixels it reads
  (column pooling above). At 1080p the only further lever is touching fewer
  pixels, which changes what the descriptor means and therefore what the gates
  mean. Not attempted.
* **`row_rgb_into` — 14%.** BGRA→RGB, 14.5 MB per 1080p frame at ~34 GB/s. The
  floor is that memcpy; the remaining cost is the 4→3 byte stride.
* **Preview `expand_row` — 11%.** RGB→BGRA for the overlay window, ~14 MB at
  ~42 GB/s. Memory-bound already.
* **`CanvasStore::pixel` / `touch_tile` — canvas reads.** `pixel` is per-pixel
  and `touch_tile` unpacks cold tiles; both are on the canvas read path, which
  the engine's colour verification and the duplicate check use.
* **4K at 30 fps is reachable now.** 3840x2160 measures 6.0 ms push + 3.0 ms
  preview (9.1 ms, 27% of the 33.3 ms budget) and `real-5-4k` is 0.86 ms p50
  after the working-set fix. The old claim that 4K was out of reach was written
  before the parallel passes and before the tile budget matched the working set.
* **The refusal-path tail** is now the largest per-frame cost on a wide 4K
  canvas: `real-5-4k` push p95 is 12.1 ms (max 14.7) because a refused frame
  scans the whole anchor window. It fits the budget, but a 3840-wide selection
  with dense content could exceed it. Levers: a shorter anchor ring ladder, an
  earlier frame-rate drop (the border already drops to 5 fps), or a cap on the
  anchor window for very tall canvases. Not attempted — measure first.
* **Canvas window → preview image** (2.9 ms at 3840x2160) is the last
  single-threaded per-frame pass; it is blocked on `CanvasStore`'s `RefCell`
  (every read mutates the working set). See `docs/backlog.md`.

## Dependency freshness

This targets Arch, ships prebuilt binaries, and declares `rust-version` in
`Cargo.toml` (currently 1.88) so cargo enforces it rather than a README note.
Dependencies are kept at the newest published version; the 2026-10-24 pass took
`png` 0.17 → 0.18, `lz4_flex` 0.11 → 0.14, and every transitive crate to latest.

Two API breaks came with those, both mechanical:

* `png` 0.18 requires `R: BufRead` (`File` must be wrapped in `BufReader`) and
  `output_buffer_size()` returns `Option<usize>`.
* `lz4_flex` 0.14 moved `block::compress` behind `alloc` (only reachable via
  `std`) and both codecs behind `safe-encode` / `safe-decode`; with
  `default-features = false` those must be named.

Verified that neither changed output: the `real-1` canvas written before and
after the upgrade decodes to **21 983 584 bytes with zero differing bytes**. A
PNG file is not byte-identical (the compressor changed) — the *pixels* are, which
is the property that matters.

## Verification discipline

A performance change is only accepted when all of these hold:

* `cargo test` green in all four feature combinations — `default`,
  `stitch-legacy`, `trace-logs`, `canvas-raw-tiles`. `stitch-legacy` matters
  because several of these changes touch code that build still compiles.
* all seven corpora unchanged: `204/97/7/3/7/7/78` accepted, `0` misplaced,
  `0` uncovered.
* `stitch-bench` measured three times, same machine, median reported.
* no probe left behind: `grep -rn 'PROBE\|probe!' src/` must be empty.
