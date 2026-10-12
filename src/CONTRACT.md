# Module Contracts

## `config`

- Responsibilities: load optional user settings (`max_fps`, `border_width`) from `$XDG_CONFIG_HOME/screenshot/config` (default `~/.config/screenshot/config`), parse simple `key = value` lines with `#` comments, and clamp values to valid ranges.
- Non-responsibilities: CLI parsing, Wayland interaction, and applying settings (consumers apply them).
- Inputs: config file text (missing file means defaults).
- Outputs: `Config { max_fps, border_width }` plus the shared default/range constants.
- Dependencies: standard library only (no TOML dependency by design).
- Forbidden dependencies: Wayland clients and CLI modules.
- Failure modes: unreadable or malformed files degrade to defaults (never fatal).
- Verification method: unit tests for parsing, comments, padding, unknown keys, and out-of-range values.
- Replacement contract: keep the file optional, non-fatal, and dependency-free.

## `cli`

- Responsibilities: parse supported flags, reject invalid combinations, resolve output paths, and render help text.
- Non-responsibilities: Wayland interaction, filesystem image writing, and clipboard serving.
- Inputs: process args, `XDG_RUNTIME_DIR`, `XDG_PICTURES_DIR`, `HOME`, and explicit paths/names.
- Outputs: `Args` with mode, clipboard mode, output path, and long-capture frame-rate cap (`--fps`, default 30, range 1-240).
- Dependencies: standard library and `libc` for local timestamp and uid fallback path.
- Forbidden dependencies: compositor-specific code and filesystem capture logic.
- Failure modes: unknown flags, missing flag values, unsafe temp directory, invalid `--name`, out-of-range or non-numeric `--fps`, and mutually exclusive output modes.
- Verification method: unit tests for parsing and path resolution, plus `cargo check`.
- Replacement contract: preserve user-facing flag behavior and return typed options without side effects beyond private temp-dir creation.

## `app`

- Responsibilities: orchestrate normal screenshot flow, pass frozen output frames into selection, orchestrate long capture (fps-capped worker thread, region-tagged frames, PrepareCapture handshake using the exact clean-render rectangle), and drive offset-aware stitching. Launch without compositor-name/environment-hint gating.
- Non-responsibilities: low-level protocol dispatch and PNG encoding internals.
- Inputs: parsed `Args` (including `max_frame_rate`).
- Outputs: saved PNG path and clipboard provider startup.
- Dependencies: `cli`, `wayland::screencopy`, `wayland::selection`, `geometry`, `image`, `stitch`, and `clipboard`.
- Forbidden dependencies: compositor adaptation branches and diagnostics bins.
- Failure modes: unsupported multi-output long selection, unavailable Wayland connection/protocols, cancelled selection, capture failure, write failure, and clipboard setup failure. Worker errors are propagated rather than silently saving a partial capture.
- Verification method: integration/manual test under niri and `cargo check`.
- Replacement contract: preserve freeze, select, composite, write, clipboard; long capture keeps pacing, damage waits, and per-frame region tags. Use output-local logical coordinates for screencopy requests, with offsets converted to returned-frame pixels for stitching. PrepareCapture returns the exact rendered rectangle after synchronizing the selected output's commit; frame receipt releases its exclusion. Discard frames invalidated by edits before deduplication/stitching. Skip requests during active edit drags or unavailable buffers. Stop must interrupt damage waits. Compatibility is user-tested, not gated by `NIRI_SOCKET` or `XDG_CURRENT_DESKTOP`.

## `geometry`

- Responsibilities: convert logical selected rectangles into per-output capture regions; clamp spanning selections to the output containing the drag's start point (`clamp_rect_to_output_at_point`, falling back to the largest piece only when the anchor lies on no output).
- Non-responsibilities: pointer input, Wayland registry handling, and pixel copying.
- Inputs: selected logical rect and logical output metadata.
- Outputs: selected viewport and scaled capture regions.
- Dependencies: standard library and screencopy region types.
- Forbidden dependencies: Wayland connection state or image buffers.
- Failure modes: empty selections, non-intersecting selections, negative/overflowing sizes.
- Verification method: unit tests for intersections, scaling, and mixed-output offsets.
- Replacement contract: return deterministic capture regions for downstream compositing.

## `image`

- Responsibilities: represent SHM images, convert wlroots formats to RGBA PNG, and blit regions.
- Non-responsibilities: Wayland capture, path policy, and clipboard ownership.
- Inputs: captured image buffers and selected capture regions.
- Outputs: PNG files and composite image buffers.
- Dependencies: `png` and `wayland-client` SHM format types.
- Forbidden dependencies: compositor branches and selection UI state.
- Failure modes: unsupported SHM format, invalid stride/geometry, short buffers, and filesystem write errors.
- Verification method: unit tests for pure blit/format behavior and `cargo check`.
- Replacement contract: accept XRGB/ARGB8888 buffers and produce valid RGBA PNG output.

## `stitch`

- Responsibilities: provide the multi-stage vertical matcher (canvas placement, perceptual bands, fuzzy hash, fast-motion scan, fixed-band detection, previous-frame fallback) plus offset-tagged geometry placement: frames arrive with the on-screen offset of their capture region, and a changed offset/size stitches the frame at its deterministic known position, growing the canvas into ragged T/L shapes with per-row coverage so never-captured corners output transparent.
- Non-responsibilities: CLI flag handling, Wayland capture, scrolling, output selection, and PNG writing.
- Inputs: existing `image::Image` buffers via `ImageRgbView` (or any `RgbSource` in tests) tagged with `FrameScreenOffset`, overlap search bounds, and optional search direction filters (`Down`/`Up`/`Left`/`Right`).
- Outputs: push results for initialized, duplicate, accepted, and no-match frames; stitched frames with viewport/anchor origins, per-row coverage, and `image_from_stitched_frame` BGRA output with transparent uncovered extents.
- Dependencies: `image::Image`, standard error handling, and `wayland-client` SHM format names.
- Forbidden dependencies: Wayland protocol flow, runtime environment checks, filesystem access, clipboard state, and CLI modules.
- Failure modes: unsupported formats, zero dimensions, short stride/data, width/stride/format mismatch, invalid overlap ranges, no overlap within the RGB threshold, geometry-frame overflow, and arithmetic overflow.
- Verification method: unit tests for exact and thresholded overlap detection, canvas placement, mismatch rejection, geometry extension/widening/shrink/movement placement with coverage and transparency, near-duplicate handling, direction-filtered no-match preservation, empty finish, plus `cargo check`.
- Replacement contract: keep stitching deterministic and side-effect-free; geometry-changed frames keep being placed at the offset-implied position with crop/band state reset for the new geometry rather than fuzzy-matched. Preserve covered pixels rather than the canvas bounding rectangle: later widening must fill recaptured strips inside that rectangle. Newly added narrow rows start coverage at their actual x offset, and empty rows export transparent. Wide/narrow/wide captures may retain transparent side notches where content was never captured.

## `clipboard`

- Responsibilities: serve saved screenshots through `wlr-data-control` in image or URI mode.
- Non-responsibilities: selecting regions, taking screenshots, and deciding output paths.
- Inputs: output path and clipboard mode.
- Outputs: Wayland data-control source offers.
- Dependencies: `wayland-client` and `wayland-protocols-wlr` data-control protocol.
- Forbidden dependencies: external clipboard commands and compositor-specific fallbacks.
- Failure modes: missing data-control manager, missing seat, provider startup failure, or send I/O failure.
- Verification method: manual paste tests under niri and `cargo check`.
- Replacement contract: keep `image/png` and file URI clipboard modes available from a saved path.

## `wayland::screencopy`

- Responsibilities: bind wlroots screencopy, capture outputs into SHM buffers, and expose frozen-output data.
- Non-responsibilities: selection UI, PNG writing, and long capture.
- Inputs: Wayland globals and requested output-local logical regions (distinct from pixel-space rectangles used to crop frozen images); persistent capture also accepts a stop flag.
- Outputs: captured output frames and region metadata; persistent capture returns `None` on cancellation and errors on protocol failure.
- Dependencies: `wayland-client`, `wayland-protocols-wlr`, `tempfile`, and `libc`.
- Forbidden dependencies: non-wlr capture APIs and compositor adaptation branches.
- Failure modes: missing globals, unsupported buffer format, failed SHM allocation, screencopy failure, and protocol cancellation.
- Verification method: manual niri capture plus `cargo check`.
- Replacement contract: provide frozen output frames suitable for selection-time compositing. Persistent damage waits must poll the stop flag with bounded latency so joining the worker does not require fresh screen damage.

## `wayland::selection`

- Responsibilities: bind layer-shell/xdg-output, display frozen output frames with dimmed outside-selection mask and visible border, redraw dirty overlay regions during drag, return a drag-selected viewport, and provide selection-box edit mode (E key) with per-edge snap targets, output-clamped edge drags, pointer-input grabbing (wheel blocking) during long-capture editing, long-mode preview and status, horizontal/vertical direction hints, and live selection-rect/output-info accessors for the capture loop. During edits the preview exclusion covers the drag trajectory; the visible border follows the current box. Pending captures are invalidated on geometry change.
- Non-responsibilities: screencopy, PNG writing, clipboard, and capture-region recomputation policy.
- Inputs: frozen captured output frames, Wayland pointer/keyboard events, and output logical geometry.
- Outputs: selected viewport or cancellation; long-session status, direction hints, live selection rect, and output infos.
- Dependencies: `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr`, `tempfile`, and `libc`.
- Forbidden dependencies: focus-grab extensions and compositor-name checks.
- Failure modes: missing layer-shell/compositor/shm/seat/output globals, missing frozen output frame, unsupported frozen frame format, no pointer, cancellation, or empty selection.
- Verification method: unit tests for overlay pixels/dirty regions/edge hit-testing/edge clamping, manual niri drag-selection and edit-mode test, and `cargo check`.
- Replacement contract: return geometry-only selection data usable by screencopy compositing; preserve edit-mode input semantics. Map through the real logical output rect and actual overlay dimensions. Cache only dim/hole; composite preview then border every time. Frozen/live borders share one closed outside-ring pixel rule, including corners and configured thickness. Frozen damage includes the ring; edit borders follow the current rectangle, not the larger exclusion hole. `prepare_capture_clean` commits on the selected output and roundtrips before acknowledgment. `complete_capture` returns false for any frame exposed to a geometry change (sticky even after moving back); the app must discard it before stitching. Preview remains excluded from the pending region until completion.
