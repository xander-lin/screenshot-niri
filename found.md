# found.md — Project Memory & Lessons Learned

Durable, verified project memory plus actionable lessons. Code is authoritative over docs.

Lesson entry format: **Context**, **Root cause**, **Correction**, **Prevention**. Record only verified incidents; never fabricate.

---

## Project Context

`screenshot-rs-rebuild` is a Wayland screenshot tool (binary `screenshot`), originally developed against niri's wlroots protocols. Normal region capture and long/scroll capture are implemented end-to-end.

- **No compositor gate**: the launcher no longer checks `NIRI_SOCKET` or `XDG_CURRENT_DESKTOP`. It attempts the Wayland protocols and leaves compositor compatibility to user testing. Out of scope: X11, focus-grab extensions, diagnostics bins.
- **Modules**: `cli` parses args and resolves output paths. `app` orchestrates capture → selection → composite → write → clipboard (no preflight). `wayland::screencopy` freezes outputs and drives the persistent long-capture session. `wayland::selection` renders the layer-shell overlay and collects geometry. `geometry` maps logical selections to per-output regions. `image` composites and writes PNG. `stitch` is the multi-stage matcher (canvas placement, perceptual bands, fuzzy hash, fast-motion scan, fixed bands, previous-frame fallback), driven by `app` from the long-capture worker. `clipboard` serves `image/png` or file URI via `wlr-data-control`.
- **Config** (`~/.config/screenshot/config`): `max_fps` (long-capture cap, default 30, range 1–240) and `border_width` (1–16). The cap is a ceiling over damage-driven pull capture.
- **Long capture**: `--long` selects one output, captures continuously, and stitches on Enter/Space. `E` toggles edit mode (per-edge resize, interior move) which blocks wheel scroll; region-changed frames are placed at the deterministic offset-implied position, growing ragged T/L canvases whose uncovered extents stay transparent. Capture pauses during edge drags.
- **Memory model**: per output, 2 full-resolution RGBA overlay buffers plus a base/dim cache; footprint is resolution-driven, not fps-driven. Per-frame capture SHM is munmapped on drop.
- **Clipboard provider**: the lingering post-screenshot `screenshot` process is the detached `--clipboard-provider`; Wayland requires the data source to stay alive to serve pastes. Expected, not a leak.

---

## Lessons Learned

### 1. Documentation drift: FINDINGS.md/CONTRACT.md claimed long mode was unsupported while code implemented it

- **Context**: Docs still described long screenshots as "out of scope"/"test-only" while `src/app.rs`, `src/cli.rs`, and `src/stitch/` implemented them (commits `b289606`..`a8eb251`).
- **Root cause**: Docs were not updated in the same changes that landed the work.
- **Correction**: Updated the stale scope/relationship claims and `src/CONTRACT.md` entries.
- **Prevention**: Treat code as source of truth; verify with `grep`/`read` before citing docs. Update `CONTRACT.md` and this file in the same commit as behavior changes.

### 2. Profiling artifacts were committed to the repository

- **Context**: `perf.data`/`perf.data.old` were committed during performance work before a `.gitignore` rule existed.
- **Prevention**: Write profiling/temp artifacts outside the repo (e.g. `/tmp/opencode`) or add ignore rules first; review `git status` before committing.

### 3. Removed an "unused" pub method that lib tests still used

- **Context**: `RawStitcher::push_frame_views` looked dead from bin-target warnings but was called by two lib tests.
- **Root cause**: Judging API liveness from bin-target warnings without checking `#[cfg(test)]` users.
- **Correction**: Restored it behind `#[cfg_attr(not(test), allow(dead_code))]`.
- **Prevention**: Before removing anything flagged dead, grep the whole crate including test modules; prefer the cfg-attr allow for test-only public API.

### 4. Path-wide `git checkout -- src/` destroyed uncommitted work

- **Context**: Mutation testing reverted with `git checkout -- src/...` while new tests/features were uncommitted; the revert wiped them twice.
- **Root cause**: Mutation experiments shared the tree with uncommitted work; the revert path was wider than the mutation and no `git status` preceded it.
- **Prevention**: Commit **before** every mutation experiment. Revert only after `git status --short` confirms the tree is clean apart from the mutation; never chain the revert with `;`.

### 5. Capture region and overlay geometry raced across threads (white borders)

- **Context**: Stitched captures could contain a spurious white box at a stitch seam. The worker read its region before `PrepareCapture` while the UI thread rendered from whatever rectangle it had at dispatch; edit drags changed it in between.
- **Root cause**: One fact ("what region is captured now") was owned by two threads with no same-source handshake; `prepare_capture_clean` ignored whether its render committed.
- **Correction**: The ack carries the region derived from the same rectangle as the clean render; the worker never trusts a local snapshot.
- **Prevention**: Derive both sides from one decision point via the handshake. Verify asynchronous clean-up actually happened. Pixel-scan visual artifacts before hypothesizing.

### 6. Capture exclusion ended before the asynchronous frame completed

- **Context**: The earlier fix cleared its exclusion on mouse release and accepted any output's redraw as capture-ready; it also reread mutable selection for the ack.
- **Root cause**: A commit/flush was treated as capture completion; damage waits have no fixed deadline, and monitor identity was missing.
- **Correction**: Retain the exact prepared rectangle until the worker delivers its frame; commit on the selected output, roundtrip before acking, and return the saved rectangle. Damage waits poll cancellation so shutdown never hangs.
- **Prevention**: Treat flush, commit, and frame completion as separate events. Validate readiness for the actual target output; never infer timing guarantees from expected frame rates.

### 7. Output scale was used for both protocol geometry and overlay buffers

- **Context**: Long capture reused pixel-space crop rectangles as `capture_output_region` args, and overlay holes multiplied logical positions by `wl_output.scale` despite logical-sized buffers at surface scale 1.
- **Root cause**: A shared rectangle type obscured coordinate units.
- **Correction**: Long requests use output-local logical units; stitch offsets use returned-frame pixels; overlay holes map through actual buffer dimensions.
- **Prevention**: Trace units through allocation, surface scale, protocol request, returned buffer, preview, and stitching. Test logical-sized and scaled buffers on offset outputs and degenerate sizes; do not weaken existing assertions.

### 8. Packaging from the project root removed the source directory

- **Context**: `makepkg --cleanbuild` ran from the project root with an upstream-fetching PKGBUILD and treated `src/` as disposable build output.
- **Root cause**: Shell working directories were not matched to each operation.
- **Correction**: Restored `src/` from a pre-build archive and verified against staged content; built the intended archive in an isolated directory.
- **Prevention**: Package only in an isolated directory with an explicit working directory; inspect PKGBUILD and check the reported version before installing. Never `makepkg clean/cleanbuild` from a Rust project root containing `src/`.

### 9. Cached selection borders were overpainted by the stitched preview

- **Context**: The live base cache included the border and the preview was drawn over it; a separate input redraw path omitted the preview.
- **Root cause**: Final visual layers were mixed into a background cache, and multiple live render paths behaved differently.
- **Correction**: Cache only dim mask and hole; composite preview then border on every live redraw; reuse one live renderer.
- **Prevention**: Test final composed pixels, including all four border edges and cache reuse with changing color/thickness/visibility. Keep capture-exclusion tests on the same foreground compositor.

### 10. Border strips omitted corners and frozen/live borders disagreed

- **Context**: Live border strips stopped at side extents (blank corners), frozen selection used an unrelated inside-border rule, and edit visibility was suppressed to keep captures clean.
- **Root cause**: Multiple border definitions and hiding the UI instead of rejecting invalid asynchronous frames.
- **Correction**: One outside-ring predicate defines frozen and live borders (corners included); dirty damage includes the ring; edit borders follow the current rectangle continuously with sticky pending-frame invalidation on geometry change.
- **Prevention**: Compare frozen/live border masks pixel-for-pixel for widths 1–16; verify partial redraw equals full redraw after movement; test move-away-and-back invalidation.

### 11. Widening did not fill previously uncaptured canvas strips

- **Context**: Canvas writes excluded the whole old bounding rectangle (including transparent side strips), and new offset rows unioned coverage with the empty `(0,0)` sentinel, making missing strips opaque.
- **Root cause**: Canvas bounds were treated as captured coverage; empty coverage was treated as a real interval at x=0.
- **Correction**: Preserve actual covered row intervals and copy newly captured strips inside the old bounds; initialize empty rows from the frame extent; export empty rows transparent.
- **Prevention**: Run a textured wide → narrow → scroll → wide sequence asserting every pixel and alpha, then repeat resizing. Recaptured strips must contain pixels; uncaptured strips must stay transparent.

---

## Verification Memory

- **2026-10-12 final state**: 133 lib + 153 bin tests pass (two profiling tests ignored); `cargo check --locked` and `cargo build --release --locked` pass. Two pre-existing clippy errors (`absurd_extreme_comparisons`) and two unused-import warnings in `cli.rs` exist at the prior commit and were not introduced by recent work.
- Release-binary smoke checks with Niri hints unset and desktop set to `sway` reached the deliberately unavailable Wayland connection instead of a compositor-name rejection.
- Wayland dispatch/compositor behavior (input grabbing, wheel blocking, passthrough toggling, live capture) remains manually testable only under a live session; unit tests cover the pure logic and pixel rendering.
