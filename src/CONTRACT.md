# Module Contracts

## `cli`

- Responsibilities: parse supported flags, reject invalid combinations, resolve output paths, and render help text.
- Non-responsibilities: Wayland interaction, filesystem image writing, and clipboard serving.
- Inputs: process args, `XDG_RUNTIME_DIR`, `XDG_PICTURES_DIR`, `HOME`, and explicit paths/names.
- Outputs: `Args` with mode, clipboard mode, and output path.
- Dependencies: standard library and `libc` for local timestamp and uid fallback path.
- Forbidden dependencies: compositor-specific code and filesystem capture logic.
- Failure modes: unknown flags, missing flag values, unsafe temp directory, invalid `--name`, and mutually exclusive output modes.
- Verification method: unit tests for parsing and path resolution, plus `cargo check`.
- Replacement contract: preserve user-facing flag behavior and return typed options without side effects beyond private temp-dir creation.

## `app`

- Responsibilities: orchestrate normal screenshot flow, run niri-only preflight for normal capture, pass frozen output frames into selection, and reject unsupported long screenshots.
- Non-responsibilities: low-level protocol dispatch and PNG encoding internals.
- Inputs: parsed `Args`.
- Outputs: saved PNG path and clipboard provider startup.
- Dependencies: `cli`, `runtime`, `wayland::screencopy`, `wayland::selection`, `image`, and `clipboard`.
- Forbidden dependencies: long-capture modules, compositor adaptation branches, and diagnostics bins.
- Failure modes: unsupported `--long`, non-niri runtime environment, cancelled selection, capture failure, write failure, and clipboard setup failure.
- Verification method: integration/manual test under niri and `cargo check`.
- Replacement contract: keep the same high-level sequence: preflight, freeze, select, composite, write, clipboard.

## `runtime`

- Responsibilities: detect whether normal screenshot capture appears to be running inside niri.
- Non-responsibilities: Wayland registry probing, compositor fallback selection, and clipboard-provider gating.
- Inputs: `NIRI_SOCKET` and `XDG_CURRENT_DESKTOP`.
- Outputs: success for niri-like sessions or a clear niri-only diagnostic.
- Dependencies: standard library environment access.
- Forbidden dependencies: Wayland protocol clients and non-niri compositor branches.
- Failure modes: missing or empty niri session hints.
- Verification method: pure unit tests for injected environment values, plus `cargo check`.
- Replacement contract: keep normal screenshot preflight lightweight and side-effect-free apart from reading process env.

## `geometry`

- Responsibilities: convert logical selected rectangles into per-output capture regions.
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

- Responsibilities: provide pure vertical image overlap detection with a small average RGB difference threshold, exact in-canvas placement detection, append/prepend operations, and a minimal test-only `RawStitcher` state foundation with current viewport, viewport-relative Top/Bottom match metadata, near-duplicate average RGB difference detection before placement, and explicit direction filtering for future long-screenshot assembly.
- Non-responsibilities: CLI flag handling, Wayland capture, scrolling, output selection, PNG writing, and enabling `--long` runtime behavior.
- Inputs: existing `image::Image` buffers using the project BGRA/XRGB/ARGB8888 convention, overlap search bounds, explicit append overlap, test-only ordered raw frames, and optional test-only search direction filters.
- Outputs: largest vertical overlap length whose paired rows are within the average RGB match threshold, first top-to-bottom exact in-canvas vertical placement, a newly stitched `Image` preserving the source format and row stride, average RGB frame difference for same-geometry images, or test-only push state results for initialized, duplicate, accepted, and no-match frames with accepted-frame viewport-relative Top/Bottom match metadata and finish-time stitched image plus current viewport.
- Dependencies: `image::Image`, standard error handling, and `wayland-client` SHM format names.
- Forbidden dependencies: Wayland protocol flow, runtime environment checks, filesystem access, clipboard state, and CLI modules.
- Failure modes: unsupported formats, zero dimensions, short stride/data, width, stride, or format mismatch, invalid overlap ranges, no overlap within the RGB threshold, and arithmetic overflow.
- Verification method: unit tests for exact and thresholded overlap detection, largest-overlap preference, first-match exact canvas placement, mismatch rejection, append/prepend bytes, invalid overlap, `RawStitcher` initialization, near-duplicate handling, exact in-canvas and viewport-relative Top/Bottom thresholded overlap placement with viewport/match metadata, alpha-ignored overlap matching, direction-filtered no-match preservation, empty finish, plus `cargo check`.
- Replacement contract: keep stitching deterministic, side-effect-free, and test-only until longshot orchestration exists; rebuild placement first checks near-duplicates against the previous frame by average RGB difference for same geometry, then checks exact vertical in-canvas placement at x=0, then falls back to average-RGB-thresholded vertical Top/Bottom append/prepend relative to the current viewport with explicit `Vertical`/`Down`/`Up` direction filtering and does not include the original multi-direction, multi-stage, or perceptual matcher.

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
- Inputs: Wayland globals and requested output/global regions.
- Outputs: captured output frames and region metadata.
- Dependencies: `wayland-client`, `wayland-protocols-wlr`, `tempfile`, and `libc`.
- Forbidden dependencies: non-wlr capture APIs and compositor adaptation branches.
- Failure modes: missing globals, unsupported buffer format, failed SHM allocation, screencopy failure, and protocol cancellation.
- Verification method: manual niri capture plus `cargo check`.
- Replacement contract: provide frozen output frames suitable for selection-time compositing.

## `wayland::selection`

- Responsibilities: bind layer-shell/xdg-output, display frozen output frames with dimmed outside-selection mask and visible border, redraw dirty overlay regions during drag, and return a drag-selected viewport.
- Non-responsibilities: screencopy, PNG writing, clipboard, and long-mode interaction.
- Inputs: frozen captured output frames, Wayland pointer/keyboard events, and output logical geometry.
- Outputs: selected viewport or cancellation.
- Dependencies: `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr`, `tempfile`, and `libc`.
- Forbidden dependencies: focus-grab extensions and non-niri compositor branches.
- Failure modes: missing layer-shell/compositor/shm/seat/output globals, missing frozen output frame, unsupported frozen frame format, no pointer, cancellation, or empty selection.
- Verification method: unit tests for overlay pixels/dirty regions, manual niri drag-selection test, and `cargo check`.
- Replacement contract: return geometry-only selection data usable by screencopy compositing.
