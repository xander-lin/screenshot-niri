# found.md — Project Memory & Lessons Learned

Durable, verified project memory plus actionable lessons. Code is authoritative over docs.

Lesson entry format: **Context**, **Root cause**, **Correction**, **Prevention**. Record only verified incidents; never fabricate.

---

## Project Context

`screenshot-rs-rebuild` is a Wayland screenshot tool (binary `screenshot`), originally developed against niri's wlroots protocols. Normal region capture and long/scroll capture are implemented end-to-end.

- **No compositor gate**: the launcher no longer checks `NIRI_SOCKET` or `XDG_CURRENT_DESKTOP`. It attempts the Wayland protocols and leaves compositor compatibility to user testing. Out of scope: X11, focus-grab extensions, diagnostics bins, sticky-header/footer compensation (a selection containing them is a user error; `E` mode is the sanctioned way to exclude them).
- **Modules**: `cli` parses args and resolves output paths. `app` orchestrates capture → selection → composite → write → clipboard (no preflight). `wayland::screencopy` freezes outputs and drives the persistent long-capture session. `wayland::selection` renders the layer-shell overlay and collects geometry. `geometry` maps logical selections to per-output regions. `image` composites and writes PNG. `stitch` is the multi-stage matcher (canvas placement, perceptual bands, fuzzy hash, fast-motion scan, fixed bands diagnostics, previous-frame fallback), driven by `app` from the long-capture worker. `clipboard` serves `image/png` or file URI via `wlr-data-control`.
- **Config** (`~/.config/screenshot/config`): `max_fps` (long-capture cap, default 30, range 1–240) and `border_width` (1–16). The cap is a ceiling over damage-driven pull capture.
- **Long capture**: `--long` selects one output, captures continuously, and stitches on Enter/Space. `E` toggles edit mode (per-edge resize, interior move) which blocks wheel scroll; region-changed frames are placed at the deterministic offset-implied position, growing ragged T/L/arch canvases whose uncovered extents stay transparent. Capture pauses during edge drags.
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

### 14. 拼接器与 DMS 对比：NoMatch 推进参考帧造成静默错位

- **Context**：按用户要求对比 DMS（DankMaterialShell v1.6.3）`dms screenshot scroll` 的拼接器。用同一份 1920×1080 像素语料分别驱动 DMS 原始 `stitch.go`（Go 复现，`/tmp/opencode/dms-sim`）与本实现，得到性能与鲁棒性数据；完整报告见 `docs/dms-stitch-comparison.md`。
- **已证事实**：① 1080p 同语料顺序采集（画布长到 12360 行）：DMS 平均 1.10 ms/帧，本实现 87.0 ms/帧；回访批 DMS 1.36 ms，本实现 1777 ms（最差 4215 ms）；本实现单帧 NoMatch 在 12k 行画布上 1.0–1.5 s。perf 归因为逐行 RGB 比较（`rgb_row_average_difference` + `CanvasScoreCache::score`）。② 200 行帧序列 `0,40,80,400,440,480`：400 帧 NoMatch 后，440 帧的 `delta_y=40` 被叠加到**未推进的** `viewport_rect`（仍对应页面 80）上，画布只增 80 行，行映射从页面 280 直接跳到 600——静默丢 320 行。③ 顶部粘性 chrome 扫描：≤10% 正常，12% 起退化，13.9% 起本实现 0/9 卡死，DMS 到 18.5% 仍 9/9；底部 chrome 结论相反（本实现 ≤8% 正确，DMS 4% 起就开始失败）。④ `FixedBandDetector` 只产出 trace，检测到的 `stable` 从未写回 `active_crop`（`src/stitch.rs:645-654`），运行时的"固定页眉页脚检测"名存实亡。⑤ 本实现画布无字节/行上限（DMS 为 256 MiB / 30000 行），1920×12360 画布 RGB 71.2 MiB；预览转换实测 1024×12000 为 36.9→49.2 MiB、13.8 ms/帧，按同公式 1920×12360 约 71→95 MiB、26–28 ms/帧（**[推断]**，未在 1080p 上直接计时）。
- **Root cause**：`push_frame_sources` 在返回 `NoMatch` 前更新 `previous_compose_frame` / `previous_perceptual_frame`（`src/stitch.rs:744-747`），但 `viewport_rect` 保持旧值；随后 `try_apply_match` 用 `translate_viewport_rect(previous_rect, match_info)`（`:844`）把"相对失败帧"的位移当作"相对上一有效视口"使用。参考帧与锚点在语义上分离，却没有对应标记。
- **产品决定（2026-10-17）**：粘性页眉/页脚的补偿是**用户选区错误**，不是拼接器职责。已写入 `AGENTS.md` 的 Out of scope 与 `stitch` 模块契约：拼接器不检测、不裁剪、不兜底；E 模式是排他手段。因此事实③**不是缺陷、不排期**，DMS 的 `matchIgnores` 忽略带**不借鉴**（它的活动行概念可以借鉴）。
- **Correction**：仅记录结论与优先级，本次未改代码。建议顺序：①把 `NoMatch` 后的相对位移判定改为需要重新与画布对齐（或同时推进 `viewport_rect` 并标记未验证锚点）；②加画布上限并在满额时停采但保留预览；③引入显式 seam/Unanchored 语义；④匹配前置走行签名缓存、预览增量转换；⑤清理 `FixedBands` 的名实不符（标注为未接线的诊断代码，或删除生产侧调用）。
- **Prevention**："参考帧基线"和"画布锚点"是两个独立事实，任何一方推进都必须让另一方保持一致，或显式标记为"未验证"。外部实现对比不得只读源码下结论：DMS 的"均值 + 活动行 + 上下忽略带"与我们的"逐行阈值"在页眉/页脚上会得出**相反**的鲁棒性排序，必须用同一份像素输入实测才能下判断。同时："对照实现做了某件事"不等于"我们也该做"——先判定它属于缺陷还是产品边界，再决定是否排期。

### 15. 拼接匹配：盆地只有一行宽、粗筛短名单不可靠、门限不能惩罚「边界少」

- **Context**：为修复教训 14 的 NoMatch 错位接受，按用户提出的「比较亮度变化的边界位置 + 方向冲量 + 拼不上就红框冻结锚点」设计做实验。语料是 6 类 1920×20000 无头 Chrome 渲染页面（正文 / 重复卡片 / 恒定行高表格 / 极稀疏 / 满宽 / 代码），每帧都是已知行偏移的裁切，所以「应落在画布第几行」精确已知，错位可计数而非目测。完整报告见 `docs/edge-stitch-design.md`。
- **已证事实**：① 候选位置从真值挪开 1 行，边界阶梯代价涨 47 倍（亮度值 18 倍）——正确位置是尖峰不是山，步长 ≥2 的粗扫都可能采不到；② 粗筛短名单在恒定行高表格上召回率 **0–33%**，短名单里全是周期诱饵；③ 半行滚动下亮度描述子正确率从 83% 掉到 11%，边界阶梯从 65% 升到 91%；④ 端到端 600 帧：现有实现 254 静默错位（480 帧口径，其中 154 帧本就无重叠），原型 **0 静默**、216 红框（全部无重叠），跟踪态单帧 32.6–76.4 ms → **1.8–2.3 ms**；⑤ 门限消融：`separation = (second-best)/second ≥ 0.80` 是 0 静默的拐点（0.30 时 13 帧静默，全部是周期内容的整数倍错位）；⑥ 8 列边界阶梯在周期内容上仍留 42 帧静默，48 列才为 0。
- **Root cause（现有实现）**：把「匹配」实现成「在整幅画布上找最像的位置」——搜索空间随画布线性增长、唯一行宽的盆地要求逐行穷举、且没有「拼得上但不敢确认」这个状态。加上教训 14 的参考帧/锚点不同步，错位就成了静默数据损坏。
- **我自己犯的错误（同轮修正）**：初版设计有 `boundary_mass` 质量门限（整屏边界太少就红框），把稀疏页面判成「无法长截图」并写成产品取舍。这是错的：删除该门限后 480 帧结果完全不变（唯一差别是 `no_signal` 计数），稀疏页 `sparse2` 稳态 24/24 全对。同一错误的另一种表现是绝对代价门限 `best ≤ 0.030`——它惩罚的正是「边界少」，导致稀疏页 `fine@truth=0.0324` 被判红。**用户指出「只要不是所有像素都一致就该能找到边界」，实测确认。**
- **我自己犯的第二个错误（2026-10-18 修正）**：§7 把 P0-1 的验收值写成「修复后画布 520 行」，但**从未实测过这个数字**——它是按「440/480 会被正确接受」推演的期望值，而修复 A 的语义恰恰是不接受它们。实际打补丁实测为 **280 行、三帧全 NoMatch**。教训：**验收标准里的每个数字都必须是跑出来的，不是推出来的**；尤其是「修复后应该变成什么」这类断言，推演很容易把「期望行为」和「实际语义」混在一起。
- **Correction**：本轮只产出设计与证据，未改生产代码。建议顺序：①先单独修教训 14 的参考帧推进问题（可独立验证：`0,40,80,400,440,480` 序列修复后画布应为 **280** 行、且 400/440/480 三帧全部 `NoMatch`；早先写的 520 是错的，见 `docs/edge-stitch-design.md` §7 勘误）；②按 `docs/edge-stitch-design.md` §5 替换匹配器；③`PushResult` 增加显式拒绝态并在 overlay 置红，红框期间采集降到 5 fps。
- **Prevention**：门限只能惩罚「证据不足以区分」，不能惩罚「页面属性」。具体地：不用绝对代价门限（随页面对比度漂移，且惩罚稀疏），不用边界数量门限（把「边界少」误判成「不可信」），只用 runner-up separation（歧义）+ `best/median`（自归一化）+ 最小可比较行数（统计前提）。性能优化的对象是**窗口**不是**候选数量**——周期内容上减少候选等于减少正确性；任何「粗扫 + 短名单」都必须先测召回率。描述子的横向分辨力由 separation 是否失效反推（1 值/行 与 8 列都不够，48 列才 0 静默）。另：测量前先确认语料的「尾部空白」不是页面属性（`sparse.png` 内容只到第 10557 行）。


### 16. 方向选择与 DMS 正面对比：多一个方向不是免费的鲁棒性

- **Context**：用户追问「只用了横向边界还是纵向边界？有必要测两种吗？」以及「现在到底什么状态？和 DMS 比如何？」。为此写了方向消融实验（`study_dir*.py`）并把 DMS v1.6.3 的 `stitch.go` + `scroll.go` 状态机原样接入同一 harness 做正面对比（`/tmp/opencode/dms-cmp`）。完整报告见 `docs/stitch-direction-and-dms.md`。
- **已证事实**：① 本设计只用横向边界（`|dL/dy|`）；在正常网页上 H、V、H+V 三种描述子完全等价（各 60/60 正确、0 错误、separation p5≈0.97）。② 退化内容上各有盲区：纯竖条纹 H 无候选、纯横条纹 V 无候选、纯渐变两者都无候选。③ **双方向会稀释 separation**：`tallgrid` 在 σ=10 时 H 单独 9/9、V 单独 0/9、H+V 0/9；原因是 V 的代价曲线 spread 只有 0.055（H 是 1.000），等于给所有候选打「差不多」的分，平均后把 H 的尖锐度冲淡（sep 0.990 → 0.769）。④ 端到端 720 帧只换方向：H/V/HV 的接受、拒绝、覆盖率都在噪声内（490/489/490，58.6%），V 的跳变少 1 次（单帧事件）。⑤ 与 DMS 正面对比（720 帧、同真值）：DMS 630 接受 / **90 次静默跳变** / 2.55 ms；本设计 490 接受 / **2 次跳变** / 13.4 ms / 覆盖率 58.6% vs 55.9%。⑥ DMS 的 90 次跳变全部来自 `seamAppend`（`scroll.go:560-570`：连续 4 次 unmatched 后不验证重叠直接追加），本设计的 230 次拒绝中 **0 次是本可匹配却拒绝**。⑦ 本设计的 13.4 ms 里 85% 花在宽窗+锚点窗（只在长跳时触发），是「愿不愿花时间确认」的选择，不是实现效率问题。
- **Root cause（方向问题）**：多方向描述子只有在「该方向携带独立位置信息」时才有增益。滚动场景下纵向结构不随滚动变化，纵向边界给的是与位置无关的常数信号；把它与横向边界平均，等于引入低区分度的稀释项。这与教训 15 的「门限不能惩罚页面属性」是同一类错误：**信息量不能用「值更多」来衡量，要用「能否区分位置」来衡量**。
- **Correction**：决定保留单方向（横向），不实现双方向；把理由写成可证伪约束（若遇到「纵向结构极弱、横向极强」的真实页面需重评）。生产代码仍未改动。
- **Prevention**：新增一个维度前必须测「它在该维度上是否携带独立位置信息」，而不是测「加了它准不准」——正常内容上几乎所有描述子都准，测不出稀释效应。检验方法是看代价曲线的 spread（区分度）与 separation，而不是 top-1。与外部实现对比时必须用**能区分「丢帧」与「错位」的指标**：DMS 的接受数更高，但把 90 次无法确定的情况写成静默接缝；只看「接受率」会得出完全相反的结论。


### 17. 换掉一个模块时，所有「为旧模块的失败模式写的补丁」都要重新审

- **Context**：把匹配器从「整幅画布找最像的位置」换成「预测窗口内验证边界位置」后，生产代码里还留着两条为旧失败模式写的补救路径：`recover_stale_bottom_edge`（在「找不到候选」后逐行 RGB 前缀搜索画布）和 `find_dirty_overlap_recovery`（每帧扫描画布边缘找脏条）。两者都编译通过、单测全绿，但语义已经变了。
- **已证事实**：① `recover_stale_bottom_edge` 在新引擎下变成**后门**——新引擎的拒绝是判断而不是失败，这条更宽松的第二匹配器会在拒绝之后把帧放进去。实测启用它：静默错位从 2 帧涨到 **29 帧**（480 帧口径），全部在重复内容上，因为行前缀相似度在卡片/表格/代码页会在偏几百行的位置报告 70–250 行「稳定」。② `find_dirty_overlap_recovery` 每帧运行、约 **4.3 ms**（占单帧预算 43%），在干净的连续滚动上**从未命中**。加两条必要条件（帧到达画布边缘 + 引擎匹配不完美）后单帧 9.9 → 6.3 ms，质量指标不变。③ `NOMATCH_RECOVERY_MIN_STABLE_ROWS = 8` 在 700 行视口上，10 行前缀就能让它接受偏移 212 行的位置；改成按帧高比例（`max(8, h/20)`）并把判据从「平均差」换成「已证明的重叠行数」（刷新过的脏条本身平均差就高，用平均差会把这条路径存在的理由一并拒掉）。
- **Root cause**：补救路径的存在理由是「旧实现会失败」，而新实现的「拒绝」是**有意的判断**。两者的语义方向相反：补救路径在失败后加一次宽松尝试，而判断被绕过之后，宽松尝试就变成了唯一的裁决者。这类代码不会编译失败，也不会在旧测试里暴露——旧测试断言的正是旧语义（「找不到候选时应该能恢复」）。
- **Correction**：从拒绝路径移除 stale-edge recovery，只保留在接受路径（引擎确实给出了候选、画布边缘需要重写）；给 recovery 加成本前置条件；把它的证据门槛改成与帧高成比例。三处都有新测试，且都验证过「改回旧行为测试会红」。
- **Prevention**：替换一个模块时，除了迁移它的调用点，还要**列出所有「因为旧模块做不到而存在的代码」**，逐条问「新模块做到了之后，这条路径是补救还是后门」。判据：如果这条路径能在新模块**已经做出判断之后**仍然改变结果，它就是后门。另：性能回归要按调用点归因，不要只看总耗时——这次 43% 的预算是「每帧都跑、从未命中」的一条路径，删掉前置条件不满足时的调用比优化算法有效得多。

### 18. 落地时的验收数字必须来自实测，不能来自推演

- **Context**：P0-1（NoMatch 不推进参考帧）的验收值在文档里写成「修复后画布应为 520 行」。这个数字是按「440/480 两帧会被正确接受」推演出来的，从未跑过。
- **已证事实**：实际打补丁后是 **280 行**，且 400/440/480 三帧全部 `NoMatch`。修复 A 的语义恰恰是**不**接受它们（参考帧不推进，这两帧与画布无重叠）。推演把「期望行为」和「实际语义」混在了一起。
- **Root cause**：写验收标准时，从「我希望系统变成什么样」出发，而不是从「这个改动的语义必然导致什么」出发。前者是愿望，后者是推论。
- **Correction**：`docs/edge-stitch-design.md` §7 加勘误，`found.md` 教训 15 的 Correction 同步改为 280，并写明「早先写的 520 是错的」。
- **Prevention**：验收标准里的每个数字都必须跑出来。尤其是「修复后应该变成什么」这类断言——先把补丁打上跑一遍，再写进文档。如果暂时不能跑，标注 **[未验证]** 而不是给一个看起来合理的数。



### 19. 合成语料只能验证结构，不能作为验收标准

- **Context**：用户报告「分明应该能拼接上的，非得报红框」。我怀疑是**亚像素滚动**（浏览器按设备像素滚动，相邻帧位移不是整数行，而引擎只在整数行上搜索），于是投入大量工作做画布多相位重采样、帧侧相位搜索、相位窗口测量。
- **已证事实**：① 用真实 Chrome 验证，`scrollTo(0, 1000.25)` 实际是 1000、`1000.5` 是 1001、`1000.75` 是 1001——**DPR=1 时浏览器把滚动位置量化到整数像素**，所谓亚像素相位根本不存在。② 我的合成语料里「存在」亚像素，因为那是我用线性插值**伪造**的。③ 用真实浏览器帧（headless Chrome + `Emulation.setDeviceMetricsOverride`，98 帧）实测旧门限：**8 接受 / 89 拒绝 / 0 错位**——位置全对，92% 被拒，正是用户看到的红框。④ 正确帧的 `sep` 实际分布是 0.637–0.915、`bm` 是 0.043–0.152；合成语料因为「同一张图裁切」一致性偏高，`sep` 普遍 > 0.85，所以 `SEP = 0.80` 在合成语料上看起来安全，在真实渲染上把 92% 的正确帧拒掉。重标定为 `0.45 / 0.28` 后：**97 接受 / 0 拒绝 / 0 错位**，跳变误接受仍为 0/12。
- **Root cause**：合成语料由同一张渲染图裁切而来，帧间差异只有「位置」，没有真实渲染的字体 hinting、抗锯齿、图片解码时序、滚动条重绘。它足以验证算法的**结构**（盆地宽度、扫描顺序、门限方向），但**分布**是错的，而门限是分布的函数。用合成语料定门限，等于用错误分布定阈值。
- **Correction**：门限按真实浏览器帧重标定；`docs/edge-stitch-design.md` §12 记录完整实测与三个缺陷（门限过严、长位移上限缺失、竞争峰被当成肩部）；相位搜索的全部代码已撤回。
- **Prevention**：**验收必须用真实数据**。判断「这个假设是否成立」时，先用真实环境做一个最小验证（这次是 6 行 CDP 脚本），再决定要不要投入实现。合成语料可以用来做回归（保证结构不被破坏），但不能用来决定阈值。另：当用户说「明明应该可以」时，先量**正确帧被拒的比例**，而不是先猜机制——这次量出来是 92%，一眼就能定位到门限而不是算法。

---

- **Mistake**：在 headless Chrome 的 CDP 截图语料上把门限从 `sep 0.80 / bm 0.10` 标定到 `0.45 / 0.28`，并宣称「97 接受 / 0 拒绝」。用户真机实测是「一点都不能工作」。
- **Root cause**：CDP `Page.captureScreenshot` 拿的是浏览器自渲染结果，**不经过 niri 合成器**，也没有 overlay 干净渲染握手和 `zwlr_screencopy_manager_v1`。它和工具实际消费的帧不是同一种东西，所以在那上面标定出来的门限没有意义——教训 19 说的是「合成语料不能定阈值」，而 CDP 截图**仍然是一种合成语料**，只是更逼真。
- **Correction**：做了 `capture-frames`（只抓真实帧）和 `replay-frames`（离线回放 + 逐像素真值 + 回投影验证）。真实语料 462 帧暴露两个致命 bug：前插时引擎坐标系被破坏（`pos`/`write`/`delta` 用了像素行而非逻辑行，且 `shift_down` 把逻辑区间也平移了，导致新帧内容根本没写进画布）、拒绝后位移守卫不放宽导致永久停摆。修复后 2 接受/405 no-match → **204 接受/0 no-match/0 像素级错位**。见 `docs/edge-stitch-design.md` §13。
- **Prevention**：
  1. **「合成」的判据是「是否经过被测路径」，不是「是否像真的」。** 任何用来定阈值的语料，必须先确认它和产品实际输入同源（同一 API、同一合成器、同一握手）。做不到就用产品自己的抓取路径去取。
  2. **给非平凡子系统写「能失败」的回归测试，覆盖每一条几何分支。** 前插 bug 活了很久，因为 `engine.rs` 的测试全部只覆盖「向下增长」的画布，`prepend` 路径一行测试都没有。新增测试必须在修复前失败。
  3. **验证判据要独立于被测逻辑。** 用「帧像素是否与画布吻合」和「帧在画布中的位置是否与邻居连续」判断错位，而不是用拼接器自己给出的 `delta` 去比对——后者会把「拒绝期间漏掉位移、恢复时一次补齐」误报成错位（这次两帧就是这样被误报的）。

---

### 21. 「跳变 + 大量重复帧」是正常输入；无置信防护的快速分析会把好引擎误诊成坏引擎

- **Context**：real-1 修复后仍剩三项未闭环异常：19 个高成本帧被写入画布、链漂移 -257px、2 帧 delta 与真值差 -160/-180。我用 python 对 21 个高成本帧做条带位移分析，得出「10 帧撕裂（条带位移差 54–250px）、写入即损伤」的结论，并开始设计「撕裂帧检测→拒绝」机制。用户指出：某些窗口的移动特性就是**跳变性**的（real-1 里 461 对相邻帧 348 对 delta=0，其余为 60–120px 档位跳动），重复帧多正是这个原因，且**全部可连接、不存在连不起来的帧**。
- **已证事实**：给 replay-frames 加严格条带判据（每个被接受帧分 4 条带、独立在画布上定位、要求置信边际 + 完整落在画布内）后：**real-1 全部 204 帧的全部条带 0 位移**，六个语料全部 0。我此前的「10 帧撕裂」全部是测量噪声：低纹理条带的最佳位移是随机数。判据工具自身在此过程中暴露两个 bug：① 候选位置把条带推出画布时 `count==0` 返回 cost 0.0（「完美吻合」），末帧底部条带因此必然被标；② 骑在画布边缘的条带拿全条带和残条比较不对等。-257px 漂移定性为：跳变过渡帧（一帧内含两个滚动位置的混合）上，oracle 与引擎对「这帧位移是多少」这个**无定义问题**给出不同答案，任何帧都没有写错位置。
- **Root cause**：两次都错在**判据先于结论**：第一次用无防护的裸匹配下结论（低纹理 → argmin 是噪声），第一次写判据时又踩了「空比较 = 完美」的地雷。内容本身没有错——错的是我把「高残差」直接当成了「撕裂」。
- **Correction**：撤销「撕裂帧检测→拒绝」的设计方向；引擎不为跳变输入做任何改动（数据证明它已正确处理）。判据保留在 replay-frames（coverage / band / drift 三项 + 按语料中位数自适应的成对异常阈值）。
- **Prevention**：
  1. **对「看起来不对」的数据下结论前，判据本身要过三道关**：置信边际（最佳必须明显优于次优/原位）、完整重叠（比较双方必须可见同样多的像素）、否定对照（在已知干净的语料上必须报 0）。这次否定对照（CDP 语料）反而暴露了判据自己的两个 bug。
  2. **cost/误差函数永远不应对空比较返回最优值**。`count==0 → return 0.0` 让「什么都看不见」击败「看见且吻合」，任何搜索都会被它劫持。
  3. **重复帧多、位移跳变是正常输入形态**（滚轮档位、PDF 翻页、终端输出），不是缺陷信号；不要为它设计拒绝机制，除非带防护的判据显示真实损伤（这次是 0）。

### 22. 连续强度描述符换不掉分档描述符：真匹配代价非零使门限分布与伪匹配重叠

- **Context**：用户要求「贯彻连续的理念」消灭分档（0/1/2 级），动机是消除 per-frame `gain` 缺陷（教训见 `#[ignore]` 测试 `a_high_contrast_block_does_not_erase_the_low_contrast_rows`，b55235e）。我实现了四种连续方案并全部实测：sqrt 映射 × 三种归一化（`Σ(a+f)` / 固定 255 / `Σmax`）+ 线性映射 × 固定基准，配套修了三处门限逻辑（`second=None` 的 sep 语义、rival 阈值与 sep 阈值的一致性、`EDGE_TIE_RATIO=1.1` 区分真并列与弱分离）。
- **已证事实**：最优连续配置（sqrt×16 + 固定基准 255 + sep 0.30 + bm 0.45 + tie 1.1）下，**七个真实语料全部回到基线**（real-1 203/0/0/0/0、real-long 97、lowcontrast-2b 78，零错位零条带，漂移与 HEAD 相同），**gain 缺陷测试通过**。但 5 个单元测试失败，全部是 `frame_from_world_rows` + `row_texture` 的「跳转必须拒绝」族。两个测量否决了所有门限调参：① 伪匹配 bm 0.36–0.55 vs real-long 真模糊帧 bm 0.40–0.48，**分布重叠**，任何 bm 阈值无法分离（bm 0.35 全测试过但 real-long 崩到 19）；② `row_texture` 是**恒定梯度纹理**——world 0/80/400 的强度分布几乎相同（非零 9574–9578/9600，聚集 75–86），任何幅值描述符在任何对齐下都「匹配」；旧分档能拒绝纯靠 26 阈值恰好切进聚集区的抖动运气。sqrt 在高强度段把 40–200 的梯度压进 101–226 窄带（差异压缩 ~5×）是不相关强纹理伪匹配 0.02–0.05 的直接原因；线性映射消除伪匹配但真周期内容并列检测失效，real-1 出 4 个位移条带。
- **Root cause（初版结论，后被推翻，见下）**：分档描述符的真匹配代价**恰好为 0**（档位全同），伪匹配代价高，门限可以卡在中间；连续描述符的真匹配代价**非零**（运动模糊/噪声真实改变梯度幅值，real-long 真帧 0.02–0.22），伪匹配（0.02–0.05）落进同一区间。这不是实现缺陷，是「保留幅值连续性」与「真匹配零代价」的内在冲突。
- **Root cause（2026-10-22 修正）**：「范围重叠」这个结论**是归一化造成的假象**。上面 0.02–0.05 的伪匹配数字来自 `mean/255`（把差值除以 255），它把全部数值压进 0.001–0.13，于是真模糊帧 0.022 与伪匹配 0.12 看起来接近。改用 `Σ|a-f| / Σ(a+f)` 后实测：real-1 清晰真匹配 **0.0105**、real-long 模糊真匹配 **0.0440**、合成伪匹配 **0.41–0.43**，相差 10–40 倍，**根本不重叠**。真正的阻碍有两项，都与「连续 vs 分档」无关：① 门限（`bm 0.28`）是按分档描述符的代价尺度标定的，换成连续尺度必须重新标定（`bm 0.35` 后 real-long 从 19 恢复 97）；② 5 个失败测试的 fixture 是 `row_texture` 恒定梯度纹理，任何幅值描述符在任何对齐下都得到相似分数——是 fixture 无法表达「内容不同」，不是描述符无能。修正后连续版（sqrt×16 + 固定基准 255 + 重新标定的门限）**七语料全部回到基线**，仅剩那 5 个退化 fixture 测试。
- **Correction**：全部改动回退（补丁存 `/tmp/opencode/continuous-descriptor.patch`，407 行）；仓库恢复 157 lib + 178 bin 全绿、七语料基线。gain 缺陷随后以**session 级 gain**修复（见 Verification Memory 2026-10-21）：gain 保留（分档需要它救低对比），但由首帧定一次、整个会话复用——标尺统一后跨帧不一致消失，且分档的「真匹配零代价」根基未动。
- **Prevention**：
  1. 换描述符时，「真匹配代价是否为 0」是首要问题，先于任何门限标定——它决定门限能否存在分离带。
  2. 用 `row_texture` 这类恒定梯度纹理做「内容不同必须拒绝」的 fixture 是无效测试：它测的是档位抖动，不是描述符判别力。
  3. 「门限调参救不救得回来」用分布重叠判断（伪匹配 bm 范围 vs 真帧 bm 范围），不要逐点试阈值——重叠即无解，省掉整轮扫描。

### 23. 用「输出里有没有 test result 行」当验证判据，会在编译失败时误报通过

- **Context**：本轮改完门限后跑「三种 feature 组合」验证，命令是 `cargo test --locked --features X 2>&1 | grep -E "^test result" | head -1`。`trace-logs` 组合实际编译失败（`E0425`，trace 里引用了尚未绑定的 `second_over_best`），但输出里仍出现了一行 `test result: ok. 159 passed`——那行来自同一命令序列里的其他 target 或上一次的产物，grep 只看「有没有匹配」，没看「是谁的匹配」。于是交付里写下了「三种 feature 组合全过」，实际 HEAD 根本编不过 `--features trace-logs`。事后 review 时手工 `cargo build --features trace-logs` 才发现。
- **Root cause**：判据是**文本存在性**，不是**进程成败**。`grep` 与 `cargo` 是两个进程，管道的退出码属于 `grep`；`head -1` 还会在 `cargo` 写完前关闭管道。三件事叠加：失败信息不在 grep 的模式里、成功行可能来自别的 target、退出码被管道吃掉。
- **Correction**：把判据落在**退出码**上——`cargo test ... || exit 1`，或 `set -o pipefail`；人工确认时看输出里有没有 `error[`，而不是有没有 `test result`。
- **Prevention**：
  1. 验证命令的成败只看退出码（`&&` / `|| echo FAIL` / `set -o pipefail`），不要用「输出里出现某行」代替。
  2. 多 feature 组合要**逐条**跑并各自检查，不要在 for 循环里只 grep 一个模式——循环会把「哪一条失败」抹掉。
  3. `#[cfg(feature = ...)]` 与「宏丢弃 token」会让默认构建**完全不检查**该路径；改动了 feature 门控代码必须显式构建该 feature。

### 24. 给「受支持的状态」发明警告：普通模式跨屏选区被判成错误

- **Context**：普通模式（非 `--long`）的选区可以跨多个输出，抓取路径按输出切块再合成（`SelectedViewport::from_outputs` 每个输出一块，`composite_captured_region` 拼回），这是**受支持**的截图方式。但白红警示边框曾对「拖拽中的跨屏选区」以及「编辑模式下的跨屏选区」点亮，只有松开鼠标后才恢复白色。用户实测反馈：普通模式跨屏是正常用法，不该出现红框。
- **Root cause**：把「长模式无法跨屏」这一真实约束**扩大**成了「跨屏本身值得警告」。实现时把「拖拽中」当作「用户还能修正」的窗口，于是给了实时警告——但普通模式的跨屏选区根本不需要修正。这是**发明需求**，不是需求变更：产品从未要求普通模式跨屏警告。
- **Correction**：`border_color_for` 去掉 `drag`/`edit_drag` 两个参数，警示条件收敛为 `spans && long_mode`（外加独立的 `stitch_warning`）。普通模式下跨屏选区在拖拽中、松开后、编辑中都保持白色。
- **Prevention**：
  1. 警示色只对应**产品拒绝的状态**。写「这里要不要警告」之前，先问「这个状态最终会不会被拒绝/裁剪」；会被正常处理的状态不配警告。
  2. 「用户还能修正」不是警告的理由；只有当**不修正就会失败**时才成立（长模式跨屏：不修就抓不了）。
  3. 判据收敛到状态本身（`long_mode && spans`），不要靠交互阶段（`dragging` / `editing`）去近似状态——阶段判断会把合法状态误判成警告。

### 25. 「一致性不变量」被一个不可能出现的测试状态证伪，于是把正确实现改错了

- **Context**：清理引擎测试时，我认为 `colour_cost_at` 的行映射 `previous_rect.y + (top - engine_pos)` 有 bug，依据是一个把 `previous_rect.y` 设成 `999` 的测试：结构代价 0.0000、颜色差 84.9，看起来是"读错行"。于是改成 `top - stitched.anchor_origin_y` 并提交（`4863a07`）。当天修拱形截图时发现：新公式在 prepend 过的画布上算出负行，颜色校验被静默跳过；而拱形的右腿因此永远拼不上。
- **Root cause**：`previous_rect.y` 与 `engine.pos` 不是两个独立变量——它们是**同一次 `commit` 的两个坐标**（像素行、逻辑行），生产代码保证成对一致。旧公式只用二者之差，所以在任何坐标系锚点下都成立。我用一个人为破坏该不变量的输入"证明"它错，等于证明了"如果输入不满足前提，公式就不成立"——这是重言式，不是缺陷。而 `anchor_origin_y` 的语义（已 prepend 的行数）与"像素行 0 对应的逻辑行"并不相同，替换后只在从未 prepend 的画布上等价。
- **Correction**：恢复旧公式，并把测试改成构造**一致的** `(previous_rect.y, pos)`：让引擎按 `commit(rect.y=0, requested_top=-60)` 的方式提交（正是拼接器 prepend 后做的事），再断言真值行颜色差 ~0、错行差异大。`stitch-legacy` 之外全绿。
- **Prevention**：
  1. 修改一个"不变量"之前，先写下它由谁维护、在哪些路径上成立。若只有测试能破坏它，那测试就是错的。
  2. 一个把输入设成**生产不可能产生**的值的测试，只能证明"实现依赖该输入"，不能证明"依赖是错的"。
  3. 换公式后必须跑**最不规则的**语料（这里是刚做完的拱形/粗糙画布路径），而不只是规则语料；规则语料上两种公式等价，测不出差异。
  4. 结论写进 `found.md` 时标明证据来源；本次已把 `4863a07` 的错误结论就地标注更正，而不是悄悄删掉。

### 26. 「证据不足」的判据写成了绝对行数，于是单分界线页面永远拼不上

- **Context**：用户要求支持「界面只有一条灰阶分界线」的长截图。实测：第二帧起全部 `Pending`。原因是 `cost_at` 的 `used >= EDGE_MIN_COMPARABLE_ROWS`（=3）：`used` 是「帧里带边界的行中，落在已填充画布行上的行数」。只有一条分界线的页面每帧恰好只有 1 行带边界，`used` 永远 ≤1，于是每个候选位置都返回 `None`，引擎连一个候选都没有。
- **Root cause**：把「证据够不够」写成了绝对计数，而帧能提供的证据量是由**内容**决定的。绝对计数在稀疏内容上变成不可能满足的条件，和判据想表达的意思（「这一行足够可信吗」）无关。
- **Correction**：改成 `used >= min(3, ladder.scoring_rows.len())` —— 不能要求一帧拿出比它自身更多的边界行。单分界线页面按 1 行判定；密集帧只与画布重叠一条缝时仍须拿出 3 行。
- **Prevention**：
  1. 任何「至少 N 个证据」的判据，先问「N 从哪来、内容能不能提供 N」。不能提供的输入不是坏输入。
  2. 不要把常数直接调小当作修复。先实测过：`EDGE_MIN_COMPARABLE_ROWS = 1` 让 real-1 从 204 接受/0 错位变成 201/6、20 条位移条带——被放进来的是「跳跃帧只有 1 行可比且恰好对上」的情况。把需求改成相对的，两个问题一起解决。
  3. 新用例的测试必须同时钉住**该拒绝的那一侧**（这里是「整帧纯灰」必须 `NoMatch`），否则放宽判据时没有东西拦住它。

### 27. 一维描述符 + 二维画布：跨轴之后，旧证据属于另一列

- **Context**：用户要求蛇形走位（右 → 下 → 左 → 下 → 右）在宽底图上拼出整张底图。实测第一版（可分离底图）通过；换成不可分离底图后，第一个「横向之后的纵向」步被拒，随后整条链 `Pending`。同一轮还发现第一个「向左」步被拒。
- **Root cause**：两处，都在「二维画布、一维证据」的接缝上。
  1. `EdgeCanvas` 是**一列**（`bins × rows`），`commit` 完全忽略 `rect.x`。第一帧那一列写下的行描述符会一直占据这些行，因为 `write` 跳过已填充行。横向滚动换了列，纵向步仍拿新列去比旧列 → 拒。
  2. 横向画布扫描（`score_canvas_overlap_with_rect`）在外接框内取样时不看 `row_coverage`。蛇形画布是参差的：向左步的重叠区包含尚未拍到的区域，那里是 0 字节；正确的落点因此被拿去和黑色比较，代价被抬高 → 拒。
- **Correction**：横向放置成功后 `engine.reset()` 再 `commit(rect, rect.y, frame)` —— 引擎的证据只描述当前列，列变了就必须从刚被横向匹配验证过的那一帧重建；横向打分跳过 `!stitched.captured(x, y)` 的取样点，与纵向颜色校验同一条规则。
- **Prevention**：
  1. 「不可分离」是合成底图的硬要求：如果页面是 `row(y) + col(x)`，`|dL/dy|` 在每一列都相同，纵向步会碰巧匹配上别的列 —— 测试会在缺少修复时**假绿**。本次前两版底图（可分离 / 只改相位）就是这样，分别掩盖了缺陷 1 的一半与全部。
  2. 每个修复都要单独回退验证。本次：回退 re-seed → 两个蛇形测试都在第 4 步失败；回退覆盖过滤 → 两个都在第 5 步失败。
  3. 画布一旦可能参差，任何「在重叠区取样」的代码都要先问「这个像素真的拍到过吗」。

### 28. 换轴兜底：判定者必须和主力同源，方向必须来自用户，可比区域必须只有已拍像素

- **Context**：用户定的规则：冲量方向决定先问哪一轴；那一轴判不出结果才问另一轴，另一轴**只跑预测级、没有锚窗**；两轴不比分；都不行就红框；没有重叠就不问。第一版把兜底接到旧的横向像素扫描上，`real-1` 直接崩掉（MISPLACED 238、链漂移 3623 px），蛇形与全部单测却全绿。
- **Root cause**：三件事，各自独立。
  1. **判定者不同源**。像素扫描唯一的终审是采样平均差 ≤ 12（移植来的粗筛阈值），没有边界描述符、没有 `used`、没有颜色校验。它当主力时有旁边的门替它挡；升格成「另一轴的判定者」后没有任何门兜底，大片平坦内容上任何 x 偏移都能得 0 分。
  2. **「某轴失败」没定义清楚**。纵向引擎会把横向移动答成 `delta 0`：结构代价 0.1172、`second/best = 2.72`（confident 逃逸）、颜色均值差 25.53 < 30。所以「冲量轴完全不行」不会自动发生。但那次的 25.53 是**测试底图**造成的（`col_band` 取值 50..200，相隔 320 px 的两列只差 25.5，颜色门在数学上无法区分）——改成 0..255 后，纯规则（不加任何「零位移不算答案」）直接通过两个蛇形测试。
  3. **可比区域取错**。横向描述符若按「搜索能到达的整条带」构建，参差画布上会包含从未拍到的列；那些零字节进入描述符，正确落点的代价被抬高，蛇形的第一个左步被拒（实测失败于第 5 步）。
- **Correction**：横向轴改成**同一套判据、按需构建**——`place_horizontally` 只为「上一帧自己的矩形」建描述符（那块像素必然已拍，因为上一帧写过），跑同一个 `decide`/`decide_from_costs`；只搜用户指明的那个方向；没有锚窗；不保留任何横向状态。纵向引擎在横向放置后仍然 re-seed（一维描述符 + 二维画布，见教训 27）。
- **实测**：`real-1/real-long/real-scroll/mini/pipeline-test/pipeline-bad/lowcontrast-2b` = 204/97/7/3/7/7/78，0 pending、0 no-match、0 错位、0 位移条带；`real-1` push 均值 1.33 ms。纯灰帧从「被接受成 delta 0」恢复为 `NoBoundaries` 拒绝，不需要任何特判。
- **Prevention**：
  1. 把一个匹配器升格去回答新问题之前，先列出它的全部门；缺的那个门就是它会答错的地方。更好的做法是让它和主力共用同一套判据，而不是给旧匹配器加门。
  2. 合成底图要同时满足：结构不可分离（`|dL/dy|` 逐列不同）**和**颜色可分离（跨列平均差显著大于颜色门 30）。只满足第一条时，测试会把一个真实的零位移接受读成「引擎正常」。
  3. 「可比区域」= 双方都确有内容的那块。参差画布上按外接框或按搜索范围取样，都会把未拍字节带进来；正确做法是取**上一帧自己的矩形**（必然已拍），重叠部分自然由 `cost_at` 的 `row_filled` 裁出。
  4. 每个修复单独回退验证：回退 re-seed → 两个蛇形都在第 4 步失败；关掉换轴兜底 → 都在第 4 步失败；把横向可比带换成整条搜索带 → 都在第 5 步失败。

---

## Verification Memory

- **A test that could not have failed is worse than no test (2026-10-22, 4863a07)**: cleaning up the engine tests uncovered both a live bug and four ways a test can be worthless. (a) `canvas_of`/`request` fabricated a pixel canvas holding row-texture rows 0..n regardless of what the test committed, and **nine tests** used it — they were running the colour check against content they had never written, so their verdicts meant nothing. (b) `request_on_canvas` trusted the caller's `previous_rect.y`, which a test could set to a value inconsistent with the canvas it had just built. (c) `prepending_rows_keeps_the_engine_in_logical_coordinates` asserted a third placement that, on a truthful canvas, has true row −34 against a prediction of −51 with both scoring alike under the row texture — it could only ever have passed because the canvas was fake. (d) Four test names referred to functions that no longer exist.

  **Correction (2026-10-22, later the same day)**: this entry originally claimed the cleanup had exposed a live bug in `colour_cost_at`'s row mapping (`previous_rect.y + (top - engine_pos)`) and that it had been replaced with `top - stitched.anchor_origin_y`. **That claim was wrong and the replacement was a regression.** The "proof" was a test that set `previous_rect.y = 999` — a pair `(previous_rect.y, pos)` that production cannot produce, because both values come from the same `commit`. With a consistent pair the original formula is exact in every coordinate space (it uses a *difference*, so a prepend or a geometry re-seed cannot break it), whereas `anchor_origin_y` is the count of prepended rows, not the logical row of pixel row 0, so the replacement read the wrong rows and silently disabled the colour check in prepend-heavy sessions. It was reverted while fixing the arch (see the next entry); the test now pins the mapping with a consistent pair. Prevention: (1) a test helper that invents the input it is supposed to observe turns the test into a tautology — build inputs from what the production path would have; (2) before keeping an assertion, ask what implementation would fail it, and verify by reverting the fix that it actually does (the first version of this very test passed against both formulas); (3) a test that only passes because the fixture is degenerate is not coverage.

- **Single-boundary (two-gray) pages stitch (2026-10-22)**: user request — a UI whose only feature is one dividing line between two flat grays. `used >= EDGE_MIN_COMPARABLE_ROWS` (3) made it unstitchable: such a frame has exactly one boundary row, so every candidate scored `None` and the second frame onward was `Pending`. Lowering the constant to 1 was measured and wrong (real-1 204 accepted / 0 misplaced → 201 / 6, 20 displaced bands) because jump frames whose single comparable row lines up were then accepted. The gate is now `used >= min(3, ladder.scoring_rows.len())`: relative to what the frame contains. Test `a_single_gray_boundary_is_enough_to_stitch` walks 520→540→560→580→599 with deltas 20/20/20/19 (the last frame keeps the boundary one row below its top), asserts the frame that starts exactly at the boundary is one flat gray and is refused as `NoMatch` after the patience, then scrolls back and asserts placement resumes (−19/−20/−20); it also checks the canvas rows are the page rows. Corpora exact: 204/97/7/3/7/7/78, 0 misplaced. Gated off under `stitch-legacy` (the old canvas scan has no boundary-row gate). Note: a *vertical* dividing line is invisible to a descriptor built from `|dL/dy|` and genuinely carries no vertical-offset information, so refusing it is correct, not a gap.
- **Local install 0.1.0-21 (2026-10-23)**: HEAD `270504b` (serpentine capture: engine re-seed across a column change + coverage-aware horizontal score) packaged as pkgrel 21, `pacman -U`; makepkg `check()` passed (172 lib + 193 bin, 1 ignored each); installed binary sha256 `88ea5cc5…` matches the package. Awaiting real-machine confirmation of a snake walk over a page wider than the viewport.
- **Serpentine capture works (2026-10-23)**: user request — walk a snake (right, down, left, down, right) over a page wider than the viewport and see whether the finished canvas is the page. Two defects, both at the seam between a two-dimensional pixel canvas and the one-column edge descriptor. (1) A horizontal placement left the engine's `EdgeCanvas` describing the previous column, because `commit` ignores `rect.x` and `write` keeps already-filled rows; the next vertical step then read the new column against the old one and was refused. A horizontal placement now re-seeds the engine (`reset`, then `commit` at the frame's own rect). (2) The horizontal canvas scan scored never-captured pixels inside the canvas bounding box (zero bytes) as content, so the first left step — whose overlap contains the not-yet-captured part of the snake — matched the correct placement against black and was refused; it now skips `!stitched.captured(x, y)`, the same rule the vertical colour check follows. Tests `a_snake_path_reconstructs_a_page_wider_than_the_viewport` (1600×1200 page, 640×480 viewport, 50% overlap, 16 frames) and `a_snake_path_works_on_a_wider_page_with_quarter_overlap` (1920×1200, 25% horizontal overlap, 16 frames) assert every step's delta and compare the finished canvas to the page pixel for pixel; reverting either fix fails both. Corpora exact: 204/97/7/3/7/7/78, 0 misplaced. The first two synthetic pages (a sum of a row term and a column term, then a phase-shifted checker) passed *without* the engine fix, because `|dL/dy|` was identical in every column — the final page adds a hashed 8×8 block texture so a column's vertical descriptor is unlike any other's.
- **Local install 0.1.0-20 (2026-10-22)**: HEAD `94335b1` (relative evidence gate) packaged as pkgrel 20, `pacman -U`; makepkg `check()` passed (170 lib + 191 bin, 1 ignored each); installed binary sha256 `a77f5efd…` matches the package. Awaiting real-machine confirmation of a page whose only feature is one gray dividing line.
- **Local install 0.1.0-19 (2026-10-22)**: HEAD `aac1b6c` (arch support + the colour-mapping revert) packaged with the same recipe, pkgrel 19, installed with `pacman -U`; makepkg `check()` passed (169 lib + 190 bin, 1 ignored each), installed binary sha256 `b9fa7ed6…` matches the package. Awaiting real-machine confirmation of the arch flow: scroll up on the left, widen at the top, move the box right, scroll down.
- **Arch-shaped capture (∩) now works (2026-10-22)**: user-reported — scroll up on the left, widen at the top, move the box to the right and scroll down, and only an L came out. Three defects, all in the ragged-canvas path. (1) `try_apply_match` skipped the append whenever the candidate rect fitted inside the canvas *bounding box*, so a frame landing in a never-captured region (the right leg) was dropped while being reported as `Accepted`; it now asks `viewport_rect_covered_by_stitched`, which consults the coverage map. (2) `colour_cost_at` sampled the pixel canvas at every row of the candidate, including never-captured bytes (zero), so it measured the frame's own brightness against black and refused the placement (`118.5` on the true row); it now skips samples the canvas did not capture. (3) `row_coverage` was a per-row *hull*, so the space between the legs was painted opaque black; it is now `RowCoverage::{Empty, Full, Spans}` with disjoint spans, keeping the common full row allocation-free (`Vec<Vec<..>>` cost ~13% of an accepted frame at 8 500 rows). Regression test `an_arch_keeps_the_gap_between_its_legs_transparent` fails on each of the three when reverted individually. real-1 204 / real-long 97 / lowcontrast-2b 78, all 0 misplaced; push timing unchanged (mean 1.34–1.42 ms, accepted 2.95–3.13 ms). Known limitation: `LegacyPipeline` (feature `stitch-legacy`, A/B only) never learned to place frames across a selection move, so the test is gated off there.
- **No configured radius left in placement (2026-10-22, c136d87)**: after the guard became `pos ± rows`, the ring ladder under it was dead weight — measured, every guard search opened exactly one ring (real-1 53 searches/53 rings, real-long 22/22, lowcontrast 11/11), because the configured base of 320 always exceeded the extent. The constant *was* the window. The guard now calls the ring scorer once with `inner = 0, outer = rows`; only the anchor walks a ladder, starting at one viewport (`rows`) and growing ×4. `EDGE_RING_BASE_HALF_WIDTH` is deleted; `EDGE_RING_GROWTH` survives as a shape, not a scale. Lesson: a ladder whose first rung is always wider than the range it searches is not a ladder — count the rings opened before believing a multi-stage design is doing anything.

- **Guard window is the frame height, and both directions are ordinary (2026-10-22, 5e65b3b)**: the user pointed out that the capture may be scrolling *back up*, and asked whether the windows were one-sided. They were not: the guard band is centred on the previous frame and the impulse window extrapolates a signed step, so real-1 — which is a scroll-up corpus (−8020 px total, 112 negative steps, 25 of 47 guard calls with a negative prediction) — exercises it throughout. But the window *size* was wrong: `pos ± (rows − minimum_overlap)` folds the evidence gate into the geometry, making the window a constant of the gate (228 on a 254-row viewport) and hiding positions that genuinely overlap. The user's rule is the right one: two frames of height `rows` overlap iff `|d| < rows`, so the window is `pos ± rows`, and evidence is required of the candidate's *score*, where it belongs. A test now walks a capture upward so the geometry cannot become one-sided. Also recorded: a full-viewport backward step leaves the two frames touching with zero rows in common (`[top, top+rows)` on both sides), so a test that does that is refused correctly — check the coordinates before believing a failure.

- **Guard made local, anchor gated on the red border (2026-10-22, e23b183)**: the user caught two semantic errors in the rebuild. (a) The guard scanned the whole canvas, which answers the anchor's question ("does this frame appear anywhere we have seen?") instead of its own ("is this the next step?"). A frame can only continue the capture where it still shares `minimum_overlap` rows with the **previous frame**, so the band is `pos ± (rows − minimum_overlap)` — 228 rows on a 254-row viewport regardless of canvas height. Scanning the canvas there also let a far match be accepted as if it were the next step. (b) The anchor ran on every frame whose neighbourhood failed, so a frame that merely failed to continue could be matched against unrelated content up the page and appended there. It now requires `refusals >= REFUSAL_PATIENCE`, shared with the UI rather than counted twice — recovery is a weaker claim than continuation. Both changes keep all seven corpora at baseline. Third correction: the stages re-*judged* overlapping positions (correct — each stage gates its own candidate set) but also re-*computed* them; a per-call `ScoreCache` reuses the score, 9.9% of lookups on real-1 (8.9% real-long, 7.0% lowcontrast-2b). Lesson: "check again" and "compute again" are different; the first can be required by the semantics, the second never is when the inputs are fixed.

- **Placement rebuilt around three searches (2026-10-22, 16cf8a7)**: the user rejected the accumulated stage soup — four stages whose reach overlapped, a 0.6-viewport displacement cap, and a refusal counter that widened it — with the demand that semantics be explicit and that the software work for a stated reason. The audit supported the complaint: the far ring accepted **0** frames on all seven corpora and could be deleted with every corpus and test unchanged; its reach was a subset of the anchor ladder's, whose cap (`rows − minimum_overlap`) is by definition every placeable position; on a 254-row viewport the old near ring (300) alone covered that whole range. What remains is three searches with distinct meanings: **impulse** (extrapolate the last step; radius `max(24, 1.05×|Δ|)`), **guard** (every row where `minimum_overlap` rows of evidence exist — extent from the canvas, so no tunable cap — walked outward from the prediction in symmetric rings), **anchor** (same reach, ordered from the last confirmed row). The guard's first ring starts at the impulse radius, so the stages partition the space. The anchor is *not* redundant: deleting it loses one real-long frame (top 2180 vs anchor 2120 vs prediction 2164), because order decides which acceptable offset wins. Deleted: `EDGE_WIDE_*`, `EDGE_MAX_DISPLACEMENT_*`, `EDGE_REFUSAL_WIDEN_STEPS`, `EDGE_ANCHOR_*`, `refusals_since_accept`, the in-gate guard filter, and the refusal-widening test. Cost: real-1 1.30 → 1.33 ms (+2%) because the guard now reaches the full canvas extent instead of stopping at 1400. Verified: seven corpora exact baseline, 161 lib + 180 bin tests × 3 feature combos. Lesson: a stage that cannot be described by what it can place, only by a constant, is a patch — measure whether deleting it changes anything before keeping it.

- **Impulse window sized by the next step; wide reach graded (2026-10-22, 8ccf0c2)**: the impulse half-width was 0.6× the last delta, chosen when 1.1× was measured to fail, and it under-covered the case it exists for — the window is meant to hold the *next* step, and an accelerating page moves further than the last one. Swept with the current gates: 0.60/0.80/1.00/1.05× all keep the corpora exact (204 accepts, 0 bands, drift −257) while moving 74→60→56→55 frames per corpus out of the wide scan; 1.10× gives 205 accepts but **2 displaced-band frames and drift −677**, because a window that wide starts catching jump frames whose prediction error is 150+ rows and deciding them on a candidate set too narrow to contain the wide stage's competitors. 1.05× landed. Its cost is neutral, not negative: with the graded wide stage the extra impulse positions cost about what the avoided wide scans save (measured 0.60→1.05: wide scans 74→54, total boundary cells 204.2M→195.3M, −4.4%; mean time flat at 1.28–1.36 ms across three runs each). The earlier 1.50→1.43 reading was taken *before* grading, when one avoided wide scan was worth 2801 positions instead of 601 — the widening bought a real 5% then, and buys nothing now. Recorded so the width is justified by correctness (the window must contain the next step), not by a speed claim that no longer reproduces. The wide stage was scoring ~1586 of 2801 positions per frame (the rest lack overlap) at 6.8M boundary cells each, while the accepted offset sat 67 rows from the prediction at the median — so it is now graded: near ring `max(300, 2×|last delta|)` then the rest to ±1400. The near ring decided **every** frame that reached the stage on all seven corpora (far-ring accepts: 0), cutting real-1 to 195M cells and **1.32 ms/frame**. The ring split rejected earlier lost 58 acceptances and added 13 misplaced writes because its bands were *fixed width*; a band scaled by the displacement still contains the page's competitors — that distinction is the whole design, and a unit test now locks the scaling. Residual risk: grading means a near-ring winner can be accepted while a cheaper true position sits in the far ring; no corpus shows it (far-ring accepts 0), but that is absence of evidence, and jumpy content whose step changes by more than 2× is where it would surface.

- **Review pass on the gate rewrite (2026-10-22, 68abd5c)**: self-review found six defects, all fixed. (a) `--features trace-logs` did not compile — the trace printed `second_over_best` above its binding, hidden by the trace macro discarding its tokens (lesson 23). (b) Two definitions of "as good" coexisted: ambiguity by ratio (`second/best < 1.5`) but rivals by absolute margin (`best + 0.02`), so a position 20x worse than a near-perfect winner counted as an equal answer; rivals are now the ≤1.5x set and `EDGE_RIVAL_COST_MARGIN` is gone. Measured on real-1: 9 of 279 windows had rivals and in all 9 the ratio was already <1.5, so the two never disagreed there — latent, not active. (c) The absolute gate ran after the colour tie-break, so 26 lowcontrast-2b windows paid for colour sampling and were then refused on cost; the cheap gate now runs first. (d) `Accept` reported `best_cost` even when the tie-break had placed the frame at a rival. (e) Colour and cost rejections were labelled `Ambiguous`, which now means only "the tie could not be resolved on pixels". (f) `colour_cost_at == None` silently skipped verification; traced now, and it never fires on any corpus. Gate margins measured for the record: real-1 accepts max winner cost 0.0548 (0 escapes), real-long 18 of 97 accepts used the margin escape (max 0.168), lowcontrast-2b max 0.0714 (12% under the 0.08 ceiling — the binding corpus). The tie-break path accepted 37 real-1 frames, 3 at an offset other than the structural winner, all verified correct by the replay's MISPLACED/band/coverage checks.

- **Median gate replaced by absolute + ratio + colour (2026-10-22, e1db844)**: the user drove a full redesign of the decision gates. `best/median` is gone (it measures the search window, not the content: one real-long frame read 0.731 in a ±36 window and 0.488 in ±1400 with the winner unchanged). What decides now, all window-independent: winner cost ceiling (0.08), runner-up ratio ≥2.5x forgiving a high winner (motion blur on real-long reaches 0.168 — below the ratio it must be refused, and `second=None` reads 0 so an empty window cannot claim the escape), and a colour verification of every acceptance against the pixel canvas (ceiling 30, between real motion 40.6 and an unrelated structural fake 82.2; the ladder is colour-blind and that blindness let fabricated content pass structurally at 0.073/1.82x). Ties (<1.5x) resolve by colour first, then nearest to the window centre; a cost-exactly-0 winner reads as infinitely ahead, which wide-narrow selections need. Two engine bugs fixed en route: the displacement guard dropped forbidden candidates instead of letting the first one veto the window (periodic content sorted a far offset first), and the colour check now maps columns through `previous_rect.x`. Engine tests build truthful pixel canvases (`request_on_canvas`, rebase-faithful) — the old `canvas_of` fabricated row-texture rows that the colour check rightly rejected. Calibration data: real-1 winners ≤0.0548 with 2 misplaced at 0.0355/0.0536 (indistinguishable by cost), synthetic fake 0.0729/1.82x/colour 7.3 at a small offset (only the 0.08 ceiling catches it). All seven corpora exact baseline, 159+180 tests ×3 feature combos, 1.53 ms/frame.



- **Session-gain fix for the per-frame gain defect (2026-10-21)**: the gain that rescues low-contrast content for the banded descriptor is now a session-wide constant (`EdgeLadder::session_gain`, decided by the frame that initialises the canvas, before its own ladder is built; every later `from_source` call takes it as a parameter). This removes the cross-frame scale mismatch without touching the banded structure: a high-contrast block scrolling into a low-contrast page saturates at level 2 on the session's scale instead of collapsing the low-contrast rows to level 0. The reproduction test from `b55235e` is un-ignored and passes. All seven corpora at exact baseline (real-1 **204**/0/0/0/0, real-long 97, lowcontrast-2b 78, others unchanged); 158 lib + 179 bin tests green under default, `trace-logs`, and `stitch-legacy`; clippy errors are the pre-existing `selection.rs` pair. Residual limitation, accepted: a session that *starts* on high-contrast content and scrolls into purely low-contrast content sees it as boundary-less (gain 1 throughout) and refuses those frames — a visible red border, never a wrong write.

- **2026-10-13 local install**: HEAD `ca24865` packaged from an isolated directory (`git archive` tarball + local-source PKGBUILD, pkgrel 11) and installed via `pacman -U`; makepkg `check()` passed with 133 lib + 153 bin tests (2 profiling tests ignored, as before). GitHub `origin/niri` still at `a8eb251` — the repo PKGBUILD (git source) would build outdated code until pushed.
- **2026-10-13 scope fix**: `a279527`（render_scope 限定选中输出）+ `3b09486`（移除重建兜底）双输出机器用户实测 3 次（404/55/183 帧、两块屏、零死锁）；精确根因见教训 13 最终版。`0.1.0-14`（trace-logs）已本地安装供用户回归验证，结果待回报。压力客户端 `/tmp/opencode/wl-stress` 与注入脚本 `/tmp/opencode/inject.py` 为 tmpfs 临时资产（见 Optimization Backlog 第 4 条）。
- **2026-10-13 long-capture deadlock root-caused and fixed** (see lesson 13): dual-output `--long` starvation traced via staged trace logging (prepare gates → release/commit accounting); final run on the dual-output machine: 178 commits, 173 releases, 30 frames captured and stitched, zero starvation. Smithay source confirms release is sent at commit-processing time (`InnerBuffer::drop`), exonerating niri.
- **2026-10-12 final state**: 133 lib + 153 bin tests pass (two profiling tests ignored); `cargo check --locked` and `cargo build --release --locked` pass. Two pre-existing clippy errors (`absurd_extreme_comparisons`) and two unused-import warnings in `cli.rs` exist at the prior commit and were not introduced by recent work.
- Release-binary smoke checks with Niri hints unset and desktop set to `sway` reached the deliberately unavailable Wayland connection instead of a compositor-name rejection.
- Wayland dispatch/compositor behavior (input grabbing, wheel blocking, passthrough toggling, live capture) remains manually testable only under a live session; unit tests cover the pure logic and pixel rendering.
- **DMS stitch comparison (2026-10-17)**: `docs/dms-stitch-comparison.md`；DMS v1.6.3 `stitch.go` 与本实现用同一 1920×1080 语料实测（1.10 ms/帧 vs 87.0 ms/帧），并复现 NoMatch 后错位接受、`FixedBands` 未接线、画布无上限三项待修问题；顶部粘性页眉失败已按产品决定列为 out of scope（非缺陷）。算法结论与缺陷优先级以该文档为准。
- **Edge-stitch experiment (2026-10-18)**: `docs/edge-stitch-design.md`；6 类 1920×20000 渲染语料（每帧真值精确已知）+ Rust 探针（实验后已删除）对比 shipped 与原型。最终配置 48 列边界阶梯 + separation ≥ 0.80 + 无绝对门限 + 无质量门限：600 帧口径 **0 静默**（shipped 254/480 帧）、216 红框（全部无重叠）、跟踪态单帧 1.8–2.3 ms（shipped 32.6–76.4 ms）。半行滚动 120/120 全对。三条结构性结论：边界盆地只有 1 行宽、粗筛短名单在恒定行高表格上召回 0–33%、门限不能惩罚「边界少」（稀疏页不是问题，删除质量门限后结果不变）。生产代码未改动。
- **Stitcher replacement landed (2026-10-18)**: `PlacementEngine` + `WindowedEdgeMatcher` (`src/stitch/engine.rs`), `LegacyPipeline` behind `stitch-legacy`, `ScriptedEngine` as the test mock. Same 480-frame corpus: silently misplaced 278 → 11, position jumps 205 → 2, per-frame 6.3 ms (tracking) / 4.2 ms (wheel). Red border + 5 fps while refusing. Two defects only surfaced after wiring: stale-edge recovery became a gate bypass (2 → 29 misplaced) and the dirty-edge scan cost 4.3 ms/frame while never hitting — see lessons 17 and 18.
- **Direction + DMS head-to-head (2026-10-18)**: `docs/stitch-direction-and-dms.md`；方向消融（H/V/H+V 在正常网页等价，双方向在 tallgrid σ=10 时把 9/9 稀释到 0/9）+ DMS 原样接入同 harness 对比（720 帧：DMS 90 次静默跳变 / 2.55 ms；本设计 2 次 / 13.4 ms / 覆盖率 58.6% vs 55.9%）。结论：保留单方向（横向）；DMS 的 `seamAppend` 不借鉴。生产代码未改动。
- **Red border recalibration (2026-10-18)**: real-browser corpus (headless Chrome, `Emulation.setDeviceMetricsOverride` 960x700 DPR 1, 98 scroll positions, `docs` page) replaced the synthetic one for threshold calibration. Old gates (`sep 0.80` / `bm 0.10`): 8 accepted / 89 refused / 0 misplaced. New gates (`sep 0.45` / `bm 0.28`): **97 accepted / 0 refused / 0 misplaced**, jumps still 0/12 wrongly accepted. Three defects found: thresholds calibrated on synthetic frames (see lesson 19), no per-frame displacement cap (630-row jumps accepted on 70 rows of evidence), and the runner-up definition measuring the winner's own shoulder instead of a rival peak (a 235-row card grid scored `sep = 1.000` while a candidate one period away matched at cost 0.000). Added `PushResult::Pending` with `REFUSAL_PATIENCE = 8` so a single ambiguous frame no longer turns the border red. Sub-pixel scrolling was investigated and **disproved**: at DPR 1 browsers quantise scroll positions to integer pixels.
- **Hyprland findings (2026-10-12)**: `docs/hyprland-findings.md` records three Hyprland-side problems (wheel passthrough, missing cursor in cursor-requested frames, cursor not removable from region frames), each with triggers, upstream evidence, and what remains separately fixable on our side. Mechanisms are source-verified; none reproduced end-to-end on a live Hyprland session yet.
- **Acceptance criteria hardened (2026-10-20)**: `replay-frames` gained coverage (no canvas row uncovered), band (no accepted frame's content displaced from its own top), chain-drift, and corpus-adaptive pairwise-outlier criteria; oracle/locate parallelised (real-1: 30 min → ~5 s). All six corpora clean — real-1: **204 accepted / 0 no-match / 0 misplaced / 0 uncovered / 0 displaced bands**. Verdict: jumpy scroll with heavy duplicates is handled correctly by the engine; no stitcher change made or needed (see lesson 21). Commits `e125c33`, `a749424`, `b618723`.
- **DMS head-to-head on real frames (2026-10-20)**: `docs/dms-stitch-comparison.md` §15, harness in `tools/dms-cmp/` (DMS source fetched at build time, gitignored). Same real-1 frames, same machine: DMS 0.123 ms/frame vs ours 1.82 (mean), but DMS output = 55% of expected height, 11 invisible seams, 30 whole-frame misplaced writes (+40~+280 rows), 72 silent drops; ours 0/0/0 with full coverage. Both sides far under the 33 ms budget — correctness, not speed, is the differentiator on real wheel-scrolled content. Known gap of ours: full-canvas realloc+copy per accepted append (≈16.5 MB/frame at 8.5k rows) — Optimization Backlog candidate.
- **Local install of the fixed stitcher (2026-10-20)**: HEAD `2271704` packaged via the established recipe (git-archive tarball → isolated dir → local-source PKGBUILD `screenshot-niri-local 0.1.0-15`, `/tmp/opencode/pkg-local/`) and installed with `pacman -U`, upgrading `0.1.0-2` (which predated the coordinate-bug fixes). makepkg `check()` passed (cargo test --release in the isolated tree); installed `/usr/bin/screenshot` hash matches the packaged binary. This build carries: the two coordinate/guard fixes (`b4f8fd4`), the acceptance criteria + parallel replay, the DMS comparison tooling. GitHub `origin/niri` still trails local HEAD, so the repo PKGBUILD (git source) keeps building outdated code until pushed.
- **Overlay UX fixes (2026-10-20)**: four user-reported issues fixed. (1) The frozen render path hardcoded the white border in `overlay_pixel`, so a drag spanning outputs never turned the warning colour; the colour is now threaded through `draw_overlay{,_region}` from the same `border_color_for_state` decision the live path uses, and spans warn live during the drag (white again once released in normal mode — a settled spanning selection is valid because capture composites per-output pieces). (2) Warning colour is now white-red `[128,128,255]` (BGRA) everywhere instead of pure red (user choice: solid blend, not stripes). (3) `PromoteLong` calls `drop_frozen_bases` on the key press: frozen outputs cleared + live render, so the real screen shows immediately instead of after session handover. (4) Edit-drag hole is the current box ∪ in-flight region, not the drag-trajectory union: shrinking now moves the dim edge with the border (enlarging was already continuous because the union grew with the box). `edit_union` removed; a stale pending region is dropped during edits (`drop_invalid_pending`) since that frame is already discarded by sticky invalidation. 155 lib + 176 bin tests pass.
- **Local install 0.1.0-16 (2026-10-20)**: HEAD `d1ff6d2` (the four overlay UX fixes) packaged with the same recipe (git-archive → isolated dir → local PKGBUILD, pkgrel 16) and installed with `pacman -U`; makepkg `check()` passed and the installed binary hash matches the packaged one. Awaiting real-machine confirmation of the four behaviours: live white-red border when a drag crosses outputs, white-red warnings everywhere, frozen image withdrawn on the `L` press, and smooth shrink in edit mode.
- **Normal-mode spanning selections are valid (2026-10-22)**: user-reported bug — normal mode flagged a selection that crossed outputs with the white-red warning border. Normal mode composites per-output pieces (`SelectedViewport::from_outputs` → `composite_captured_region`), so spanning is a supported capture and never needed a warning; only long mode cannot span. `border_color_for` lost its `drag`/`edit_drag` parameters and now warns on `spans && long_mode` (plus the independent `stitch_warning`), so a normal-mode spanning selection stays white during the drag, after release, and in edit mode. The long-mode clamp paths (`PromoteLong`, long-mode release) are untouched. See lesson 24 for why the warning existed at all.
- **Local install 0.1.0-18 (2026-10-22)**: HEAD `e551fef` (normal-mode spanning selections no longer warn) packaged with the same recipe, pkgrel 18, installed with `pacman -U`; makepkg `check()` passed (168 lib + 189 bin), installed binary sha256 `7def6148…` matches the package. Awaiting real-machine confirmation that a normal-mode drag across outputs keeps the plain white border throughout.
- **Local install 0.1.0-17 (2026-10-22)**: HEAD `6f08b20`（颜色校验行映射修复 + 引擎测试去伪）packaged with the same recipe (git-archive tarball → isolated `/tmp/opencode/pkg-local/` → local PKGBUILD, pkgrel 17) and installed with `pacman -U`, upgrading `0.1.0-16`. makepkg `check()` passed (168 lib + 189 bin tests, 1 ignored each); installed `/usr/bin/screenshot` sha256 `921bbab3…` matches the packaged binary; `--version` reports 0.1.0. Awaiting real-machine confirmation of long capture with the corrected colour check — the failure mode it fixes is a *silent refusal* after a prepend, so the visible symptom would be a red border appearing mid-scroll where it previously did not.
- **Low-contrast corpus and fixes (2026-10-20)**: `testdata/frames/lowcontrast-2b` (generated by `testdata/frames/gen-lowcontrast.py`: alternating grays 128/130 — **2 levels apart** — random run lengths 12–48 px, so non-periodic and no long flat block). Initially 0 accepted / empty canvas. Two absolute thresholds were at fault: near-duplicate detection used a fixed 2.0 mean difference (a real 60 px scroll on 2-level content differs by ~1.0, so every moving frame was dropped), and boundary quantisation used absolute levels 8/26 (all boundaries fell to level 0, so every frame was refused). Fixed by making the duplicate threshold relative to the frame's luminance range (10%, clamped to the old absolute value) and, for frames whose peak boundary never reaches level 1, scaling deltas by their own peak. Two gates also needed correcting: `EDGE_MIN_COMPARABLE_ROWS` 8 → 3 (stripe content has boundaries only at run edges; **not 1** — motion-blurred real-1 frames produce a single lucky row and reintroduced 8 displaced-band frames), and runner-up/rival selection now requires comparable evidence (¾ of the winner's boundary rows) so a one-row coincidence is not treated as a rival while a true periodic decoy still refuses. Result: 78 accepted / 0 no-match / 0 misplaced / 0 uncovered (canvas 6620 rows vs truth 6285); delta 1 refuses instead of guessing (47/40/0). real-1 unchanged 204/0/0/0/0; other five corpora unchanged. Commit `137f174`.

---

## Optimization Backlog（未排期优化方向）

2026-10-13 死锁排查会话中识别，均为内存/性能优化，不影响正确性。动手前先读教训 13 的 Prevention。

1. **遮罩条带化**（收益最大）：全屏 overlay 画布 ×3 → 框外四条带 surface（洞区无 surface），预览可独立为框内小 surface。内存 ~33MB/输出(1080p) → 周长级 <1MB；4K 双输出 ~200MB → 个位数 MB。代价：surface 数量×输出数、configure/resize、输入区域管理全面复杂化。触发条件：4K/多输出内存成为实际顾虑。
2. **wl_shm → dmabuf/GPU 路径**：GPU 渲染画布 + 合成器零拷贝，CPU 占用归零。代价：EGL/GBM 依赖栈与驱动兼容矩阵，违背最小依赖原则。当前负载（简单填充、每秒次数个位数）CPU 成本可忽略，仅当捕获会话 CPU 实测成为问题时考虑。
3. **overlay 缓冲 3→2**（收益最小）：修复后缓冲数已非正确性参数（乒乓已断、prepare 不依赖提交），2 块仅在突发时偶发推迟一次重画。省 8.3MB/输出。低优先级。
4. **诊断资产去留**：压力客户端（wl-stress，建立健康基线的差分工具）与 uinput 注入脚本当前在 tmpfs 上，重启即失；是否收编入仓库 tools/ 由项目主决定。
5. **接受路径画布重分配**（2026-10-20 DMS 对比暴露）：`append_frame_at_position` 每次接受都新建全画布缓冲并整体拷贝（8534 行 × 644 宽 × 3B ≈ 16.5 MB/帧，末段占接受均值 3.99 ms 的大头）。改为容量预留 + 单次扩容（Vec amortized growth）或分段行存储可把拷贝摊到 O(新增行)。触发条件：接受帧耗时成为 30 fps 瓶颈（当前最差 9.67 ms，占预算 29%，不紧迫）。
