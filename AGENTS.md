# AGENTS.md — screenshot-plain

## Project Identity And Purpose

- Rust Wayland screenshot tool, binary `screenshot`; originally developed for niri.
- This branch is the **plain** variant: region and full-screen capture only. It is
  derived from the full tool by deleting long/scroll capture, the stitcher, the
  capture corpus and its analysis tools.
- The full variant (long capture with stitching, `--long`, `--fps`, corpus
  verification) lives on the `niri` branch. Do not reintroduce long-capture
  machinery here; a change that needs it belongs on `niri`.

## Functional Goals And Scope

- **Selection**: freeze every output (with-cursor and cursor-free pair), dim the
  screen, drag a rectangle, save a PNG and copy it to the clipboard.
- **Full screen**: `1`–`9` capture one output (ordered left to right by
  `(x, y)`); `0` captures every output as one image (the bounding box of the
  outputs, with each output's piece laid out end to end — a physical gap between
  outputs is closed, not painted black).
- **Cursor**: `H` toggles the cursor in the captured frame; hidden by default.
  It can be pressed while the drag button is held (the selection completes on
  release).
- **Cancel**: `Esc` at any time.
- **Output path policy**: temp dir default → `--file [PATH]`
  (Pictures/Screenshots) → `-o/--output PATH` → `--name NAME` → positional PATH
  (same as `--file PATH`).
- **Clipboard**: `wlr-data-control` serving `image/png` (default) or a
  `file://` URI (`--url`), as a detached provider process.
- **Config** (`~/.config/screenshot/config`): `border_width` (1–16, default 3).
  Parsing is optional and non-fatal; malformed files fall back to defaults.
- **Out of scope**: long/scroll capture, stitching, corpus tools, X11,
  compositor-name gates, focus-grab extensions, diagnostics compatibility bins.

## Product Philosophy

- Pure Wayland-protocol client; no external command dependencies at runtime
  (no grim, no wl-copy, no shell-outs).
- Minimal dependency footprint (`libc`, `png`, `tempfile`, `wayland-client`,
  `wayland-protocols`, `wayland-protocols-wlr`).
- The screen freezes and dims during selection, so the capture matches what the
  user saw.
- Memory stays proportional to the screen, not to the number of captures: one
  without-cursor frame per output plus a cursor-sized patch, and two overlay
  buffers per output.

## Project Principles

- Respect each module's boundary (see Module Contracts).
- `found.md` holds durable project memory and verified lessons; code is
  authoritative over docs — verify against code before relying on a doc claim.
- Keep docs (`README.md`, `AGENTS.md`, `found.md`) in sync in the same change as
  behaviour milestones. A doc claim the code never implemented is a defect.
- Verification: `cargo test` and `cargo clippy --all-targets` must stay clean
  (current: 48 tests, no clippy warnings). Interactive Wayland flows require a
  manual pass under a live compositor; ask the user to run it.
- The plain branch never gains a long-capture feature by accident: deleting
  code must leave the normal path compiling, tested and documented.

## Structure Map

- `src/main.rs` → binary entry; prints `screenshot: <error>` and exits 1 on error.
- `src/lib.rs` → exposes `app`, `cli`, `clipboard`, `config`, `geometry`,
  `image`, `wayland::{screencopy, selection}` for tests; `trace` is macro-only.
- `src/app.rs` → orchestration: capture the frozen pair → selection overlay →
  composite per-output pieces → PNG → detached clipboard provider.
- `src/cli.rs` → flag parsing, output-path resolution, help/version text.
- `src/config.rs` → `border_width` from the user config file.
- `src/geometry.rs` → `LogicalRect`, `OutputInfo`, `SelectedViewport`: splits a
  logical selection into per-output pieces and derives pixel crop rectangles
  and destination offsets from the captures themselves (N-way, never two-way).
- `src/image.rs` → SHM image representation, XRGB/ARGB to RGBA, region blit,
  PNG writing.
- `src/clipboard.rs` → detached `wlr-data-control` provider (image or file URI).
- `src/wayland/screencopy.rs` → one-shot wlr-screencopy v3 capture of every
  output with and without the cursor overlay.
- `src/wayland/selection.rs` → layer-shell overlay: frozen frames, dim mask,
  closed border ring, drag geometry, full-screen keys, `H`, `Esc`; fractional
  scale and overlay buffer sizing.
- `docs/overlay-fractional-scale.md` → why overlay buffers are sized from the
  capture and the fractional-scale hint, with the hardware checklist.

## Module Contracts

- **`config`** — Owns the optional `border_width` setting; parses `key = value`
  lines with `#` comments and clamps the range. Must not parse CLI args or touch
  Wayland. Malformed/unreadable files degrade to defaults (never fatal).
  Verified by unit tests for parsing and range clamping.
- **`cli`** — Owns flag parsing, invalid-combination rejection, output-path
  resolution, and help/version text. Must not touch Wayland, write images, or
  serve the clipboard. Failure: unknown flags, missing values, unsafe temp dir,
  invalid `--name`, `--output` with `--file`. Verified by parsing/path unit
  tests.
- **`app`** — Orchestrates freeze → select → composite → write → clipboard.
  Launches without compositor-name gating. Must not implement protocol dispatch
  or PNG encoding. Verified by `cargo check` plus a manual capture pass.
- **`geometry`** — Converts a logical selection into per-output crop rectangles
  and destination offsets. Pixel ratios come from each capture
  (`image.width / output logical width`), never from `wl_output.scale`, which is
  an integer hint: on a 1.5 output reporting 2 it asks for pixels the capture
  does not have. Edges, not widths, are mapped (`floor` of both edges) so a
  fractional seam stays one boundary column; destination offsets equal the
  widths already written. Covers 1, 3 and 4 outputs, stacked/negative layouts
  and mixed scale; a gap between outputs is closed. No pointer input, registry
  handling, or pixel copying.
- **`image`** — Represents SHM images, converts XRGB/ARGB8888 to RGBA PNG, and
  blits regions. Readers bound themselves by `stride * height`, never by
  `data.len()`. No Wayland capture, path policy, or clipboard ownership.
- **`clipboard`** — Serves `image/png` or a file URI via `wlr-data-control` from
  a detached child process; the parent waits for a `ready` line before returning.
  No region selection, capture, or path decisions; no external clipboard
  commands. Verified by manual paste tests under a live compositor.
- **`wayland::screencopy`** — Binds wlr-screencopy v3 and captures each output
  into an SHM buffer (one connection per output, one capture each). Accepts only
  XRGB/ARGB8888; flips rows when `YInvert` is set; reuses no buffer across
  captures. Refuses zero-sized or >2 GiB buffers. No selection UI, PNG writing,
  or compositing.
- **`wayland::selection`** — Layer-shell overlay: dimmed frozen frames, a closed
  outside border ring (same rule for all widths, corners included), drag/edit-free
  geometry (a drag ends the selection; `E` is not part of this variant),
  full-screen digit keys, `0`, `H`, `Esc`. The frozen frame is the **without-cursor** capture plus a
  `CursorDelta` patch of every pixel where the with-cursor capture differs, so
  `H` costs no rebuild and the second full frame is not stored. Overlay buffers
  are `max(capture pixels, ceil(logical × preferred_scale / 120))` with
  `buffer_scale` 1 and a `wp_viewport` destination equal to the logical size;
  without `wp_viewporter` the buffer stays logical. Frozen dirty bands are
  outset by one buffer pixel when the buffer is denser than the logical grid
  (fractional scaling); without it one ring line survives at the box edge after
  a move. `0` and spanning selections are ordinary captures (no warning colour).
  No long mode, no edit mode, no preview, no capture-clean handshake.

## Commands

- Build: `cargo build --release` (binary `target/release/screenshot`; Rust 1.88+,
  enforced by `rust-version`).
- Test: `cargo test` (48 tests, one target: `main.rs` links the library instead
  of re-declaring the modules, so the `#[cfg(test)]` blocks run once;
  `[profile.test] opt-level = 2` keeps the pixel tests fast).
- Lint: `cargo clippy --all-targets` (currently zero warnings).
- Trace logs: `--features trace-logs` (see `src/trace.rs`).
- Packaging: `PKGBUILD` (package `screenshot-plain`, conflicts with
  `screenshot-niri`).
- Release binaries: x86_64 v3 with a generic fallback; the AUR PKGBUILD
  lists both sources and extracts/installs the one the local CPU can run
  (`avx avx2 bmi1 bmi2 f16c fma movbe` plus `lzcnt` or AMD's `abm`), so one
  binary is installed. aarch64 uses the architecture default.
