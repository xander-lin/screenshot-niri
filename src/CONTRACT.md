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

- Responsibilities: orchestrate normal screenshot flow, run niri-only preflight for normal capture, and reject unsupported long screenshots.
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

- Responsibilities: bind layer-shell/xdg-output, display dimmed overlay surfaces with transparent selection and visible border, redraw dirty overlay regions during drag, and return a drag-selected viewport.
- Non-responsibilities: screencopy, PNG writing, clipboard, and long-mode interaction.
- Inputs: Wayland pointer/keyboard events and output logical geometry.
- Outputs: selected viewport or cancellation.
- Dependencies: `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr`, `tempfile`, and `libc`.
- Forbidden dependencies: focus-grab extensions and non-niri compositor branches.
- Failure modes: missing layer-shell/compositor/shm/seat/output globals, no pointer, cancellation, or empty selection.
- Verification method: unit tests for overlay pixels/dirty regions, manual niri drag-selection test, and `cargo check`.
- Replacement contract: return geometry-only selection data usable by screencopy compositing.
