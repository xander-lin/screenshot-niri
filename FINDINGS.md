# Durable Findings

## Project Scope

- `screenshot-rs-rebuild` is a fresh, niri-only Rust screenshot tool with binary name `screenshot`.
- Supported runtime target is niri on Wayland through wlroots protocols exposed by niri.
- Normal interactive region screenshots are in scope.
- Out of scope for user-facing/runtime behavior: non-niri compositors, X11, focus-grab behavior, and diagnostics compatibility paths.
- Long screenshots and stitching are implemented end-to-end and wired into the CLI (verified 2026-10-08 against `src/app.rs`, `src/cli.rs`, and git history).

## Module Relationships

- `cli` parses arguments and resolves output paths for `app`.
- `app` orchestrates the normal screenshot flow: niri preflight, frozen screencopy capture, selection, compositing, file write, and clipboard startup.
- `runtime` provides lightweight niri-session detection for normal capture.
- `wayland::screencopy` captures and freezes output frames for later region selection and compositing.
- `wayland::selection` draws the frozen captured frames in the layer-shell overlay while collecting drag-selected geometry.
- `wayland::selection` caches per-overlay base and dimmed frozen buffers after layer-surface configure so pointer-motion redraws avoid per-pixel frozen resampling and alpha blending.
- `geometry` maps selected logical regions to per-output capture regions.
- `image` represents captured buffers, composites selected regions, and writes PNG files.
- `stitch` is the full multi-stage matcher ported from the original project (canvas exact placement, perceptual bands, fuzzy hash, fast-motion scan, fixed-band detection, previous-frame fallback); `app` drives it from the long-capture worker thread.
- `clipboard` serves the saved screenshot through `wlr-data-control` as `image/png` or file URI data.

## Architecture Decisions

- Original long screenshot architecture exists in `/home/life/Work/screenshot-rs`; next rebuild work should first understand and port that architecture in stages.
- Manual-scroll MVP is no longer the recommended longshot target unless the original implementation approach proves infeasible for the rebuild.
- Superseded (2026-10-08): `--long` is now supported end-to-end (selection → continuous capture → stitching → PNG + clipboard), and the full original matcher was ported into `src/stitch/`.
- The earlier minimal average-RGB `RawStitcher` foundation description is superseded by the full port above; `RawStitcher` is now the orchestrating entry point over the ported submodules.
- Long capture frame rate is configurable (`--fps`, 1–240) with a hardcoded default of 60; the cap is a ceiling over damage-driven pull capture, so slower machines and static content naturally capture less often (2026-10-08).
- Selection-box edit mode (2026-10-08): E toggles editing; edges have snap targets only in edit mode; edit mode holds pointer input on the overlay so wheel events cannot scroll the underlying content; each frame is tagged with its capture region and the app/stitcher place region-changed frames at the deterministic offset-implied position (crop/band state resets for the new geometry). The canvas grows into ragged T/L shapes tracked by per-row coverage; uncovered extents output transparent.
- Do not introduce external input synthesis or app-specific automatic scrolling first; prefer porting the original pointer-passthrough continuous-capture model.
- Normal screenshot performance optimization is acceptable for now based on user feedback, so work should proceed to long screenshot foundations unless new performance data appears.

## Original Long Screenshot Findings

- Original long mode keeps the overlay session alive for the capture duration and enables pointer passthrough.
- Original long mode uses damage-based in-process screencopy up to 120fps, with capture-clean overlay acknowledgement before each frame.
- Original frame processing runs on the main thread through `RawStitcher`.
- Original stitching keeps a stitched RGB canvas plus the current viewport.
- Original live preview skips capture exclusion.
- Original long mode finishes on Enter or Space and cancels on Esc.

## Verification Memory

- Automated validation for the frozen-background implementation passed with `cargo test selection`, `cargo test`, and `cargo check`.
- Fuzzy/threshold overlap `RawStitcher` validation passed with `cargo test stitch` (39 passed), `cargo test` (69 passed), and `cargo check`.
- Manual niri session testing before the frozen-background change was reported successful for selection, capture, file writing, and clipboard behavior.
- Post-cache manual niri feedback reports selection responsiveness and CPU behavior are much improved; this does not prove all performance issues are solved.

## Maintenance Rule

- Current code, project documentation, tests, and command results remain authoritative. This file is only durable memory for recurring project context and should be updated only with stable, useful findings.
