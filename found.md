# found.md — Lessons Learned

Verified mistakes and actionable lessons only. Do not fabricate incidents; record new ones promptly.

## 1. Documentation drift: FINDINGS.md/CONTRACT.md claimed long mode was unsupported while the code implemented it

- **Mistake/context**: On 2026-10-08, `FINDINGS.md` (Project Scope, Module Relationships, Architecture Decisions) and `src/CONTRACT.md` (`app`, `stitch`) still described long screenshots as "out of scope", "test-only", and "`--long` remains unsupported", but `src/app.rs` (`run_long`, `LongCaptureWorker`), `src/cli.rs` (`--long`), and the full `src/stitch.rs` + `src/stitch/` port (commits `b289606`..`a8eb251`) implement long capture end-to-end.
- **Root cause**: Documentation was not updated in the same changes that landed the longshot work; the docs had drifted across several milestones.
- **Correction**: Updated the stale FINDINGS.md scope/relationship claims and the `src/CONTRACT.md` `cli`/`app`/`stitch`/`wayland::selection` entries (2026-10-08).
- **Prevention**: Treat code as the source of truth when answering questions or planning work — verify with `grep`/`read` before citing docs. When completing a behavior milestone, update `CONTRACT.md` and `FINDINGS.md` in the same commit.

## 2. Profiling artifacts were committed to the repository

- **Mistake/context**: `perf.data`/`perf.data.old` were committed during performance work and later removed from git by commit `650e4a3` ("Remove perf data, add to gitignore"). The files still exist on disk (now ignored).
- **Root cause**: Profiling output was generated inside the repo before a `.gitignore` rule existed.
- **Prevention**: Write profiling/temp artifacts outside the repo (e.g. `/tmp/opencode`) or ensure ignore rules exist first; review `git status` before committing.

## 3. Removed an "unused" pub method that lib tests still used

- **Mistake/context**: While wiring `push_frame_with_offset` (2026-10-08), `RawStitcher::push_frame_views` looked dead from the bin target's warnings, so it was deleted; `cargo test` then failed because two lib tests call it. The bin-target dead-code warning was misleading: the method was test-only API.
- **Root cause**: Judging API liveness from the bin target's warnings without checking `#[cfg(test)]` users.
- **Correction**: Restored the method behind `#[cfg_attr(not(test), allow(dead_code))]`.
- **Prevention**: Before removing anything flagged dead, grep the whole crate including test modules (`grep -rn "<name>" src/`); prefer `#[cfg_attr(not(test), allow(dead_code))]` for test-only public API over deletion.

## 4. Path-wide `git checkout -- src/` destroyed uncommitted test work during mutation testing

- **Mistake/context**: While mutation-testing the edit-mode feature (2026-10-08), mutations were reverted with `git checkout -- src/` while the new gap-closing tests were still uncommitted; the revert wiped an hour of uncommitted work (later reconstructed from session context and re-committed as `35ac995`). The same mistake was repeated later the same day: `git checkout -- src/wayland/selection.rs` reverted a mutation and also the uncommitted live-selection-box fix (reconstructed, commit `8176f0e`).
- **Root cause**: Mutation experiments and uncommitted feature work shared the working tree; the revert path was wider than the mutations and no `git status` check preceded it. Despite the recorded prevention rule, compound shell commands (`cmd1 && cmd2 ; git checkout -- …`) made the destructive revert a muscle-memory tail.
- **Correction**: Reconstructed and committed both times; subsequent mutations ran on a committed tree.
- **Prevention**: Commit **before** every mutation experiment — no exceptions. Revert mutations only after `git status --short` confirms the tree is clean apart from the mutation, and never chain the revert into a compound command with `;`.

## 5. Capture region and overlay geometry raced across threads (white borders in stitched output)

- **Mistake/context**: Long captures occasionally contained a spurious white box (2026-10-08 user report; pixel-scanning the output showed a 3px pure-white rectangle sitting exactly on a stitch seam). The capture worker kept its own region snapshot read before `PrepareCapture`, while the UI thread rendered the capture-clean overlay from whatever `drag` rectangle it had when handling that message; edit-mode drags changed the rectangle in between, so captures swept across freshly drawn borders. Two aggravators made the contamination persist: near-duplicate frame skipping kept polluted canvas rows from being overwritten, and the long-mode preview re-displayed the polluted canvas next to the box where a later edit-extend could re-capture it.
- **Root cause**: One logical fact ("what region is being captured right now") was owned by two threads with no same-source handshake; the paused flag and region updates also ran in the wrong order in the UI loop tail (unpause before region update), and `prepare_capture_clean` ignored whether its render actually committed.
- **Correction**: The PrepareCapture ack now carries the region derived from the exact rectangle used for the clean render (worker never trusts a local snapshot; frames are skipped during drags or failed renders); edit drags render the drag-trajectory union as the hole with the border suppressed; `prepare_capture_clean` reports commit success; the dim/base cache is keyed on (size, hole rect, border color, visibility).
- **Prevention**: When two threads must agree on geometry, derive both sides from a single decision point (send the decision through the handshake, don't duplicate state). Any "clean-up before an asynchronous action" must verify the clean-up happened, not fire-and-forget. When a bug involves visual output, pixel-scan the artifact to locate it geometrically before hypothesizing — the offset (border 3px + inset 1px) pinned the root cause immediately.

## 6. Capture exclusion ended before the asynchronous frame completed

- **Mistake/context**: The earlier border fix cleared its transparent trajectory on mouse release and accepted any output's successful redraw as capture-ready. It also dispatched input after drawing and then read the possibly changed selection for the acknowledgment. The documentation dismissed release-time races as too short for human input.
- **Root cause**: A commit/flush was treated as capture completion; neither cross-connection scheduling nor damage waiting has a fixed completion deadline, and monitor identity was missing from the readiness decision.
- **Correction**: Retain the exact prepared rectangle until the worker delivers its frame; all redraws preserve its transparent hole, including release/cancel and preview redraw. Commit on the selected output, roundtrip before acknowledging the worker, and return the saved rectangle rather than rereading mutable selection. Damage waits now poll cancellation so retaining exclusion cannot hang shutdown on a static screen.
- **Prevention**: Test delayed completion across shrink/move/release and preview redraw at pixel level; treat flush, commit, and frame completion as separate events. Validate readiness for the actual target output and do not infer timing guarantees from expected frame rates.

## 7. Output scale was used for both protocol geometry and overlay buffers

- **Mistake/context**: Long capture reused physical-pixel frozen-image crop rectangles as `capture_output_region` arguments, and overlay holes multiplied logical positions by `wl_output.scale` despite allocating logical-sized buffers with surface buffer scale 1.
- **Root cause**: The shared rectangle type obscured coordinate units. Existing scale-2 rendering tests allocated a hypothetical 2x buffer, missing the real allocation path.
- **Correction**: Long requests use output-local logical units as specified by the wlr-screencopy XML. Stitch offsets use returned-frame pixels; overlay holes map through actual buffer dimensions. Added tests reproducing the real logical-sized overlay on a scale-2 output. During this change, an initial tiny-region inset adjustment failed the existing 2x2 test; corrected the inset to preserve the required minimum size and added containment checks for 1–4 pixel selections.
- **Prevention**: Trace units through allocation, surface scale, protocol request, returned buffer, preview, and stitching. Test both logical-sized and scaled buffers on offset outputs, plus degenerate sizes; do not weaken existing assertions when an implementation change breaks them.

## 8. Packaging from the project root removed the source directory

- **Mistake/context**: During local installation (2026-10-12), a temporary PKGBUILD was written under `/tmp/opencode`, but `makepkg --cleanbuild` ran from the project root. It used the upstream-fetching PKGBUILD and treated the project's `src/` as disposable build output. An earlier archive attempt also ran in `/tmp/opencode` and accidentally included its own output.
- **Root cause**: Shell working directories were not matched to each operation; creating a file in another directory does not change the directory of subsequent commands.
- **Correction**: Restored the complete source directory from the pre-build working-tree archive and verified `git diff --exit-code -- src` against the staged source. Built the intended local archive in the isolated package directory, passed 278 release tests, and installed `screenshot-niri 0.1.0-6` through pacman. Removed accidental build artifacts via trash.
- **Prevention**: Run packaging only in an isolated directory, explicitly set the tool's working directory, inspect PKGBUILD there, and check the reported package version before installing. Create source archives from an explicit project root with the archive outside it. Never use makepkg clean/cleanbuild from a Rust project root containing its own `src/`.

## 9. Cached selection borders were overpainted by the stitched preview

- **Mistake/context**: User testing of the long overlay showed the selection border underneath preview pixels. The live base cache included the border, then the preview was drawn over it; a separate input redraw path omitted the preview altogether.
- **Root cause**: Final visual layers were mixed into a background cache and multiple live render paths had different compositing behavior.
- **Correction**: Cache only dim mask and transparent hole; render preview and then border on every live redraw. Reuse one live renderer for input, preview, and capture preparation; merge identical overlay constructors and remove the unused borderless entrypoint.
- **Prevention**: Test final composed pixels, including all four border edges and repeated cache reuse with changing color, thickness, and visibility. Keep capture exclusion tests against the same foreground compositor so stacking changes cannot reintroduce screenshot contamination.

## 10. Border strips omitted corners and frozen/live borders disagreed

- **Mistake/context**: Live border strips stopped at the selection's side extents, leaving four corner squares blank. Frozen selection used an unrelated inside-border rule. Edit visibility was suppressed to keep pending captures clean, hiding the box during movement.
- **Root cause**: Multiple border definitions and using UI suppression instead of rejecting invalid asynchronous frames.
- **Correction**: One outside-ring pixel predicate defines frozen and live borders, including corners and configured thickness; dirty damage includes the ring. Edit borders follow the current rectangle continuously, with sticky pending-frame invalidation on geometry change and discard before stitching.
- **Prevention**: Compare frozen/live border masks pixel-for-pixel for widths 1–16; verify partial redraw equals a full redraw after movement. Test move-away-and-back invalidation, since final rectangle equality alone cannot prove frame validity.

## 11. Widening did not fill previously uncaptured canvas strips

- **Mistake/context**: Inspection for repeated wide/narrow/wide selections found canvas writes excluded the whole old bounding rectangle, including transparent side strips. New offset rows also unioned their coverage with the empty sentinel `(0,0)`, incorrectly making missing left strips opaque.
- **Root cause**: Canvas bounds were treated as captured coverage, and empty coverage was treated as a real interval starting at x=0.
- **Correction**: Preserve actual covered row intervals and copy newly captured strips inside the old bounds; initialize empty row coverage from the frame extent. Export empty rows as transparent.
- **Prevention**: Run a textured wide → centered narrow → scroll → wide sequence, assert every output pixel and alpha, then repeat resizing. Check that recaptured strips contain real pixels and uncaptured intermediate strips remain transparent.
