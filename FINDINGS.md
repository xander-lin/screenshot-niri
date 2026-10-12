# Durable Findings

## Project Scope

- `screenshot-rs-rebuild` is a Wayland screenshot tool with binary name `screenshot`, originally developed against niri's wlroots protocols.
- The launcher no longer checks `NIRI_SOCKET` or `XDG_CURRENT_DESKTOP`; it attempts the Wayland protocols and lets users test compositor compatibility.
- Normal interactive region screenshots are in scope.
- Out of scope for user-facing/runtime behavior: X11, focus-grab behavior, and diagnostics compatibility paths. Other Wayland compositors are not rejected by name and are user-tested.
- Long screenshots and stitching are implemented end-to-end and wired into the CLI (verified 2026-10-08 against `src/app.rs`, `src/cli.rs`, and git history).

## Module Relationships

- `cli` parses arguments and resolves output paths for `app`.
- `app` orchestrates the normal screenshot flow: frozen screencopy capture, selection, compositing, file write, and clipboard startup; there is no compositor-name preflight.
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
- Long capture frame rate is configurable (`--fps`, 1–240) with a hardcoded default of 30 (2026-10-08: measured real-world stitching stays reliable even at 10 fps, so the default dropped from 60 to 30); the cap is a ceiling over damage-driven pull capture, so slower machines and static content naturally capture less often.
- Capture pauses while an edit-mode edge drag is in progress; only the settled rectangle after release is captured (2026-10-08).
- Constant session memory is resolution-driven, not fps-driven: per output, 2 full-resolution RGBA overlay buffers plus a base-pixels cache, plus the stitched canvas and the frame pipeline; `OVERLAY_BUFFER_COUNT` reduced 3→2 accordingly (2026-10-08). Per-frame capture SHM is munmapped on drop (no leak).
- The `--long` live selection box was invisible during drag on outputs not at the logical origin or with scale ≠ 1: the live dim path intersected the global-logical selection with a pseudo `(0,0,buffer)` rect. Fixed by mapping through the output's real logical rect plus scale (`selected_buffer_rect`) (2026-10-08).
- Config file + border safety (2026-10-08): `~/.config/screenshot/config` (`max_fps`, `border_width`); long captures inset one logical pixel per side so the overlay border never bleeds into frames; captures always wait for damage so the clean overlay frame is composited first; spanning selections warn red and clamp into edit mode on release; edit mode also moves the box by interior drag.
- The lingering post-screenshot `screenshot` process is the detached `--clipboard-provider`: Wayland requires the data-source process to stay alive to serve pastes; a new copy (or next screenshot) cancels it, so at most one exists at a time. Expected behavior, not a leak.
- Selection-box edit mode (2026-10-08): E toggles editing; edges have snap targets only in edit mode; edit mode holds pointer input on the overlay so wheel events cannot scroll the underlying content; each frame is tagged with its capture region and the app/stitcher place region-changed frames at the deterministic offset-implied position (crop/band state resets for the new geometry). The canvas grows into ragged T/L shapes tracked by per-row coverage; uncovered extents output transparent.
- White-border contamination root cause (2026-10-08): stitched long captures could contain a spurious white box whose geometry matched a selection border offset from the capture region (verified by pixel-scanning the output: 3px pure-white rectangle at a frame seam). The capture region (worker-side snapshot) and the overlay render rectangle (UI-thread `drag` state) had no same-source guarantee: the worker read its region before `PrepareCapture`, while the clean render used whatever rectangle the UI thread had at dispatch time — edit drags changed it in between, so the capture region swept across freshly drawn borders. Fixed by making the PrepareCapture ack carry the region derived from the same rectangle as the clean render (skip the frame while a drag is active or when the clean render cannot commit), by suppressing the border and rendering the drag-trajectory union as the hole during edit drags, and by keying the dim/base cache on (size, hole rect, border color, border visibility) instead of buffer size alone. Same fix also aligned the full-dim render path's hole mapping with `selected_buffer_rect` (scale-aware), matching the fast path.
- Capture exclusion hardening (2026-10-12): the original handshake could restore the border immediately after edit release while a damage-waiting frame was still pending, and could commit a clean buffer on an unrelated output. `capture_pending` now keeps the exact capture rectangle transparent and border-free through completion, the handshake targets the selected output and roundtrips before acknowledgment, and damage waits poll a stop flag. Long screencopy requests are output-local logical coordinates; returned pixel dimensions and output scale are converted separately for stitching.
- niri screencopy semantics (verified in niri source, 2026-10-08): a `copy_with_damage` frame is queued and copied at the next output redraw, and the copied content is the render of **every submit niri has received by that redraw** — not just the client's preceding commit. Therefore capture safety depends on the overlay state at redraw time, not on commit/request ordering. The earlier claim that edit-release races were unreachable by human input was incorrect: damage waits and scheduling have no fixed completion deadline. Exclusion now lasts until the frame completes.
- Spanning-selection clamping follows the click anchor (2026-10-08): the box belongs to the output the drag started on (`clamp_rect_to_output_at_point`), not the output covering the most area. Multi-output overlay renders now go through `queue_render` so a busy buffer on one output cannot leave it showing a stale border color (red/white mix during spanning drags). The long-mode preview maps stitched content through the frame's own capture region (`capture_region_logical_rect`, the inset rectangle), not the un-inset selection box — the previous one-pixel offset made E-extended captures re-capture shifted preview pixels as misaligned seams.
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

- Closed persistent borders and repeated resize (2026-10-12): frozen selection and live capture now use the same outside-ring predicate, including all four corner squares and configured widths 1–16. Dirty redraw bounds include the ring. The visible edit border follows the current box continuously; a pending frame is marked invalid on any geometry change and discarded before dedup/stitching, even if the rectangle moves back. This supersedes earlier border-suppression behavior. The wide → narrow → scroll → wide regression found that canvas bounding-box overlap prevented recaptured side strips from being copied; writes now preserve only covered row extents, initialize new row coverage at the true x offset, and leave empty rows transparent. Uncaptured middle side strips remain intentional notches. Fully disjoint horizontal captures still use a single coverage interval per row, so arbitrary internal holes are not represented exactly.

- Live overlay stacking/cleanup (2026-10-12): preview drawing previously followed a cached border and overwrote it. Live redraws now share a single dim/hole → preview → border compositor; border is outside the cache and drawn last whenever capture safety permits. Removed identical overlay constructors and the unused borderless-render entrypoint, and routed input redraws through the preview-aware live renderer. There is one user selection, one stitched-image preview, and one overlay surface per output; temporary capture/drag rectangles are exclusion geometry, not additional visible boxes. Regression tests exercise all four border edges, white/red colors, thickness/visibility changes against the same cached base, and pending-capture transparency. 280 tests pass; live verification remains pending.

- Automated validation for the frozen-background implementation passed with `cargo test selection`, `cargo test`, and `cargo check`.
- Fuzzy/threshold overlap `RawStitcher` validation passed with `cargo test stitch` (39 passed), `cargo test` (69 passed), and `cargo check`.
- Manual niri session testing before the frozen-background change was reported successful for selection, capture, file writing, and clipboard behavior.
- Post-cache manual niri feedback reports selection responsiveness and CPU behavior are much improved; this does not prove all performance issues are solved.
- Edit-mode/geometry-stitching validation (2026-10-08): mutation testing initially showed four injected bugs surviving the full suite (region-change duplicate bypass, crop-anchor delta, Enter not leaving edit mode, preview transparency); decision logic was extracted into pure functions (`should_skip_duplicate_frame`, `resolve_key_action`/`apply_key_action`) and regression tests added until each mutation is caught. Suite: 240 tests. Wayland dispatch/compositor behavior (input-region grabbing, wheel blocking, passthrough toggling) remains manually-testable only under a live niri session.
- White-border fix validation (2026-10-08): `cargo test` 272 tests pass (126 lib + 146 bin); new regression tests cover `prepare_capture_region` (inset + spanning rejection), drag-union hole growth, border suppression during drags, and union-driven render selection. Live confirmation that stitched captures stay border-free across E-edits is pending manual niri testing.
- Capture hardening validation (2026-10-12): regression tests cover pending-capture pixels after shrink/move/release with preview redraw, scale-2 outputs using logical-size overlay buffers, logical screencopy requests with pixel-space stitching offsets, and tiny-region bounds. Tests pass (129 lib + 149 bin; two existing profiling tests ignored); `cargo check --locked` and `cargo build --release --locked` pass, with two existing unused-import warnings in `cli.rs`. Release-binary smoke checks for normal and long modes with Niri hints unset and desktop set to `sway` both reached the deliberately unavailable Wayland connection instead of a compositor-name rejection. Live compositor capture remains to be verified.

## Maintenance Rule

- Current code, project documentation, tests, and command results remain authoritative. This file is only durable memory for recurring project context and should be updated only with stable, useful findings.
