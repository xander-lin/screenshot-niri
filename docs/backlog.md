# Improvement backlog

A durable, prioritised list of what is still worth changing. It exists so a
context reset cannot lose the findings; every item names the evidence and the
next concrete step. Move finished items to "Delivered" with the commit or
measurement that closed them.

## Delivered (do not re-open)

- **Normal-mode drag residue on fractional scale** — the user's "4K striping"
  was this, not stitched output: normal mode, *before the first release*, while
  the box shrinks under fast pointer motion. Fixed by the dirty-band outset in
  `changed_selection_rects` (found.md lesson 36); user-confirmed on 0.1.0-40.
- **Low-contrast band pages** — the magnitude-only descriptor accepted a
  one-period tone swap; `boundary_directions_agree` on thin evidence fixes it.
  Tests: `a_continuous_low_contrast_band_pattern_stitches`,
  `a_band_page_with_one_readable_edge_still_stitches`.
- **Parallel passes** — fused conversion+descriptor, scoring windows, and the
  overlay preview draw (`draw_preview_on_overlay_with_threads`: 4K 6.29 → 2.40 ms
  1:1, 4.27 → 1.90 ms scaled), all with pixel-identity tests.
- **4K-wide tile thrash** — budget 4× → 8× a frame (8 MB floor), then raised on a
  width change (`raise_budget_for`). `real-5-4k` push mean 181 → 2.50 ms, replay
  180 → 30 s, canvas byte-identical. `stitch-bench` prints `memory_report`.
- **Normal-mode `0` = every screen**; **`R` debug canvas view**.
- **Verification speed** — one test target (main.rs links the library),
  `[profile.test] opt-level = 2` (26.7 → 1.36 s warm), content-keyed truth caches
  (`--refresh`), `truth_key_for`, `stitch-bench --dir` uses manifest
  offsets/direction, corpus PNG loader shared.
- **Store write path** — `write_spans` copies a span at a time. Measured: no
  change on `real-5` (the write is not the bottleneck); kept for simplicity.
- **One-command verification** — `src/bin/verify-corpora.rs` owns the 11 corpus
  baselines, runs them with bounded concurrency, prints a table and exits
  non-zero on any mismatch. It found immediate drift: `real-4-4k` was documented
  as 447/0 while the stitcher has reported 438/2 since the direction check
  landed (lesson 38), and `real-2` as 303 ambiguous / 241 refusals instead of
  296 / 238. 46 s wall at `--jobs 2` (real-5 alone is 40 s).
- **The `real-5-4k` push tail** — it was not the refusal path (that corpus has
  none): 115 accepted frames ran a full horizontal search to answer "nothing
  moved", because a zero-delta acceptance recorded the horizontal axis as the
  impulse. Clearing the direction memory on a zero-delta step (lesson 39) took
  push mean 2.50 → 0.83 ms, p95 12.14 → 1.54 ms, UI thread 2.86 → 1.17 ms, with
  every corpus baseline unchanged.
- **Tail attribution** — `stitch-bench` prints the slowest frames with their
  outcome and canvas footprint; with `--features trace-logs` +
  `SCREENSHOT_TRACE_PROFILE=1` it prints the per-stage breakdown, and the
  shipped path now fills `frame_prepare` / `canvas_match` (they were legacy-only,
  which is why the tail could not be attributed before).
- **Preview window conversion** — split across four helpers through a new
  `CanvasStore::with_resident_rows` (unpack once, hand out `Sync` row slices):
  3840x2160 2.9 → 1.9-2.1 ms, `real-5-4k` 0.36 → 0.24 ms. Memory-bound now; a
  further gain would need to move fewer bytes, not more threads. The full-canvas
  conversion at the end of a capture uses the same path.

## P0 — next

1. **Geometry-edit cost (~10-12 ms per edit).** The five edits in `real-5-4k`
   are now the slowest frames: `reseed_from_canvas` rebuilds the descriptor rows
   from the canvas in 512-row bands. It happens once per edit (not per frame), so
   it does not threaten 30 fps, but it is the largest single burst left. Next
   step: measure the band cost and consider skipping bands whose rows cannot
   affect the next search (the engine only needs the rows the guard/anchor can
   reach), which must keep every corpus baseline identical.

3. **Verification runner follow-ups** (the runner itself is delivered): a
   `--record` mode that prints a new baseline table after an intentional change,
   and running `truth-frames` alongside when a change could move the pixel-level
   ground truth. Neither is needed today.

## P1 — measured, smaller, or risky

4. **A genuine horizontal placement costs ~8.7 ms.** `place_horizontally` builds
   three descriptors from pixels per call: the frame's ladder (session gain +
   levels), the canvas strip's ladder, and the search. The zero-delta fix removed
   the *spurious* calls; a real snake capture still pays this on every turn.
   Next step: measure a snake corpus (or synthesise one with `--synthetic` plus
   a horizontal path), then build the ladders from one contiguous strip copy
   instead of per-pixel store reads. Keep the pixels identical.

5. **`replay-frames` reprojection check** — 26 s of the 30 s `real-5-4k` replay.
   It locates each accepted frame's content in the canvas the stitcher just
   built, so it is not cacheable across stitcher changes (it *is* the independent
   measurement). Options: leave it; add an opt-in `--fast` (local search ±2 frame
   heights around the recorded row, full scan as fallback) documented as a
   different verdict; or run corpora concurrently.

6. **Overlay commit/damage cost** — the preview *draw* is now measured and
   parallel; the commit and damage path at 4K is not in any benchmark. Measure in
   a live session (or a probe that exercises `render_live_overlays` on a 4K
   buffer). If material, damage only the changed rows/border.

7. **`truth-frames` consensus** — `Mosaic::loo_cost` walks each frame twice (fit
   gain/offset, then median). Fusing needs an approximate gain first. Verdicts
   are cached, so only a first run on a new corpus pays it; low value.

8. **`capture-frames` writes `session.json` every frame** — O(n²) bytes (813
   frames ≈ 10 MB). Flush every N frames + on exit if corpora get much longer.

9. **Preview draw thread spawn per redraw** — four scoped spawns per accepted
   frame (~0.1 ms). A persistent pool would remove it; not worth the lifecycle
   until something else needs a pool.

10. **Code hygiene** — `draw_preview_rows` takes 16 arguments; a small context
   struct would read better. No behaviour change, low priority.

## P2 — known limitations, documented, not scheduled

- **Mirrored/clone outputs** make long capture refuse (every rectangle spans).
- **A capture that starts on a featureless screen** cannot know how far it
  scrolled; refused rather than guessed
  (`a_capture_that_starts_on_a_featureless_screen_is_not_guessed`).
- **Sticky headers/footers** are out of scope (product decision; use `E`).
- **1.2× fractional scale** — a 1-logical-px scroll is 1.2 physical px, so
  integer-row matching cannot align every step; `real-4-4k` records 45 displaced
  band flags with 0 far placements. Sub-pixel/phase matching would invalidate
  every gate calibration; do not attempt without a concrete user report.
- **`stitch-bench --synthetic` has no geometry changes** by construction; use
  `--dir` for the production path.
- **Architectural options, deliberately unscheduled** (moved here from
  `found.md`'s old list): (a) strip the overlay into four bands around the box
  instead of one full-screen surface — the memory win is large at 4K/multi-output
  but it multiplies surfaces, configures and input regions; (b) a dmabuf/GPU
  path — zero-copy but drags in EGL/GBM and a driver matrix, against the
  minimal-dependency principle; (c) deciding whether the health-baseline stress
  client and uinput scripts should live in `tools/`.

## Pending real-machine confirmation

- Band-pattern page (the user's real page) stitches through thin evidence.
- `0` in normal mode saves one image covering every screen.
- `R` shows the stitched canvas in the box (amber border, capture paused).
- 4K long capture: preview follows the scroll; per-frame UI time down.
- Normal-mode drag with a shrinking box on the fractional-scale output: no
  residue (user-confirmed on 0.1.0-40; re-check after later changes).

## Measurement debt

- The overlay draw was measured with an in-module probe (removed); re-measuring
  means re-adding a timing test temporarily or tracing a live session. The
  remaining timing test compares pixels only.
- `stitch-bench` reports push, canvas→window conversion and the final canvas
  footprint; it does **not** include the overlay draw, commit or Wayland
  round-trips. Say which one a number covers when quoting it (some older numbers
  in `docs/performance.md` predate the manifest-offset fix and are labelled).

## Reproduce the baselines

```
cargo test                                   # 261 tests, ~1.3 s warm
cargo build --release --bins
./target/release/verify-corpora --jobs 2     # 11 corpora, ~46 s, non-zero on drift
./target/release/stitch-bench --dir testdata/frames/real-5-4k   # push p50 0.86 ms
```

Expected acceptance counts are in `AGENTS.md` and
`testdata/frames/README.md`; `MISPLACED`, `UNCOVERED` and displaced bands must
stay at their recorded values.
