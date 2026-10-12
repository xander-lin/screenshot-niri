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

- Each module has a contract in `src/CONTRACT.md` (responsibilities, non-responsibilities, inputs/outputs, failure modes, verification). Respect and update it when changing module boundaries.
- `FINDINGS.md` is the durable project memory; code is authoritative over docs — verify against code before relying on doc claims.
- Keep docs (`CONTRACT.md`, `FINDINGS.md`, `README.md`) in sync in the same change as behavior milestones (see `found.md` lesson on documentation drift).
- Verification: `cargo test` (unit tests live alongside modules; stitch has extensive pure-logic tests), `cargo check`, manual niri testing for Wayland flows.
- Long-capture performance protections must be preserved: configurable frame-rate cap (`--fps`, hardcoded default 30, range 1–240), damage-based capture after the first frame, near-duplicate frame skip (bypassed when the capture region changed), bounded capture channel backpressure, and capture pause during edit-mode edge drags.
- Selection-box edit mode (E key) semantics must be preserved: edges have snap targets only in edit mode; edit mode blocks wheel scroll by holding pointer input; frames captured after a resize are stitched at their deterministic known position; only the settled rectangle after an edge-drag release is captured.
- Overlay rendering maps through the output's real logical rect and actual overlay buffer dimensions (`selected_buffer_rect`), not `wl_output.scale`: overlay surfaces currently use buffer scale 1. Long screencopy requests use output-local logical coordinates; frozen-image crop rectangles use pixels.
- Capture exclusion lasts until frame completion, including after edit release. Border visibility is continuous during edits: any pending frame exposed to changed geometry is permanently invalidated and discarded before dedup/stitching, even if the box returns to its old position. PrepareCapture commits on the selected output and synchronizes the UI connection before acknowledging the worker.
- Live overlays share one renderer: cached dim mask/hole → stitched preview → selection border. The border is drawn last on every redraw and follows the current box, not the exclusion union. Frozen and live paths use the same closed outside-ring pixel rule. Frozen dirty-region damage includes the full configured border width.
- Memory: overlay buffers are full-resolution RGBA per output (2 each) plus a base-pixels cache; footprint is resolution-driven, not fps-driven. Keep `OVERLAY_BUFFER_COUNT` minimal.

## Structure Map

- `src/main.rs` → bin-only modules (`app`, `cli`, `clipboard`) + lib modules.
- `src/lib.rs` → exposes `geometry`, `image`, `stitch`, `trace`, `wayland::{screencopy, selection}` for tests.
- `src/app.rs` → orchestration; normal flow and long flow; `LongCaptureWorker` (dedicated capture thread, configurable 30 fps default cap, prepare-capture-clean ack handshake).
- `src/stitch.rs` + `src/stitch/` → full multi-stage matcher ported from the original (canvas exact placement, perceptual bands, fuzzy hash, fast-motion scan, fixed bands, previous-frame fallback).
- `src/wayland/screencopy.rs` → wlr screencopy v3 client; one-shot frozen captures and persistent `CaptureSession` for long mode.
- `src/wayland/selection.rs` → layer-shell overlay: frozen/dimmed selection UI, drag redraw, long-mode preview and status, pointer passthrough for capture.
- `src/geometry.rs`, `src/image.rs`, `src/cli.rs`, `src/clipboard.rs` → see `src/CONTRACT.md`.

## Commands

- Build: `cargo build --release` (binary `target/release/screenshot`; Rust 1.80+).
- Test: `cargo test`.
- Trace logs: `--features trace-logs` (see `src/trace.rs` env gates).
- Packaging: `PKGBUILD` (package `screenshot-niri`, conflicts with `screenshot-plain`).
