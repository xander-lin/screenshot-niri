# DMS 拼接算法 vs 本实现：源码级对比与同输入实测

- 日期：2026-10-17
- 对照对象：DankMaterialShell（DMS）`dms screenshot scroll` 的拼接器
- DMS 基线：**v1.6.3**（本机 `dms-shell 1.6.3-2`）源码
  - `core/internal/screenshot/stitch.go`（407 行，拼接器本体）
  - `core/internal/screenshot/scroll.go`（会话策略：节流、重复帧、seam）
  - `core/internal/screenshot/region_render.go`（实时预览绘制）
  - `core/internal/screenshot/stitch_test.go`、`scroll_sim_test.go`（测试）
- 本实现基线：HEAD `a279527` 工作树
  - `src/stitch.rs`（3987 行）+ `src/stitch/{canvas,placement,perceptual,hash,motion,fixed_bands,rgb,types}.rs`（合计 7013 行）
  - `src/app.rs`（长截图编排）、`src/wayland/selection.rs`（overlay 与预览）

证据等级沿用 `docs/hyprland-findings.md` 的约定：**[已核实]** 有源码行号或实测数据；**[推断]** 由代码路径推导未实测；**[待实测]** 机制明确但未在真机会话复现。

> 说明：`stitch.go` 在 DMS master 与 v1.6.3 逐字节相同（已 diff）；`scroll.go` master 多出 Hyprland Lua 键盘转发等与拼接无关的改动。本文算法结论以 v1.6.3 为准。
> 实测用的 Go 复现与像素语料放在 `/tmp/opencode/`（tmpfs，重启即失，未入库）。
> **范围声明**：粘性页眉/页脚的补偿不是本实现目标（`AGENTS.md` → Out of scope）。§4.1 的数据只解释判据差异，不构成待办项。
> 后续：针对本文 §6 的 NoMatch 错位接受，另做了「边界位置 + 方向冲量 + 显式失败态」的机制实验，见 `docs/edge-stitch-design.md`（480 帧同语料：静默错位 254 → 0，单帧 50.9 → 11.0 ms）。
> **再后续（2026-10-20）**：生产引擎已在真实 screencopy 语料上与 DMS 正面对比，见 §15——DMS 快 ~15×（0.123 vs 1.82 ms/帧）但输出只有一半高度、11 处隐形接缝、30 帧整帧错位、72 帧静默丢失；本实现全部正确性判据干净。下表耗时行是旧匹配器的历史数字。

---

## 1. 结论速览

| 维度 | DMS | 本实现 | 判定 |
|---|---|---|---|
| 规模 | 407 行单文件，1 个类 | 7013 行、9 个模块，多阶段流水线 | 取向不同：DMS 极简，本实现覆盖 T/L 形与几何编辑 |
| 匹配表示 | 每行 3 个亮度均值（水平三带各 17 采样） | 全宽逐像素 RGB（步长 12→2）+ 64-bin 亮度带签名 + 模糊哈希 | DMS 快 1–3 个数量级（随画布变长） |
| 接受判据 | 整段平均 ≤9.0 + 活动行 ≥12 + 上下忽略带 | 任一行平均 >3.0 即拒（更严），无忽略带 | 判据形状不同；粘性 chrome 不在本实现目标内（见 §4.1） |
| 方向 | 只支持垂直，双向搜索 | 垂直+水平，可用箭头提示 | 本实现更强 |
| 跳跃（超出重叠） | 连续 4 次静止后 `seamAppend` 保留新段 | **无 seam 路径**；且 NoMatch 后可能错位接受 | 本实现有实质缺陷 |
| 粘性页眉/页脚 | 靠忽略带部分掩盖 | 不处理（设计取舍） | 非目标：选区含页眉属用户选区错误（见 §4.1） |
| 画布上限 | 256 MiB / 30000 行，满后停采 | 无上限（仅 i32 坐标约束） | 本实现有 OOM 风险 |
| 预览成本 | 常量级（下采样到 ≤240×360 面板） | 每帧全画布 RGB→RGBA（随画布线性增长） | DMS 更优 |
| 选区几何编辑 / T-L 形 | 无 | 有（E 模式、透明角、宽窄往返） | 本实现独有 |
| 单帧耗时（1080p，同语料） | 平均 **1.10 ms**，最差 9.0 ms | 平均 **87.0 ms**；NoMatch 帧 **0.8–1.5 s** | 本实现需优化 |

---

## 2. 数据表示与签名

### DMS

`rowCols [3]float32`，每行只保留**三个水平带**的平均亮度（`stitch.go:34-35`）。采样点由 `newStitcher` 预生成：带区间 8–32% / 34–66% / 68–92%，每带 17 点（`stitch.go:20,58-67`）。即"左右各 8% 边缘不参与"（侧栏/滚动条），中间三带足以区分文本行。

- 每帧签名：18×24 网格亮度，432 个 `float32`（`stitch.go:25-26,86-99`），仅用于重复帧判断。
- 画布保存 **RGBA 原始帧字节**（`newStitcher(s.frameW * 4)`，`scroll.go:544`），预览直接取用。

### 本实现

- 画布是 **RGB24**（`src/stitch/types.rs:26-44`，`stride = width*3`），内存比 DMS 少 25%。
- 匹配用 `RgbSource` 逐像素 RGB（`src/stitch/rgb.rs:16-21`）；canvas 打分先 12 像素步长粗筛、再 2 像素确认（`src/stitch/placement.rs:10-13,637-659`）。
- 额外维护：`PerceptualFrame` 全帧亮度（`src/stitch/perceptual.rs:20-42`）、64-bin × 8 行滑窗签名（同文件 474-537）、模糊哈希两级级联（`src/stitch/hash.rs:183-201`）、`FixedBandDetector`、fast-motion 候选（`src/stitch/motion.rs`）。

**代价对比**：我们每条候选位置都要读整幅重叠区的 RGB 像素；DMS 一次比较只读 `3 × 行数` 个 float32。这是后文 79×（短画布）到 1300×（12k 行画布回访）差距的根源。

---

## 3. 定位流程

### 3.1 DMS：预测 → 近扫 → 全扫

`locateFrame`（`stitch.go:187-201`）：

1. `adjacentOffset`：以 `lastOffset` 为中心向两侧按 1px 扩张，找相对**上一帧**的位移。差值 <0.25 立即返回；差值 <1.0 时启动 10 步倒计时，倒计时归零即停（`stitch.go:211-253`）。因为只有行维，水平/垂直无需区分。
2. 由 `pred = anchor + d` 给出候选位置，`verifyAt(pred)`（`stitch.go:203-209`）。
3. `scanPositions(nearOnly=true)`：`pred ± 160` 加上画布两端（`stitch.go:330-350`）。
4. `scanPositions(nearOnly=false)`：步长 8 全画布粗扫，再在胜者 ±8 内 1px 精修（`stitch.go:352-363`）。

全部失败 → `pushFrame` 返回 `(0,false)`，**不改任何状态**（`stitch.go:152-155`）。

### 3.2 本实现：canvas → perceptual → fallback

`DefaultPlacementPipeline::place`（`src/stitch.rs:168-307`）严格按优先级叠加候选：

1. **canvas 精确放置**：以 `previous_rect` 为局部中心，在 [32, 64, 128, 192] 四个半径环内找，分数足够好且有 1.0 分差就接受；否则步长 24 全画布粗扫取前 12 名，每名 ±24 精修（`src/stitch/placement.rs:89-269`）。分数 = 重叠区 RGB 平均差，`>12` 直接放弃（`src/stitch/placement.rs:511-514`）。
2. **perceptual**：用 8 行带、64-bin 的亮度签名做全偏移（±150）搜索，取 `median/p75/p90/mean` 四重门槛，并要求比"零位移"和"非相邻次优"各好 0.5（`src/stitch/perceptual.rs:192-266`）。
3. **previous-frame fallback**：`find_frame_shift_match_source` 对上一帧做逐 shift 全宽搜索；每行平均差 >3.0 即否决该 shift（`src/stitch.rs:1053-1088,1921-1980`）。
4. 仍失败则尝试两种恢复：`find_dirty_overlap_recovery`（重叠区"脏带"重写）与 `find_nomatch_recovery`（陈旧边缘替换，要求 `stable_rows ≥ 8`、`dirty_rows ≤ 192` 且 ≤35%）（`src/stitch.rs:752-813,1602-1611`）。

两者结构同构（预测 + 局部 + 全局 + 验证），但**验收函数的严格度差别很大**（下一节）。

---

## 4. 接受判据（鲁棒性差异的根源）

### DMS

- `verifyAt`：`diff <= 9.0`（整段**平均**亮度差）、比较行数 `>= 100`、活动行匹配数 `>= 12`（`stitch.go:203-209`）。
- `pairDiff`/`canvasDiff` 直接剔除顶部 10%、底部 8%（最少 16px，最多 h/4）（`stitch.go:123-128,255-299`）。
- 活动行定义：与上一行亮度差 >2.0 的行（`stitch.go:130-136`），纯空白行不参与决策（注释 `stitch.go:28`）。这正是它能在全白/重复卡片页上保持正确的机制。

### 本实现

- canvas 路线：重叠区**平均** RGB 差 ≤12 才进入候选（`placement.rs:511-514`），但最终 `confirm_vertical_shift` 要求**任意一行**平均差 ≤3.0（`src/stitch.rs:1921-1947`）。一行页眉不同就整体否决。
- 没有"忽略上下矩形"的概念；`matchIgnores` 的等价物只有 `FixedBands`，而它没有接入生产路径（`src/stitch.rs:645-654`）；按 §4.1 的取舍，它也不应接入。
- 空白行没有特殊排除：`average_difference_same_source` 对相同白行给出 0 分，反而会奖励"全白对齐"。

### 4.1 粘性页眉/页脚：明确的非目标

按项目决定（`AGENTS.md` → Out of scope），**选区里含粘性页眉/页脚属于用户选区错误**，拼接器不检测、不裁剪、不兜底。理由是这类补偿必须靠启发式猜测"哪几行是 chrome"，判错会把正常内容当成固定条而永久丢弃，代价高于收益；正确做法是选区时用 E 模式把固定条排除在外。

因此下面两组数据**只用于说明两套判据的形状差异，不构成待办项**。保留它们的价值是：解释为什么某些页面组合会失败，以及为什么"平均差 + 忽略带"这类改动会连带改变页脚行为（不是单独修好页眉）。

**同输入实测（1920×1080，文本页，粘性顶部 chrome 高度扫描，10 帧从 0 滚到 1080）**

| 顶部 chrome | 占帧高 | DMS 接受 | 本实现接受 |
|---|---|---|---|
| 108 px | 10.0% | 9/9 | 9/9（高度正确） |
| 130 px | 12.0% | 9/9 | 2/9（高度 1320，应为 2160） |
| 150 px | 13.9% | 9/9 | **0/9（完全卡死）** |
| 200 px | 18.5% | 9/9 | 0/9 |
| 300 px | 27.8% | 8/9（高度 2040，少 120） | 4/9（高度 1560） |
| 432 px | 40.0% | 7/9（高度 1920） | 5/9（高度 1680） |

**底部 chrome**（同一语料）结论相反：

| 底部 chrome | DMS | 本实现 |
|---|---|---|
| 43 px (4%) | 5/9，高度 1680 | 9/9，高度 2160（正确） |
| 86 px (8%) | 3/9，高度 1440 | 9/9，高度 2160（正确） |
| 108 px (10%) | 2/9，高度 1320 | 9/9，高度 **3132**（重复内容，错） |
| 200 px (18.5%) | 4/9，高度 2440（错） | 8/9，高度 3040（错） |

机制（**[推断]**，由代码路径推导，未逐帧打点）：

- 我们顶部失败：`confirm_vertical_shift` 逐行要求平均差 ≤3.0（`src/stitch.rs:1941-1943`），固定页眉与画布同位置的旧内容不同，必然有一大批行超标 → 整帧否决。
- DMS 顶部通过：`matchIgnores` 直接剔除顶部 10%（1080 帧为 108px，`stitch.go:123-128`），剩下的比较区域是滚动内容；同时"活动行 ≥12"避免了空白行的假匹配。
- 我们底部更耐受：`append_frame_at_position` 保留旧像素，只写旧画布未覆盖的条带（`src/stitch/canvas.rs:220-248`），页脚只污染新追加的尾部，下一帧仍有大段重叠可比对。
- DMS 底部更脆弱：`pushFrame` 把 `h-over` 行的帧尾部**整段写入画布**（`stitch.go:159-161`），页脚直接进入画布底部，下一帧的对应行就不再匹配；其底部忽略区只有 8%（86px），比顶部更窄。

> 结论：两者都没有真正"识别并裁剪"粘性 chrome，只是判据形状不同导致强弱互补。这**不是**待修的缺陷：项目已明确把粘性页眉页脚补偿列为 out of scope，正确做法是选区时用 E 模式把固定条排除在采集区之外——这是我们已有、DMS 没有的能力。

---

## 5. 提交与画布增长

### DMS

- 只提交**超出画布的部分**：`appendRows(frame, f, h-over)`（`stitch.go:159-161,365-379`）；向上滚动 `prependRows` 整段前置拷贝（`stitch.go:162-166,381-400`）。
- `append` 到 Go slice 为均摊 O(1)；`prepend` 为 O(canvas)。
- 上限：`stitchMaxCanvasBytes = 256 MiB`、`stitchMaxRowsCap = 30000`（`stitch.go:9-10`），超出置 `full`；会话层看到 `full` 就停止采集（`scroll.go:121`），预览与 UI 保持可用。
- 无 T/L 形概念：宽度固定，行覆盖恒为全宽。

### 本实现

- 每次放置**新建完整画布**并 blit 旧内容（`src/stitch/canvas.rs:166-209`），代价 O(canvas)/帧；RGB24 比 DMS 的 RGBA 省 25%。
- 支持参差画布：`row_coverage` 记录每行实际覆盖区间，未捕获区在导出时保持透明（`src/stitch/canvas.rs:43-79,172-199`），T/L 形与宽窄往返因此成立。
- **没有字节/行上限**（`src/stitch/canvas.rs:130-137` 只检查 i32 坐标）。
  - 实测 1920×12360 画布 = **71.2 MiB** RGB；同尺寸继续输出预览还要再分配 1920×4×12360 ≈ **94.9 MiB** RGBA，且旧 Vec 在下一帧前才释放，峰值接近 2×。
  - DMS 在 256 MiB 时主动止损；我们只受物理内存约束。

---

## 6. 跳跃、seam 与方向

### 6.1 DMS 的 seam 策略

会话层（`scroll.go:557-581`）：

- 非重复帧 → `pushFrame`；失败记 `unmatched`。
- 重复帧且 `unmatched` → 计数；连续 `scrollSeamTicks = 4` 次（`scroll.go:27`）后，再试 `pushFrame`，仍失败则 `seamAppend` **无条件把整帧追加为新段**（`stitch.go:174-185`）。
- 重复帧且上一帧已放置 → 直接丢弃（静止内容不重复写）。

含义：内容跳得再远，**最坏情况是输出里出现一条接缝，而不是后续全部丢失**。实测（1024×720 帧，从 160 直接跳到 3000）：

```
DMS: 0→720, 80→800, 160→880, 3000(NoMatch)×4 → 第 5 次 seamAppend → 1600
     随后 3080/3160/3240 正常继续 → 1840
```

### 6.2 本实现：无 seam

`grep -rn "seam" src/` 只有 `src/app.rs:46` 的一条注释，没有任何 seam 拼接路径。`PushResult::NoMatch` 的语义是"丢弃本帧且不改变画布"（`src/stitch.rs:744-749`）。

实测（同 1024×720 语料，从 160 跳到 3000）：

```
ours: 0→720, 80→800, 160→880, 3000/3080/3160/3240/3320 全部 NoMatch → 永远停在 880
```

用户快速翻页（Page Down、点"下一页"、切标签页）后，**后续所有滚动都不再被记录**，只能结束或退回重叠区。这与 DMS 的行为差距是结构性的。

### 6.3 更隐蔽的问题：NoMatch 后推进参考帧导致错位接受

`push_frame_sources` 在返回 `NoMatch` 之前仍然更新 `previous_compose_frame` 与 `previous_perceptual_frame`（`src/stitch.rs:744-747`），但 `viewport_rect` 保持原值。下一帧若走 perceptual/fallback，其位移是**相对失败帧**算出的，却被应用到**旧 viewport** 上：

```rust
// src/stitch.rs:844
let candidate_rect = translate_viewport_rect(previous_rect, match_info)?;
```

`append_frame_at_position` 保留旧像素、只写非覆盖区（`src/stitch/canvas.rs:220-248`），于是输出"看起来连续、实际缺一大段"。

实测复现（200 行帧，页面 4000 行，序列 0,40,80,400,440,480）：

```
top=   0  Initialized            canvas 200
top=  40  Accepted dy=40         canvas 240
top=  80  Accepted dy=40         canvas 280
top= 400  NoMatch                canvas 280
top= 440  Accepted dy=40         canvas 320   ← 相对 400 帧的位移，叠在旧 viewport(=页面 80) 上
top= 480  Accepted dy=40         canvas 360
逐 20 行映射（画布行 → 页面行）:
  [0, 20, 40, 60, 80, ..., 240, 600, 640, ...]
```

画布 280 行之前的内容与页面 0..280 一致，之后直接跳到页面 600..680：**页面 280..600 共 320 行被静默丢弃**，而画布高度只增加了 80 行。DMS 在失败帧上不推进 `last`（`stitch.go:152-155`），因此不会出现该错位；它会把无法验证的帧整体丢弃或整体 seam。

> 这是本次对比中最值得优先处理的正确性问题。

### 6.4 方向

- DMS：只有垂直，`adjacentOffset` 同时向正负两侧搜索（`stitch.go:211-253`），因此向上滚动天然支持，无需任何提示。实测 500→420→340→260：画布 440 行，逐 40 行映射为 260..700，正确。
- 本实现：垂直 + 水平（`SearchAxis`、`try_horizontal_shift`），可用 ↑↓←→ 提示（`src/wayland/selection.rs:1166-1169`）；无提示时垂直双向、水平禁用（`src/stitch.rs:1060-1066`）。
- 实测：本实现向上滚动（500→180）无论是否按 Up，结果完全相同（520 行，映射 180..660 正确），与 DMS 一致；水平维度是 DMS 完全没有的能力（L 形截图依赖它）。

---

## 7. 重复帧判定

| | DMS | 本实现 |
|---|---|---|
| 签名 | 18×24 亮度网格（432 点） | app 层：每 8 像素 1 点、3 通道平均差 <0.5（`src/app.rs:496-523`）；stitch 层：每 4 像素 RGB 平均差 ≤2.0（`src/stitch.rs:589-606`） |
| 判据 | 平均差 ≤1.1 **且**最大差 ≤4（`stitch.go:22-24,109-120`） | 平均差 <0.5 / ≤2.0 |
| 额外语义 | `dup && prevPlaced` 直接丢弃，不重新提交；`dup && unmatched` 用于 seam 计数 | `Duplicate` 提前返回，不推进任何参考帧；app 层还会先做一次原始帧近重复过滤 |
| 实测成本 | 可忽略（签名 432 点） | 0.2–0.3 ms（1080p，每帧） |

两者都很便宜；DMS 多了"最大差"约束，能避免"整体接近但局部大改"被误判为重复，我们只有均值。

---

## 8. 预览与内存

### DMS

- 面板在选区左/右空隙中自动择位，尺寸上限 240×360 逻辑像素（`scroll.go:224-302`）。
- 只显示画布**尾部** `previewRows`（`startRow = totalRows - maxPreviewRows`，`scroll.go:268-278`），双线性近似下采样（`region_render.go:518-597`）。
- 每帧成本与画布长度无关，恒定 O(面板像素) ≈ 8.6 万像素。
- 只有 `added > 0` 才重绘（`scroll.go:587-594`），重复帧零渲染。

### 本实现

- 预览是"所见即所得"地叠加在选区外：把整幅 stitched 画布按 `current_origin` / `viewport_rect` 映射回屏幕像素（`src/wayland/selection.rs:1668-1740`），跳过选区内部和透明角。
- 每个 `Accepted` 帧都调用 `image_from_stitched_frame` 做**全画布** RGB→RGBA 转换（`src/app.rs:387`，`src/stitch/canvas.rs:43-79`）：
  - 实测 1024×12000：36.9 MiB RGB → 49.2 MiB RGBA，**13.8 ms/帧**。
  - 按同公式，1920×12360 约 71 → 95 MiB、约 26–28 ms/帧。
- 之后 `update_long_capture_preview` 递增 seq 并排队重绘（`selection.rs:797-806`）。

这是设计取向差异（我们的预览贴在屏幕上更直观，DMS 是侧栏缩略图），但**成本随画布线性增长**，在长页面后段与拼接器抢同一颗 CPU，形成第二个掉帧来源。

---

## 9. 同输入实测（本次对比的核心数据）

方法：Rust 生成一份 1920×1080 页面语料（16–40 行文本块 + 10–30 行空白间隔），写成 RGB 文件；Go 侧读入并扩成 RGBA（DMS 的原生布局）。两边跑**同一批帧、同一顺序**。DMS 用 v1.6.3 原始 `stitch.go`（只补了 `clamp`）。

| 场景 | DMS | 本实现 |
|---|---|---|
| 48 帧顺序向下（画布 12360 行） | **53 ms 总计 / 1.10 ms 平均 / 9.0 ms 最差** | 4.18 s 总计 / **87.0 ms 平均** |
| 同批 48 帧回访（画布已 12.4k 行） | 平均 **1.36 ms** / 最差 40.5 ms | 平均 **≈1814 ms** / 最差 4215 ms（总计 87 s） |
| 画布 7080 行时的尾段 22 帧 | 不适用（恒定） | 平均 87.8 ms，**22/22 超过 33 ms**（30 fps 预算） |
| 单帧 NoMatch（画布 2k–12k 行） | 不适用 | **0.8–1.5 s** |
| 正确性（0→400，步长 40，240 行帧） | 画布 640 行，逐 20 行映射与页面一致 | 640 行，映射与 DMS 逐点相同 |
| 重复卡片页（140 行周期） | 640 行，正确 | 640 行，正确 |
| 稀疏文本页（全白 + 每 100 行 20 行文字） | 600 行，正确 | 600 行，正确 |
| 大量空白（全白页） | 240 行后全部 `placed=false`（拒绝在空白上"猜"位移） | 240 行后全部 `Duplicate`（同样不增长） |

性能归因（本实现，canvas 12360 行，perf + 内置 profile）：

- `rgb_row_average_difference` 35–65%，`CanvasScoreCache::score` 25–47% —— 即"逐行 RGB 比较 + 候选打分"。
- 单帧 57.6 ms 的 `CanvasAccepted` 中：canvas 扫描 35.5 ms，apply/append 10.4 ms。
- 顺序采集时每帧 `append` 1.6–2.0 ms（1920 宽），不是主因；主因是匹配，其次是 NoMatch 恢复路径对整个画布逐行回退扫描（`local` 段 2×帧高，失败后 `full` 段覆盖整幅画布，见 `src/stitch.rs:1221-1247`）。

> 注意：以上是单线程 CPU 合成负载。真实会话还要叠加 screencopy 拷贝、overlay 合成与预览转换，但拼接器单帧成本是硬上限。DMS 的 1.1 ms 说明"行签名"路线足以支撑 30 fps 长页采集。

---

## 10. 代码级缺陷与差距（按优先级）

### 缺陷 1（正确性，最高优先级）：NoMatch 推进参考帧，导致错位接受

- 证据：`src/stitch.rs:744-747` 在 `NoMatch` 前更新 `previous_compose_frame` / `previous_perceptual_frame`；`viewport_rect` 不变；下一帧 `translate_viewport_rect(previous_rect, match_info)`（`:844`）把相对失败帧的位移套到旧锚点。
- 影响：快速甩动后输出"表面连续、实际缺行"，且高度只轻微增长，用户很难察觉。
- 对照：DMS 失败时直接 `return`，不动 `last`/`anchor`/`lastOffset`（`stitch.go:152-155`）。
- 建议：`NoMatch` 时保留感知基线可以，但**不得**让后续帧的位移相对它生效；要么把 `previous_rect` 也随失败帧推进（并标记"未验证锚点"），要么在 `NoMatch` 后要求先与画布重新对齐（拒绝纯相对位移）。

### 问题 2（文档与代码不符，非功能缺陷）：`FixedBands` 检测未接线

- `fixed_detector.observe(...)` 的返回值只进 trace（`src/stitch.rs:645-654`）；`active_crop` 仅在初始化（`:556-577`）和几何变化（`:520-527`）时由 `FixedBands::default()` 计算，从未使用检测到的 `stable`（`src/stitch/fixed_bands.rs:66-68` 会写入 `self.stable`，但无人读取它去裁帧）。
- 证据：`grep -rn "fixed_detector.stable|active_crop = Some" src/` 只有上述四处；`detect_fixed_bands` 仅被单元测试调用。
- 判断：按 §4.1 的取舍，"把 `stable` 写回 `active_crop`"**不是待办**——那恰恰是会给用户擦屁股的自动裁剪。真正要修的是**文档声称**（`AGENTS.md`/`found.md` 的 "fixed bands" 措辞）与运行时不符：现状等于一个只产出 trace 的诊断模块。可选处置：①保留为诊断/实验代码并在文档中标注"未接线、非功能路径"；②删除 `FixedBands` 相关的生产侧调用与配置面，只留单元测试；③若将来重新考虑，必须配一组"判错时不会永久丢内容"的约束。
- 影响面：无功能影响（因为本来就没生效），但会误导后续维护者以为页眉已有保护。

### 缺陷 3（稳定性）：无画布上限

- 见 §5。建议增加字节/行上限（可参考 256 MiB / 30000 行）并在满额时停止采集但保留预览与导出，避免长页面把内存吃穿。

### 差距 4（功能）：无跳跃恢复 / seam

- 见 §6.2。建议引入显式 `PushResult::Unanchored` + 会话层"连续 N 次静止 → 整帧 seam 追加"策略，或在 UI 上明确提示"内容跳变，已停止拼接"。

### 差距 5（性能）：匹配用全宽逐像素 RGB

- 见 §9。可选优化（按收益排序）：
  1. 为画布维护与 DMS 同构的**行签名缓存**（3×f32/行），候选筛选与全局扫描全部走签名，只在最终确认时做像素复核。签名内存约 12 B/行（12k 行 ≈ 144 KiB）。
  2. 把 `find_nomatch_recovery` 的全画布回退扫描限制在局部窗口，避免 O(canvas) 最坏路径。
  3. 预览改为按需/降频：只在 `Enter` 前或每隔 N 帧转换一次，或缓存"上次转换到的行数"做增量转换。

### 差距 6（性能/内存）：预览全画布 RGB→RGBA

- 见 §8。当前每个 Accepted 帧 14–28 ms，且临时分配 ~95 MiB。

### 差距 7（鲁棒性）：空白行没有"活动行"概念

- `confirm_vertical_shift` 单行 >3.0 即否决（`src/stitch.rs:1941-1943`），本身是更严的判据，不是缺陷；真正可改进的是它对**空白行**的处理：相同白行给出 0 分，会奖励"全白对齐"。
- 可借鉴 DMS 的 "活动行"（与上一行亮度差 >2.0 才算有效比较行，纯空白行不参与决策，`stitch.go:130-136`），用来降低空白区的假匹配与误拒。
- **不**借鉴同一函数里的 `matchIgnores`（顶部 10%、底部 8% 忽略带）：它的用途正是掩盖粘性 chrome，按 §4.1 属于 out of scope。只取"活动行"，不取"忽略带"。

---

## 11. 本实现的优势（应当保留）

1. **水平拼接与方向提示**：DMS 完全没有；L 形截图、横向滚动依赖它。
2. **选区几何编辑**：E 模式、移动、四边独立 resize、宽窄往返、透明缺角，DMS 无对应能力。
3. **damage 驱动采集**：帧请求用 `copy_with_damage`（`src/wayland/screencopy.rs:376-386`），静止画面不会送达新帧（也正因如此 worker 的等待必须可被 stop flag 打断，见 `:277-290`）；DMS 静止时仍按 45 ms 轮询整帧。
4. **逐级恢复路径**：`find_dirty_overlap_recovery` / `find_nomatch_recovery` 针对悬停动画、刷新带等真实 Web 场景，DMS 只有"丢弃 + seam"。
5. **测试规模**：`stitch.rs` 内 73 个单元测试，覆盖几何偏移、参差画布、透明角、恢复路径、perceptual 边界；DMS 为 7 个拼接测试 + 3 个会话仿真（后者设计质量很高，见 §12）。
6. **无外部命令依赖**：DMS 在 Hyprland 上通过 `hyprctl` 绑定 Enter/Esc（`scroll.go:345-409`），我们不需要。

---

## 12. 值得直接借鉴的两点

1. **`scroll_sim_test.go` 的仿真手法**：用"带空白间隔 + 重复卡片块 + 固定侧栏 + 逐帧悬停噪声 + 分数偏移滚动"合成页面，跑缓动 glide、向上滚动、甩动恢复三个场景，并断言"行数误差 ≤ 一个最小追加量"与"抽样行等于页面内容"。这比逐函数单测更能暴露真实退化。本实现已有大量单点测试，但缺少这类端到端仿真；可把 DMS 的三个场景改写成 Rust 测试（其 harness 在 `/tmp/opencode/dms-sim`，可直接复用数据生成器）。
2. **"活动行"概念**：只把"与上一行有差异"的行计入有效比较（`stitch.go:130-136`），纯空白行不参与决策，可降低空白区的假匹配。**只借这一点**；同一处的"上下忽略带"不借，它是为粘性 chrome 设计的兜底，按 §4.1 属于 out of scope。

---

## 13. 复现方法

```bash
# DMS 侧：取 v1.6.3 拼接器 + 仿真测试（算法与 master 相同）
mkdir -p /tmp/opencode/dms-sim && cd /tmp/opencode/dms-sim
curl -sSLO https://raw.githubusercontent.com/AvengeMedia/DankMaterialShell/v1.6.3/core/internal/screenshot/stitch.go
curl -sSLO https://raw.githubusercontent.com/AvengeMedia/DankMaterialShell/v1.6.3/core/internal/screenshot/stitch_test.go
curl -sSLO https://raw.githubusercontent.com/AvengeMedia/DankMaterialShell/v1.6.3/core/internal/screenshot/scroll_sim_test.go
go mod init sim

# stitch.go 属于 screenshot 包；为了单独编译，把包名改为当前包名，并补两个
# 包内依赖（原文件所在包的其他文件提供，这里给出等价实现）：
#   package screenshot  → package sim（或与目录同名）
#   helpers.go:
#     const scrollSeamTicks = 4
#     func clamp(v, lo, hi int) int { if v < lo { return lo }; if v > hi { return hi }; return v }
#     func min(a, b int) int / max(a, b int) int  —— Go 1.21+ 已内置，无需提供
go test -v ./...
```

实测性能对比需要同一份像素语料：Rust 侧生成 RGB 页面文件（`page.rgb` + `meta.txt`），Go 侧读入并扩展为 RGBA 后喂给 `pushFrame`；本文的生成器（1024 或 1920 宽、16–40 行文本块 + 空白间隔）与 DMS `scroll_sim_test.go` 的 `webbyPage` 同构。

本仓库侧的性能基线需要一个临时集成测试（本次已删除，未入库）：直接调 `RawStitcher::push_frame`，用 `Instant` 计时，并读取 `last_profile_breakdown()` 拆分各阶段；若需 trace 细节，用：

```bash
SCREENSHOT_TRACE_PROFILE=1 SCREENSHOT_TRACE_VERBOSE=1 \
  cargo test --features trace-logs --test <临时测试名> --release -- --nocapture
```

注意两点：
- 必须用 `--release`，debug 构建的数字不可与 DMS 的 Go 结果比较。
- `last_profile_breakdown()` 只在 `cfg!(test)` 或 `SCREENSHOT_TRACE_PROFILE` 开启时累计（`src/stitch.rs:553`），否则读数恒为 0。

> 本文的临时探针（Rust 侧合成语料与 Go harness）已从仓库移除；如需重跑，按本节重建即可，数据生成器与 DMS 测试同构。

---

## 14. 未验证与风险

- **[推断]** 单帧耗时差距在真实会话中的用户可见程度：受 screencopy 带宽、overlay 合成、CPU 频率影响；但 87 ms vs 1.1 ms 的量级差不可能被抵消。
- **[待实测]** 缺陷 1（错位接受）在真实滚动中出现的频率：合成语料下可稳定复现，真实网页的滚轮惯性是否常触发"无重叠跳跃"仍需真机验证。
- **[未验证]** 用户实际是否会按 §4.1 的预期用 E 模式排除粘性页眉页脚。若实测发现多数用户不知道这个操作，应当补的是**文档与 UI 提示**（例如检测到选区顶部内容"不随滚动变化"时显示一次提示仍照常拼接），而不是让拼接器自动裁剪。这一条是对"不给用户擦屁股"边界的唯一松动点：提示可以，兜底不行。
- **[未验证]** DMS 的 `seamAppend` 在真实页面上的观感（接缝位置、是否可见）；本文只验证了它"不会让后续滚动永久失效"。
- 本对比未覆盖：多输出、旋转输出（DMS 直接拒绝，`scroll.go:130`）、光标处理、剪贴板；这些与拼接算法无关。

---

## 15. 真实 screencopy 语料重测（2026-10-20，生产引擎 vs DMS v1.6.3）

§9 的实测基于旧 canvas-scan 匹配器与合成语料；本节在**真实 screencopy 帧**上
重跑对比。这是首次两侧都吃到与产品完全同源的输入。

### 15.1 方法

- 语料：`testdata/frames/real-1`（462 帧 644×254，niri 下 `zwlr_screencopy` 抓取，
  滚轮跳变滚动：461 对相邻帧中 348 对 delta=0，其余 60–120px 档位跳动）。
  真值 = 逐像素穷举（`truth.tsv`，累计位移 -8020）。
- DMS 侧：`tools/dms-cmp`——v1.6.3 `stitch.go` 原样（只改包名 + 补 `clamp`/
  `scrollSeamTicks`）+ `scroll.go:546-581` 状态机逐行复刻。计时含每帧全部拼接
  工作（rowSamples + frameSig + duplicateFrame + pushFrame/seamAppend）。
- 本实现侧：生产 `RawStitcher::push_frame_with_offset`，`replay-frames --release`
  内置 `push timing`（完整推送调用；PNG 解码与 oracle 排除）。
- 同机（32 核，推送单线程）、同帧序、DMS 无方向提示（其生产模式亦然）。

### 15.2 结果

| 指标 | DMS v1.6.3 | 本实现（生产引擎） |
|---|---|---|
| 放置 / 接受 | 100 | 204 |
| 重复判定 | 343 | 256 |
| 拒绝 | 19（全部静默丢弃） | 0 no-match（1 pending，下一帧恢复） |
| 隐形接缝（seamAppend） | **11** | 0（无 seam 路径） |
| 落位与真值矛盾（成对） | **40** | 2（恢复补跳；条带判据 0 错位） |
| 整帧错位写入（条带判据） | **30 帧**（内容整体偏离自身锚点 +40~+280 行） | 0 |
| 静默丢帧（placed 且 0 新增行，真值滚动 60–180px） | **72 帧** | 0（UNCOVERED=0） |
| 最终画布 | 4529 行（应得 ~8274，**55%**；其中验证过的新增仅 1735/8020 行） | 8534 行，全部覆盖 |
| 每帧耗时（全部帧均值） | **0.123 ms** | 1.82 ms |
| 放置/接受帧均值 | **0.252 ms** | 3.99 ms |
| 最差单帧 | **1.79 ms** | 9.67 ms |
| 总耗时 | 57 ms | ~839 ms |

### 15.3 判定

- **性能**：DMS 快 ~15×（0.123 vs 1.82 ms），但两侧都远低于 33 ms/30fps 预算——
  本实现最差单帧 9.67 ms 也只占预算 29%。**在此语料上性能不是分水岭，正确性是**：
  DMS 的输出只有约一半高度、四处隐形接缝、30 帧整帧错位、72 帧静默丢失，
  全部无提示；本实现全部判据干净（0 错位 / 0 未覆盖 / 0 位移条带）。
- **DMS 损坏的机制**正是其快的原因：每行 3 个亮度带均值做签名太弱，滚轮 60px
  跳变在 17 采样下与「没动」难以区分（错配系列 -60/-120 = 匹配到旧位置、
  0 行入画布）；预测窗失败后 `seamAppend` 把帧无验证地堆到画布尾。§4.2 在
  合成语料上测得 90 次跳变，真实滚轮语料复现同一失败模式。
- **与 §11（2.5×）相比差距扩大到 ~15× 的原因**：本语料帧更小（644 宽），
  DMS 成本随分辨率近线性缩小；而本实现接受路径每次 append **全画布重分配拷贝**
  （8534×644×3 ≈ 16.5 MB/帧）——结构低效，已列 Optimization Backlog。
- 修订：§1 结论速览的耗时行（1.10 vs 87 ms）是旧匹配器数字，现口径见 §11
  （960×700 CDP：6.3 vs 2.55 ms）与本节（644×254 真实帧：1.82 vs 0.123 ms）。

### 15.4 复现

```bash
# DMS 侧（源码一次性下载，见 tools/dms-cmp/README.md）
cd tools/dms-cmp && go build -o cmp . && DIR=../../testdata/frames/real-1 ./cmp
# 本实现侧
cargo build --release --locked --bins
./target/release/replay-frames --dir testdata/frames/real-1   # 看 push timing 行
# DMS 画布正确性分析（成对矛盾 / 条带位移 / 静默丢帧）按本节数字用 placements.tsv 复算
```
