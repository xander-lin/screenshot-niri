# AGENTS.md — screenshot-rs-rebuild

## Project Identity And Purpose

- Rust rebuild of a **Wayland screenshot tool**, originally targeting niri; binary name `screenshot`.
- Upstream: `xander-lin/screenshot-niri`. The historical original-project path `/home/life/Work/screenshot-rs` is unavailable in this workspace.
- Launch is compositor-agnostic: do not gate on Niri session hints. Users test compatibility with their compositor's advertised Wayland protocols.
- Serves interactive screen capture on Wayland without external tools (no grim, no wl-copy, no shell-outs).

## Functional Goals And Scope

- **Normal mode** (default): freeze all outputs (with-cursor and cursor-free frame pair), dimmed drag-region selection overlay, save PNG, copy to clipboard. `1`–`9` full-screen capture, `H` toggles cursor in output, `L` promotes selection to long mode, `Esc` cancels.
- **Long/scroll mode** (`--long`): select a region on a single output, scroll content, continuous capture (configurable fps cap via `--fps`, default 30) with live stitched preview, direction hints (`Up`/`Down`/`Left`/`Right`), selection-box edit mode (`E`) with per-edge snap targets and wheel blocking for T/L-shaped captures and static-footer completion, finish on `Enter`/`Space`, stitch into one PNG (+ transparent corners for ragged shapes).
- **Clipboard**: `wlr-data-control` serving `image/png` (default) or `file://` URI (`--url`), as a detached provider process.
- **Output path policy**: temp dir default → `--file [PATH]` (Pictures/Screenshots) → `-o/--output PATH` → `--name NAME` → positional PATH (same as `--file PATH`).
- **Config file** (`~/.config/screenshot/config`): `max_fps` (default frame-rate cap when `--fps` absent) and `border_width` (visible selection-border thickness); optional, non-fatal, dependency-free parsing.
- Long-mode selections spanning outputs show a **red warning border**; releasing clamps to the click-anchor output (largest piece fallback) and enters edit mode.
- Edit mode supports edge resize and whole-box move (interior drag); captures are inset 1 logical pixel inside the box so the overlay border is never captured.
- Out of scope: X11, focus-grab extensions, diagnostics compatibility bins. Cross-compositor compatibility is user-tested.

## Product Philosophy

- Pure Wayland-protocol clients; zero external command dependencies at runtime.
- Minimal dependency footprint (`libc`, `png`, `tempfile`, `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr`).
- Stitching logic is deterministic, side-effect-free pure image code, heavily unit-tested; interactive/protocol code is kept separate from it.
- Frozen-screen selection UX: the screen freezes and dims during selection so the capture matches what the user saw.

## Project Principles

- Respect each module's boundary (see Module Contracts below) when changing interfaces.
- `found.md` holds durable project memory (module relationships, architecture decisions) and lessons learned; code is authoritative over docs — verify against code before relying on doc claims.
- Keep docs (`found.md`, `README.md`, `AGENTS.md`) in sync in the same change as behavior milestones (see the documentation-drift lesson in `found.md`).
- Verification: `cargo test` (unit tests live alongside modules; stitch has extensive pure-logic tests), `cargo check`, manual testing of Wayland flows under a live compositor.
- Long-capture performance protections must be preserved: configurable frame-rate cap (`--fps`, hardcoded default 30, range 1–240), damage-based capture after the first frame, near-duplicate frame skip (bypassed when the capture region changed), bounded capture channel backpressure, and capture pause during edit-mode edge drags.
- Selection-box edit mode (E key) semantics must be preserved: edges have snap targets only in edit mode; edit mode blocks wheel scroll by holding pointer input; frames captured after a resize are stitched at their deterministic known position; only the settled rectangle after an edge-drag release is captured.
- Overlay rendering maps through the output's real logical rect and actual overlay buffer dimensions (`selected_buffer_rect`), not `wl_output.scale`: overlay surfaces currently use buffer scale 1. Long screencopy requests use output-local logical coordinates; frozen-image crop rectangles use pixels.
- Capture exclusion lasts until frame completion, including after edit release. Border visibility is continuous during edits: any pending frame exposed to changed geometry is permanently invalidated and discarded before dedup/stitching, even if the box returns to its old position. PrepareCapture commits on the selected output and synchronizes the UI connection before acknowledging the worker.
- Live overlays share one renderer: cached dim mask/hole → stitched preview → selection border. The border is drawn last on every redraw and follows the current box, not the exclusion union. Frozen and live paths use the same closed outside-ring pixel rule. Frozen dirty-region damage includes the full configured border width.
- Memory: overlay buffers are full-resolution RGBA per output (3 each — one current on screen, releases arrive bursty behind commits, the third keeps a free canvas through bursts) plus a base-pixels cache; footprint is resolution-driven, not fps-driven.
- Overlay presentation follows the commit-on-change contract: a buffer already showing the current rect and preview seq is never re-committed, so steady-state long capture issues zero commits and capture throughput never depends on compositor release timing.
- Long capture is single-screen by construction: at the frozen→live transition the render scope narrows to the selected output (`UiState::render_scope`), and overlay rendering, the pending-render debt, and its clearing count in-scope overlays only. The other outputs' overlays stay static frozen images. Never widen the debt condition back to all overlays — an all-overlays clearing condition is unsatisfiable when each output's canvas turns over independently.
- Prepare must never abandon capture frames: transient all-canvases-held states right after a drag burst are normal (releases are in flight); the per-frame recovery path must always flush and read the socket so releases get processed. `dispatch_poll`'s early return (`dispatch_pending > 0`) deliberately skips flush/read and must not be the only path a recovery depends on.

## Structure Map

- `src/main.rs` → bin-only modules (`app`, `cli`, `clipboard`) + lib modules.
- `src/lib.rs` → exposes `geometry`, `image`, `stitch`, `trace`, `wayland::{screencopy, selection}` for tests.
- `src/app.rs` → orchestration; normal flow and long flow; `LongCaptureWorker` (dedicated capture thread, configurable 30 fps default cap, prepare-capture-clean ack handshake).
- `src/stitch.rs` + `src/stitch/` → full multi-stage matcher ported from the original (canvas exact placement, perceptual bands, fuzzy hash, fast-motion scan, fixed bands, previous-frame fallback).
- `src/wayland/screencopy.rs` → wlr screencopy v3 client; one-shot frozen captures and persistent `CaptureSession` for long mode.
- `src/wayland/selection.rs` → layer-shell overlay: frozen/dimmed selection UI, drag redraw, long-mode preview and status, pointer passthrough for capture.
- `docs/hyprland-findings.md` → Hyprland 兼容性问题调研（三个问题的触发条件、上游源码证据、版本时间线、可做的规避、待实测清单）。
- `src/geometry.rs`, `src/image.rs`, `src/cli.rs`, `src/clipboard.rs` → see Module Contracts below.

## Module Contracts

Concise boundaries for each module: what it owns, what it must not do, and how it is verified.

- **`config`** — Owns optional user settings (`max_fps`, `border_width`) from `$XDG_CONFIG_HOME/screenshot/config`; parses `key = value` lines with `#` comments and clamps ranges. Must not parse CLI args or touch Wayland. Malformed/unreadable files degrade to defaults (never fatal). Verified by unit tests for parsing and range clamping.
- **`cli`** — Owns flag parsing, invalid-combination rejection, output-path resolution, and help/version text. Must not touch Wayland, write images, or serve the clipboard. Failure: unknown flags, missing values, unsafe temp dir, invalid `--name`, out-of-range `--fps`, mutually exclusive output modes. Verified by parsing/path unit tests.
- **`app`** — Orchestrates freeze → select → composite → write → clipboard, and long capture (fps-capped worker, PrepareCapture handshake on the exact clean-render rectangle, offset-aware stitching). Launches without compositor-name gating. Must not implement protocol dispatch or PNG encoding. Screen-space capture regions use output-local logical coordinates; stitch offsets use returned-frame pixels. Discard edit-invalidated frames before dedup/stitching. Verified by integration/manual test under a live Wayland compositor and `cargo check`.
- **`geometry`** — Converts logical selected rectangles into per-output capture regions; clamps spanning selections to the drag-start output (`clamp_rect_to_output_at_point`, largest-piece fallback). No pointer input, registry handling, or pixel copying. Verified by intersection/scaling/mixed-output unit tests.
- **`image`** — Represents SHM images, converts XRGB/ARGB8888 to RGBA PNG, and blits regions. No Wayland capture, path policy, or clipboard ownership. Verified by blit/format unit tests.
- **`stitch`** — Deterministic multi-stage matcher plus offset-tagged geometry placement: changed offset/size places a frame at its known position, growing ragged T/L canvases with per-row coverage (uncovered extents transparent). Must not handle CLI, Wayland, scrolling, output selection, or PNG writing. Preserve covered pixels (not the bounding rectangle) so later widening fills recaptured strips; never-captured strips stay transparent. Verified by the extensive stitch unit tests.
- **`clipboard`** — Serves `image/png` or file URI via `wlr-data-control`. No region selection, capture, or path decisions; no external clipboard commands. Verified by manual paste tests under a live Wayland compositor.
- **`wayland::screencopy`** — Binds wlroots screencopy and captures outputs into SHM buffers (one-shot frozen + persistent long-capture session). Requests use output-local logical regions; persistent capture returns `None` on cancellation. No selection UI, PNG writing, or stitching. Damage waits poll a stop flag with bounded latency so joining the worker never requires fresh damage. Verified by manual capture plus `cargo check`.
- **`wayland::selection`** — Layer-shell overlay: dimmed frozen frames, visible border, drag/edit interactions, long-mode preview/status, and live selection/output accessors. Caches only dim/hole; composites preview then border every time. Frozen and live borders share one closed outside-ring rule (corners, configured width); frozen damage includes the ring. `prepare_capture_clean` commits on the selected output and roundtrips before acking; `complete_capture` rejects any frame exposed to a geometry change (sticky), which `app` must discard. No focus-grab extensions or compositor-name checks. Verified by overlay-pixel/dirty-region/edge unit tests plus manual drag-selection and edit-mode tests.

## Commands

- Build: `cargo build --release` (binary `target/release/screenshot`; Rust 1.80+).
- Test: `cargo test`.
- Trace logs: `--features trace-logs` (see `src/trace.rs` env gates).
- Packaging: `PKGBUILD` (package `screenshot-niri`, conflicts with `screenshot-plain`).
