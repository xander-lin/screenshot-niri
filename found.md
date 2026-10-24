# found.md — Project Memory & Lessons Learned

Durable, verified project memory plus actionable lessons. Code is authoritative over docs.

Lesson entry format: **Context**, **Root cause**, **Correction**, **Prevention**. Record only verified incidents; never fabricate.

---

## Project Context

`screenshot-plain` is the plain variant of a Wayland screenshot tool (binary
`screenshot`), originally developed against niri's wlroots protocols. It performs
region and full-screen capture only; the `niri` branch carries long/scroll
capture, the stitcher, and the corpus verification tooling.

- **No compositor gate**: the launcher attempts the Wayland protocols and leaves
  compositor compatibility to user testing. Out of scope: X11, focus-grab
  extensions, diagnostics bins.
- **Modules**: `cli` parses args and resolves output paths; `app` orchestrates
  capture → selection → composite → write → clipboard; `wayland::screencopy`
  freezes every output with and without the cursor; `wayland::selection` renders
  the layer-shell overlay and collects geometry; `geometry` maps a logical
  selection to per-output pixel pieces; `image` composites and writes PNG;
  `clipboard` serves `image/png` or a file URI via `wlr-data-control`.
- **Config** (`~/.config/screenshot/config`): `border_width` (1–16, default 3).
- **Memory model**: per output, one without-cursor frame plus a cursor-sized
  patch, and two full-resolution RGBA overlay buffers. Footprint is
  resolution-driven.
- **Clipboard provider**: the lingering post-screenshot `screenshot` process is
  the detached `--clipboard-provider`; Wayland requires the data source to stay
  alive to serve pastes. Expected, not a leak.

---

## Lessons Learned

### 1. Documentation drift: `0` was documented as painting gaps black

- **Context**: `AGENTS.md` said `0` captures the union of all outputs and that a
  gap between outputs is opaque black. The code has always laid each output's
  piece out end to end (`dst` offset equals the widths already written).
- **Root cause**: A design note became a documented guarantee without being
  implemented or verified against `selected_axis_offset`.
- **Correction**: Documented the real behaviour (gaps are closed) and pinned it
  with `a_gapped_layout_packs_pieces_without_painting_the_gap`.
- **Prevention**: Trace every geometry claim through `capture_regions` and
  `capture_size`; prefer a unit test over prose for behaviour that is easy to
  assume.

### 2. Dead code hidden behind `#[allow(dead_code)]`

- **Context**: `screencopy::capture_region` had no caller in any target and was
  kept alive by an allow attribute.
- **Root cause**: The one-shot region capture was replaced by the persistent
  session, and the function was not removed.
- **Correction**: Deleted it (39 lines) during the review that produced this
  branch.
- **Prevention**: Grep the whole crate (including `src/bin`) before keeping an
  `allow(dead_code)`; an allow is for test-only or feature-gated API, not for
  leftovers.

### 3. `wl_output.scale` is an integer hint, not a logical→pixel ratio

- **Context**: The overlay was soft on a 1.5-scaled output, and full-screen
  capture of that output produced no file.
- **Root cause**: The crop used `output.scale` as the ratio: on a 1.5 output
  reporting 2 it asked for 2560x1440 from a 1920x1080 capture, and the blit
  refused. The overlay buffer was logical-sized and upscaled by the compositor.
- **Correction**: Derive the ratio from the capture itself
  (`image.width / output logical width`), map **edges** (not widths) so a
  fractional seam stays one boundary column, and size the overlay buffer from
  the capture plus `wp_fractional_scale_v1.preferred_scale` (120ths), with
  `buffer_scale` 1 and a `wp_viewport` destination.
- **Prevention**: Never convert logical geometry with `wl_output.scale`. Test
  mixed scale, stacked and negative-origin layouts; verify seams column by
  column.

### 4. Two roundings of the same box left a line of the old selection

- **Context**: After dragging a box on a fractionally scaled output, one ring
  line of the previous position survived at the box edge.
- **Root cause**: Dirty bands were derived from the box mapped into buffer pixels
  with floored edges, while the renderer classified a buffer pixel by the logical
  pixel it covers. The two disagree by one buffer pixel at the edge.
- **Correction**: Outset frozen damage bands by one buffer pixel whenever the
  buffer is denser than the logical grid (`damage_outset`); at an identity scale
  the mappings cannot disagree.
- **Prevention**: Test partial redraws pixel for pixel against a full redraw when
  the buffer and logical grids differ.

### 5. The cursor pair cost a second full frame

- **Context**: Normal mode captured every output twice (with and without the
  cursor) and stored both whole: 16.6 MB at 1080p for the cursor.
- **Root cause**: The session treated the two captures as independent frames
  even though they differ only where the compositor drew the cursor (measured
  0.07–0.13% of the pixels).
- **Correction**: Store the without-cursor capture once plus a `CursorDelta`
  patch of **every** differing pixel (so a blinked caret is reproduced exactly),
  and let `H` choose whether to apply it. The session's resident footprint fell
  from 124.7 MB to 51.8 MB on a two-output 1920x1080 rig.
- **Prevention**: When two variants of the same frame are captured together,
  store the difference, not the frame twice; measure the actual differing pixel
  count before assuming a shape.

### 6. Packaging from the project root treated `src/` as build output

- **Context**: A `makepkg --cleanbuild` was once run from the repository root,
  and the tool removed the source directory.
- **Root cause**: The working directory was not matched to the operation.
- **Correction**: Restored `src/` from a pre-build archive and verified against
  staged content; package only in an isolated directory with the archive as the
  source.
- **Prevention**: Never run packaging commands in the project root. Inspect the
  PKGBUILD and the reported version before installing.

### 7. "The tests passed" must mean a `test result: ok` line

- **Context**: A verification loop treated the absence of visible failures as
  success even when compilation had failed.
- **Root cause**: Judging success by the wrong signal (or by a truncated tail).
- **Correction**: Check the `test result:` line explicitly and separate compile
  errors from test failures.
- **Prevention**: Verification commands must surface the result line, and a
  non-zero exit is never "probably fine".

### 8. Removing a module's support code can silently leave the interface

- **Context**: This branch was produced by deleting long capture. The first
  scripted deletion removed whole ranges between anchors and took the frozen
  renderer with it.
- **Root cause**: Range deletion between two unrelated markers, without checking
  which other items lived inside the range.
- **Correction**: Restored from `HEAD` and switched to item-by-item deletion
  (brace matching from each named item), compiling after each phase.
- **Prevention**: Never delete by line range when functions are interleaved;
  delete by named item, and let the compiler prove the survivors are complete.

---

## Verification Memory

- **Plain branch verification (2026-10-24, v0.2.0)**: derived from `niri`
  `e5bc1ba`; `cargo test --release` → 48 passed; `cargo clippy --release
  --all-targets` → zero warnings; `cargo build --release` produces
  `target/release/screenshot`. Source size 3,987 lines across the retained
  modules (the `niri` branch's stitcher and tools removed). Release `v0.2.0-plain`
  ships x86_64 and aarch64 tarballs under
  `screenshot-niri-0.2.0/` (binary, README, LICENSE).
- **Packaging**: `PKGBUILD` builds `screenshot-plain` and conflicts with
  `screenshot-niri`. It was **not installed** on this machine: the installed
  `screenshot-niri-local 0.1.0-48` owns `/usr/bin/screenshot`, and replacing it
  would remove the full variant. Install only when the user asks for it.
- **Interactive flows**: drag selection, `1`–`9`, `0`, `H`, `Esc`, spanning
  selections and fractional-scale crispness need a live compositor pass; ask
  the user to run them.
