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

### 1. Documentation drift: project docs claimed long mode was unsupported while code implemented it

- **Context**: Project docs still described long screenshots as "out of scope"/"test-only" while `src/app.rs`, `src/cli.rs`, and `src/stitch/` implemented them (commits `b289606`..`a8eb251`). A separate module-contract file (`src/CONTRACT.md`) also drifted.
- **Root cause**: Docs were not updated in the same changes that landed the work.
- **Correction**: Updated the stale scope/relationship claims. Module boundaries now live as a concise "Module Contracts" section in `AGENTS.md` (the standalone file was later removed).
- **Prevention**: Treat code as source of truth; verify with `grep`/`read` before citing docs. Update `AGENTS.md` and this file in the same commit as behavior changes.

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

### 12. Cited upstream default values from memory, which inverted a problem's root cause

- **Context**: While diagnosing Hyprland screenshot/cursor behavior, an assertion was made that `cursor:hide_on_key_press` defaults to enabled and therefore the launch hotkey hides the cursor, attributing the "requested cursor frame has no cursor" problem to default behavior. Checking the release source (`src/config/values/ConfigValues.cpp`, v0.56.2) showed `hide_on_key_press` defaults to `false`, `inactive_timeout` to `0` (never), and `invisible` to `false` — with defaults, the cursor is not hidden at all.
- **Root cause**: Upstream default values and behaviors were quoted from memory/wiki impressions instead of being verified against the actual source of the pinned version.
- **Correction**: Re-read the release source option table line by line; re-scoped that problem to "requires the user to have enabled a hide-related option", and documented the corrected attribution.
- **Prevention**: Any assertion about an external dependency's behavior (default values, version, exact semantics) MUST cite a source file + line, or an official record. Mark unverified claims explicitly as unverified and never use them as a premise for reasoning or fixes.

### 13. 长截图死锁：全输出捆绑的渲染欠账在多输出下结构性无法清账（客户端缺陷，已修复）

- **Context**: 双输出机器上 `--long` 每次报 "long capture produced no frames"，且滚完干等几分钟也永不恢复；同源码单输出机器正常、v0.1.0 双输出正常。排查手段：多轮 trace 日志（prepare 门槛、commit/release 两端对账、pending_render 状态）+ 独立压力客户端（`/tmp/opencode/wl-stress`）建立健康基线做差分。
- **已证事实**: ① Smithay 在 commit 处理时即时发送 `wl_buffer.release`（`InnerBuffer::drop`），niri 无辜；② 压力客户端：2 画布 + 60fps 无条件提交 + 相同内容 + 双输出各一 surface（728 提交/727 归还）完全不饿——提交节奏、缓冲数量、内容去重、输出数量均非独立诱因；③ 被选中输出的拖拽结束瞬间存在"全部画布已提交、release 在途"的正常瞬态（每次运行 long-start 均为选中输出 available=0）；④ 卡死期间 release 仍在持续到达（单次会话 15046 次），但可用画布恒为 0——深度日志抓到乒乓闭环：`pending_render=Some(false)` 永久滞留，每次 release 触发 `flush_pending_render` 立即重提交刚释放的画布（内容与屏幕相同的无效提交），`committed=1/2 -> pending=Some` 无限重复。
- **Root cause**: 渲染欠账的清账条件写死为"单次 flush 内提交全部 overlay"（`committed >= overlays.len()`），而欠账的触发只需一次部分提交（拖拽期间另一输出的画布恰在 niri 手里，双输出下常态）。两者复合：`Some` 一旦记上，清账在乒乓下结构性不可满足 → prepare 永远抢不到画布 → 永久死锁（干等也不恢复，用户观察正确）。单输出机器 `len=1`，任何成功渲染都满足清账 → 缺陷结构性不可见。8176f0e 砍缓冲 3→2 只是加重了瞬态频率，不是根因；此前"提交节奏/缓冲余量/niri 不归还"三个归因均被实验逐一推翻。
- **Correction**: ① commit-on-change：buffer 已显示当前 rect+preview seq 则跳过提交、计入"已满足"，prepare 直接 ack——采集与提交/release 彻底解耦；② 渲染范围限定（`UiState::render_scope`）：长截图转换起渲染与欠账只统计选中输出的 overlay，"长截图=单屏"成为结构事实而非补丁，乒乓断粮；③ 缓冲恢复 3 块（吸收提交突发）；移除误加的"饥饿重建画布"兜底（实测不必要）；`flush()` EAGAIN 不再致命。
- **Prevention**: 记账单位必须与资源所有权一致——画布按输出独立周转，欠账就不得捆绑全部输出做 all-or-nothing 清账；"部分提交"产生的债务必须有可达成的清账条件；用最小压力客户端建立健康基线，再逐项逼近应用模式做差分定位；release 接收与 commit 发送两端同时打点对账，不要单边推断。

---

## Verification Memory

- **2026-10-13 local install**: HEAD `ca24865` packaged from an isolated directory (`git archive` tarball + local-source PKGBUILD, pkgrel 11) and installed via `pacman -U`; makepkg `check()` passed with 133 lib + 153 bin tests (2 profiling tests ignored, as before). GitHub `origin/niri` still at `a8eb251` — the repo PKGBUILD (git source) would build outdated code until pushed.
- **2026-10-13 long-capture deadlock root-caused and fixed** (see lesson 13): dual-output `--long` starvation traced via staged trace logging (prepare gates → release/commit accounting); final run on the dual-output machine: 178 commits, 173 releases, 30 frames captured and stitched, zero starvation. Smithay source confirms release is sent at commit-processing time (`InnerBuffer::drop`), exonerating niri.
- **2026-10-12 final state**: 133 lib + 153 bin tests pass (two profiling tests ignored); `cargo check --locked` and `cargo build --release --locked` pass. Two pre-existing clippy errors (`absurd_extreme_comparisons`) and two unused-import warnings in `cli.rs` exist at the prior commit and were not introduced by recent work.
- Release-binary smoke checks with Niri hints unset and desktop set to `sway` reached the deliberately unavailable Wayland connection instead of a compositor-name rejection.
- Wayland dispatch/compositor behavior (input grabbing, wheel blocking, passthrough toggling, live capture) remains manually testable only under a live session; unit tests cover the pure logic and pixel rendering.
- **Hyprland findings (2026-10-12)**: `docs/hyprland-findings.md` records three Hyprland-side problems (wheel passthrough, missing cursor in cursor-requested frames, cursor not removable from region frames), each with triggers, upstream evidence, and what remains separately fixable on our side. Mechanisms are source-verified; none reproduced end-to-end on a live Hyprland session yet.
