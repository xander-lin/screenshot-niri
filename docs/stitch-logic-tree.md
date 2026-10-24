# 图像拼接逻辑树

这份文档描述**一帧从进来到写进画布的完整决策树**：每个判断问的是什么、窗口多大、闸门按什么顺序过、失败落到哪里。它回答"代码为什么是这个顺序"，不重复设计实验数据（见 `docs/edge-stitch-design.md`）与性能数字（见 `docs/performance.md`）。

代码是权威：下文用函数名和阶段名，不写行号。结构或顺序一变，这一份必须同改。

---

## 0. 一句话结构

```
重复？ ──是──> Duplicate（不碰任何状态）
  │否
  ▼
在画布的哪里？ ──冲量──> 守卫──> 锚点──> 薄证据──> 都没有 = 拒绝（Pending / NoMatch）
  │
  ▼
写进去（append 或 已知位置）──> 提交（描述子 + 步长记忆 + 方向记忆 + 拒绝计数）
```

三个名词先对齐：

| 词 | 含义 | 代码 |
|---|---|---|
| **place** | 决定帧在画布上的位置，**纯函数**，不改画布 | `WindowedEdgeMatcher::place` |
| **accept** | 把 place 的结论包成 `EngineDecision::Place`（含 `FrameMatch`） | `WindowedEdgeMatcher::accept` |
| **commit** | 位置已落盘后，把这一帧写进描述子画布与步长记忆 | `commit` / `commit_known_step` |
| **apply / append** | 把**像素**写进像素画布、必要时扩画布 | `try_apply_match` → `append_frame_at_position` |

---

## 1. 坐标与两层画布

### 1.1 五个坐标

| # | 坐标系 | 谁用 | 怎么换 |
|---|---|---|---|
| 1 | 全局逻辑（合成器页面坐标） | 选区矩形、输出布局、layer-shell | 起点 |
| 2 | 输出本地逻辑 | screencopy 请求（`prepare_capture_clean`） | 减去输出原点 |
| 3 | 抓帧像素（frame） | `RgbFrame`、`FrameScreenOffset` | 逻辑 × 抓帧密度 |
| 4 | 画布像素（canvas row/col） | `StitchedFrame`、tile、coverage | `canvas_origin` 把逻辑行与像素行对齐；画布**向上增长会重定基** |
| 5 | 引擎逻辑行 | `EdgeCanvas`、`pos`/`anchor`/`last_delta`、`last_engine_top` | `canvas_origin = engine_top - pixel_row` |

规则只有一条但最容易被破坏：**引擎只存第 5 个坐标**。`commit` 同时收到 `requested_top`（逻辑）与 `rect`（像素），所有写入、`pos`、`anchor`、delta 都用 `requested_top`；把像素行当逻辑行用会让"向上增长"之后画布不再生长（`found.md` 教训 14）。

### 1.2 两层画布

| 层 | 类型 | 存什么 | 谁写 |
|---|---|---|---|
| 像素画布 | `StitchedFrame` → `CanvasStore` | 64 行 tile、lz4 冷压缩、每行 coverage、`current_origin`/`anchor_origin` | `append_frame_at_position` |
| 描述子画布 | `EdgeCanvas` | 沿滚动轴每行的 48 bins × 3 级（0/1/2）、`filled`、`nonzero`、`origin_row` | `commit`（`EdgeCanvas::write`） |

一帧的描述子是 `EdgeLadder`：`levels`（rows × bins）、`scoring_rows`（最多 `EDGE_MAX_SCORING_ROWS = 192` 条最强边界行，升序）、`row_mass`（每行非零级数，供分母）。

`EdgeCanvas` 是**一列**（沿轴），没有 x 维度；这正是水平放置后必须 `reset` + 重新 `commit` 的原因（§8）。

---

## 2. 状态：帧间记住什么

### 2.1 `RawStitcher`（编排层）

| 字段 | 含义 | 拒绝时 |
|---|---|---|
| `stitched` | 像素画布 | 不变 |
| `viewport_rect` | 上一帧在像素画布的矩形 | **冻结** |
| `active_crop` | 当前帧的有效裁剪（固定带检测器；生产等于整帧） | 不变 |
| `previous_compose_frame` | 上一帧转换结果（= 比较基准） | **冻结** |
| `spare_compose_buffer` | 回收的转换缓冲（两帧循环） | 拒绝时归还当前帧的缓冲 |
| `accepted_direction` | 上一次**真正移动**的轴与方向 | **冻结**（零位移会清空，见 §6.4） |
| `last_screen_offset` | 上一帧的屏幕偏移（几何变化的判据） | 不变 |
| `consecutive_refusals` | 连续拒绝数 | **+1** |
| `last_engine_top` / `canvas_origin` | 引擎逻辑行 ↔ 像素行的映射（诊断 + 几何编辑） | 不变 |
| `last_reject_reason` | 最近一次的拒绝原因（UI / trace） | 覆盖 |

### 2.2 `WindowedEdgeMatcher`（引擎）

| 字段 | 含义 | 拒绝时 |
|---|---|---|
| `canvas: Option<EdgeCanvas>` | 描述子画布 | 不变 |
| `pos` | 上一个**已接受**帧的逻辑行（预测中心） | **冻结** |
| `anchor` | 上一个**已验证**位置（锚点搜索中心） | **冻结** |
| `last_delta` / `trend` | 上一步位移与其趋势（预测） | **冻结** |
| `frame_rows` | 上一帧的行数（重叠/守卫半径） | 不变 |
| `pending_ladder` / `pending_top` | `place` → `commit` 之间传递的一帧描述子 | `place` 开头清空 |
| `session_gain` / `gain_decided` | 整个会话唯一的量化增益 | 不变（第一帧无边界时递延） |

`pending_top` 目前**只写不读**（commit 用的是参数 `requested_top`），是清理候选；`pending_ladder` 是真实契约："place 造好的描述子给 commit 用，避免第二次读帧"。

---

## 3. 入口树

```
push_frame_with_offset(compose, analysis, direction, offset)
├─ validate 两个 source
├─ 几何变化？ =  offset ≠ last_screen_offset
│              ∨ frame.width  ≠ active_crop.width
│              ∨ frame.height ≠ active_crop.height
│  ├─ 是 → push_geometry_frame（§7，已知位置，不过任何闸门）
│  └─ 否 → push_frame_sources（§3.1）
└─ 首次调用时 last_screen_offset 也记下（Initialized）
```

### 3.1 `push_frame_sources`

```
push_frame_sources（编排，约 100 行）
├─ begin_frame()                       清空本帧诊断
├─ initialize_frame()                  stitched 为空 → 第一帧即画布 → Initialized
├─ duplicate_gate()                    §3.2 → 是则 Duplicate（唯一不碰状态的出口）
├─ prepare_frame()                     §3.3 → PreparedFrame { compose, cropped, fused }
├─ choose_axis()                       §3.4 → (EngineDecision, SearchAxis)
├─ Reject → 归还缓冲 → reject_frame()  §9 → Pending / NoMatch
└─ Place  → apply_placement()          像素 + engine.commit → AppliedPlacement
             └─ commit_accepted()      描述子已提交，这里推进编排层状态
```

`push_frame_sources` 只负责"顺序"，每个阶段是独立函数：
`begin_frame`（7 行）/ `initialize_frame`（30）/ `duplicate_gate`（59）/
`prepare_frame`（56）/ `choose_axis`（78）/ `apply_placement`（65）。
`apply_placement` 返回 `None` 表示落点无法映射到画布（今天只有溢出会走到，防御性拒绝）。

### 3.2 重复判定

- 比较对象：**上一帧的 viewport 矩形**（从画布按采样读，不物化整块）vs 新帧。
- 采样：两个轴每 4 像素；同一次采样同时得到"平均差"和"亮度范围"。
- 阈值：`clamp(0.1 × 亮度范围, 0.25, duplicate_threshold=2.0)`。
  相对阈值是低对比度内容的生命线：绝对阈值会把差 2 级的两灰页面上的真实滚动判成重复，画布永不增长。
- 判为重复后：更新 `last_duplicate_*` 诊断，返回 `Duplicate`。

### 3.3 转换 + 描述子

- 裁剪等于整帧（生产配置）→ `rgb_frame_from_source_fused`：**转换与 bin 均值一趟并行完成**，输出 `PrebuiltMeans`。
- 否则（固定带检测器的真实裁剪，trace-only）→ 串行转换，引擎自己从帧建描述子。
- `session_gain` 在引擎里量化，调用方只给原始均值。

### 3.4 选轴与回退

```
primary = accepted_direction.axis()           // 上一次真正移动的轴
          .unwrap_or(hint → Horizontal/Vertical)  // 还没有冲量时，箭头提示说了算
```

1. `place_on_axis(primary, primary=true)`。
2. 触发回退的条件（任一）：
   - primary 返回 `Reject`，或
   - primary 返回 **零位移 Place**（"没动"在像素已经变化时不构成答案，§6.4）。
3. 回退只问一次 `other_axis`：
   - 垂直回退：对称搜索（向上滚是普通一步）；不需要用户提示。
   - **水平回退：必须有水平方向提示**（内容里没有"该往哪边"的信息）；没有提示直接 `Reject`。
4. 回退结果**不与 primary 比分数**，只按"是否给出了更好的答案"取舍：
   `improved = primary 不是零位移 || 回退给出非零位移`。
5. 水平轴胜出后：垂直引擎 `reset` + 用这一帧重新 `commit`（§8）。

这条"零位移不是答案"的规则是 `real-5-4k` 上 115 帧 × 8.7 ms 的根因修复，测试 `a_zero_delta_acceptance_clears_the_direction_memory`。

---

## 4. 引擎 `place` 的五段

`place` 是**纯的**：只读请求与画布，不写任何状态；接受时把描述子放进 `pending_ladder`、位置放进 `pending_top`，由 `commit` 消费。

前置检查（任一失败即拒绝，且**不进入搜索**）：

```
描述子：prebuilt 可用则用（尺寸/layout/长度必须匹配），否则 EdgeLadder::from_source
gain：  gain_decided ? session_gain : 本帧峰值（递延规则）
canvas 存在？ bins 一致？ scoring_rows 非空（全平帧 = NoBoundaries）
```

预测：`predicted_top = pos + round(last_delta + trend)`。

### 4.1 阶段 1 — 冲量（impulse）

| 项 | 值 |
|---|---|
| 问题 | 上一帧的步长还成立吗？ |
| 中心 | `predicted_top` |
| 窗口 | `predicted_top ± max(24, 1.05×|last_delta|)`（首帧无冲量则跳过） |
| 搜索 | `decide`（窗口内逐行，不再分环） |
| 额外闸门 | 若 `|top − pos| ≥ rows`（远放置）→ `far_placement_supported`（§5.3） |

1.10 会破：它开始抢走本该由守卫放置的跳变帧（`real-1` 出现 2 条带位移）。

### 4.2 阶段 2 — 守卫（guard）

| 项 | 值 |
|---|---|
| 问题 | 这一帧是上一帧的下一段吗？（**只对重叠位置**成立） |
| 中心 | `pos`（不是 prediction） |
| 窗口 | `pos ± rows`，整条带完整搜索；预测**只用来排平局**（`tie_centre`） |
| 搜索 | `decide_ring(inner=0, outer=rows)` |
| 额外闸门 | **不调用** `far_placement_supported`：本地位置按构造就有重叠，证据自足 |

窗口是 `rows` 而不是 `rows − minimum_overlap`：重叠是几何问题，证据是质量问题，不能把闸门折进窗口（`real-2` 帧 1043–1067 的教训）。

### 4.3 阶段 3 — 锚点（anchor）

| 项 | 值 |
|---|---|
| 问题 | 这一帧在画布的**任何**位置出现过吗？（与"是不是下一段"是两个问题） |
| 中心 | `anchor`（最后一个已验证位置；`pos` 是预测中心，这里不用） |
| 窗口 | 环：半径 `rows`（一个视口），每环 ×4，直到覆盖画布边界；跳过守卫带 |
| 搜索 | `search_rings` → 每环 `decide_ring`；近环先试，第一个接受即返回 |
| 额外闸门 | 每个接受候选都过 `far_placement_supported`（§5.3） |

- 环的尺寸来自帧（`rows`），不是配置常量——固定宽度带实测丢 58 次接受、多 13 次写错。
- 锚点**不再等边框变红**：与上一帧完全不重叠的帧在守卫带里连打分都不可能，"等"只会更远。等待属于边框与降帧（§9）。
- 没有位移上限：能匹配到哪里由画布还能提供多少证据决定，而不是配置的像素数。

### 4.4 阶段 4 — 薄证据（thin continuation）

| 项 | 值 |
|---|---|
| 问题 | 重叠只读得到 1–2 条边界行，但答案是否**像素级确凿**？ |
| 窗口 | 守卫带 `pos ± rows`（薄的证据不允许去远处找） |
| 门槛 | 见下 |
| 额外闸门 | 无需 `far_placement_supported`：离 `pos` 不超过半帧 |

全部满足才接受，否则落空：

1. 结构代价**恰为 0**（每一行每个 bin 都吻合）——这是"事实"不是"拟合"；
2. basin 之外没有第二个 0 代价答案（页面在步长处重复 → 拒绝）；
3. `|top − pos| ≤ rows/2`（帧的大部分仍是上一帧的内容，`real-2` 的 +340/11% 重叠重复块被这条挡住）；
4. 像素平均差 ≤ `EDGE_THIN_MAX_COLOUR_DIFFERENCE = 1.0`（`real-2` 的重复行薄赢家实测 1.8–7.1）。

它是**最后一段**而不是第一段：把它提到守卫之前，`real-4-4k` 的重复行会从 2 次远放置变成 196 次。

### 4.5 阶段 5 — 拒绝

返回 `NoCandidate`。调用方决定它算 `Pending` 还是 `NoMatch`（§9）。

---

## 5. 闸门：`decide_from_costs`

所有搜索阶段共用这一个判定。输入是候选 `(cost, top, used)` 列表，输出 `Accept` 或 `Reject(原因)`。

| 顺序 | 检查 | 条件 | 失败 |
|---|---|---|---|
| 1 | 样本量 | `len < 4` | `InsufficientEvidence` |
| 2 | 排序 | 按 cost 升序 | — |
| 3 | 次优 | basin 外最便宜（`basin = max(frame_rows/8, 2)`） | `None` → ratio = 0 |
| 4 | 并列者 | cost ≤ best×1.5，basin 外，按 basin 去重 | 只用于 tie-break |
| 5 | 优势比 | `second_over_best`（best=0 且 second>0 → ∞；否则 second/best） | — |
| 6 | 绝对上限 | `best > 0.08` 且 `ratio < 2.5` | `NoCandidate` |
| 7 | 歧义 | `ratio < 1.5` → 对 winner + 并列者做像素校验，存活者取离中心最近；一个都不存活 | `Ambiguous` |
| 8 | 像素校验 | 选中者的颜色差 > 30 | `NoCandidate` |
| 9 | 方向校验 | `used ≤ 8` 且边界方向与画布相反 | `NoCandidate` |
| 10 | 相对完整度 | `window_used = max(used) among cost ≤ best×1.5` | 交给阶段 3 的 `used ≥ window_used` |

两条必须记住的性质：

- **闸门不能依赖窗口**。`best/median` 试过：同一个赢家在 ±36 窗口读 0.731、±1400 读 0.488。现在的量都是"帧 + 画布"的性质。
- `second = None` 读作 0，所以"窗口里没有对手"不能冒充 2.5 倍优势；但 `best = 0` 时读作 ∞（完美答案是最高置信）。

### 5.1 为什么 8 条可比行之后改为方向校验

低对比度条带页的整个色阶只有 4–12 级，绝对颜色上限 30 看不见它：暗带贴到亮带上，代价几乎为 0。描述子只存幅度、不存方向，所以方向是最后一道像素判据（`boundary_directions_agree`）。分数缩放的重采样保持方向、单周期色阶交换会反转方向——这是两者唯一的稳定差别。

### 5.2 为什么像素校验对**每一次**接受都做

描述子对颜色是盲的；一个结构上完美重复、颜色不同的页面能骗过所有结构闸门（实测 0.073 的胜者、1.82 倍优势）。tie 与"自信的假答案"是同一个故障穿了两套衣服。

### 5.3 `far_placement_supported`（仅远放置）

在 `|top − pos| ≥ rows` 时（冲量与锚点的接受），额外要求：

1. `used ≥ window_used`（相对完整度：赢家解释的边界行不少于同窗口任何合理候选）；
2. 像素可验证：`colour_cost_at(top).is_some()`；
3. 像素能**反驳**次优：`rival_colour ≥ (winner + 30) / 2`。

空白区在任意偏移都"匹配"，它的对手也落在噪声里 → 拒绝；一条边界的页面仍按那一条边界要求（相对，不是绝对行数）。

---

## 6. 落盘树

`EngineDecision::Place` 之后：

```
1. find_dirty_overlap_recovery（脏边修复，窄条件）
   ├─ x == 0、等宽、帧触到画布边、engine 的 score 不干净
   └─ 命中 → append_frame_at_position_overwriting_overlap_rows → engine.commit → commit_accepted
2. try_apply_match
   ├─ translate(previous_rect, match)  → candidate_rect
   ├─ candidate 已被画布完全覆盖（viewport_rect_covered_by_stitched）
   │  └─ 是 → 不写像素，只更新 viewport（内容已在画布上）
   └─ 否 → append_frame_at_position
3. engine.commit（描述子 + pos/anchor + 步长记忆）
   └─ 若本帧由水平轴放置：先 reset，再 commit（重新播种列）
4. commit_accepted（§6.3）
```

> **本轮的清理**：这里原本还有一段 `find_nomatch_recovery` 兜底（约 200 行，含 `recover_stale_bottom_edge` / `recover_stale_top_edge`）。`translate_viewport_rect` 从不返回 `None`（只可能溢出报错），所以 `try_apply_match` 的 `false` 分支不可达，兜底与两个 stale-edge 修复一起是**死代码**，而且它绕过了上面所有闸门。已删除；`try_apply_match` 现在若返回 `false`（今天不会发生）就直接落到 `NoCandidate`，不再有旁路。脏边修复 `find_dirty_overlap_recovery` 是**活路径**，位置在它之前。

### 6.1 `append_frame_at_position`（像素）

- 目标矩形超出画布 → 按需扩宽/扩高；**宽度变化 → `reflow_store` 重切所有 tile**（这是唯一非增量的路径）。
- 向上增长 → `relabel`/`shift_forward`，只移动 tile 头，不搬像素。
- 逐行合并 coverage（`RowCoverage::from_spans`，相邻区间合并，保留从未写入的洞），只写**未被覆盖**的像素（先写入者胜）。
- 返回帧在像素画布中的矩形。

### 6.2 `engine.commit`

- 消费 `pending_ladder`（没有则重建）。
- `canvas.write(requested_top, ladder)`：只写 `filled == false` 的行；向上越界时先 `shift_forward`/重定基。
- `pos = anchor = requested_top`。
- `last_delta = requested_top − previous`；`trend = 0.5×trend + 0.5×(delta − last_delta)`。
- `gain_decided` 为假且本帧有边界 → 锁定会话增益。
- `pending_top = None`；`last_committed_ladder` 留作调试。

`commit_known_step`（几何编辑用）：完全相同的写入，但**不更新** `last_delta`/`trend`（盒子移动不是滚动）。

### 6.3 `commit_accepted`（编排层）

```
viewport_rect = new_rect
previous_perceptual_frame = 本帧（legacy 用）
previous_compose_frame 换成本帧，旧帧的缓冲进 spare
path / 诊断
accepted_direction：moved ? record(direction) : 清空
consecutive_refusals = 0
```

`moved` = `delta_x ≠ 0 ∨ delta_y ≠ 0`。零位移清除方向记忆，否则停顿期会把"恰好回答了零"的轴锁成冲量（§3.4）。

### 6.4 拒绝出口

```
reject_frame(reason)
path = NoMatch
consecutive_refusals += 1
< REFUSAL_PATIENCE(5) ? Pending : NoMatch
```

除了 `consecutive_refusals` 和诊断字段，**什么都不动**：`viewport_rect`、两个参考帧、`pos`/`anchor`/`last_delta`/`trend`、方向记忆全部保持——这是"用户往回滚还能接上"的前提。

---

## 7. 几何编辑（known step）

选区在 `E` 里被移动/缩放（或帧尺寸变了）时，位置是**已知的**，不搜索：

```
1. 转换新帧
2. delta = 新 offset − 旧 offset（y 还要减去旧 crop 的 y）
3. frame_x/y = previous_rect + delta
4. 重叠评分（诊断用 score）：canvas_overlap_rect + 两个视图直接采样（不拷 5 MB）
5. append_frame_at_position（已知 rect）→ new_rect
6. engine_top = last_engine_top + delta_y（**不能**用像素 rect，会被向上增长重定基）
7. engine.commit_known_step(new_rect, engine_top, 新帧)
8. 方向记忆：delta ≠ 0 → record；delta == 0（纯 resize）→ 清空
9. layout_changed = 宽度变 ∨ delta_x ≠ 0
   └─ 是 → engine.reset() → commit_known_step → reseed_from_canvas
```

`reseed_from_canvas`：把画布像素按 512 行带重新量化成描述子（每带多读一行做边界），**只重建整段新 x 范围都有覆盖的带**——一行如果在别的列截的，描述子会由 0 组成，而"填了的 0 行"比缺行更糟（会被当成黑色参与比较）。`reset` 之后画布只剩这一帧，reseed 把它扩回整个会话的证据，否则比滚动步长还矮的盒子永远恢复不了。

编辑的耗时构成（`real-5-4k` 实测）：加宽时的 reflow（8.8 ms）> reseed（0.6–7.5 ms）> 提交 > 评分（0.06–0.5 ms）。都是**每次编辑一次**。

---

## 8. 水平轴（按需构建）

水平方向不维护常驻画布（少见，且每次垂直步都要重新播种），而是**每次现建**：

```
place_horizontally(stitched, previous_rect, frame, direction)
1. 无方向提示且不是 primary → 拒绝（内容不告诉你往哪边）
2. gain = session_gain_on(frame, Horizontal)
3. frame_ladder = from_source_on(frame, Horizontal)
4. strip = previous_rect 的像素区域（**必须**是上一帧矩形：
   更宽的带会含从未抓取的列，0 字节污染描述子，蛇形第一次左移就被拒）
5. strip_ladder = from_source_on(strip, Horizontal)；bins 必须一致
6. 一次性 EdgeCanvas(strip_ladder)，pos = previous_x
7. 窗口：primary → previous_x ± width（两个方向都合法）；
        fallback → 只搜提示指向的一侧
8. decide(...)（**只有这一段**：没有守卫，没有锚点，没有环）
9. 接受 → Place { rect.x = top }
```

水平胜出后，垂直引擎必须 `reset` + `commit`（§3.4）：`EdgeCanvas` 只有一列，原来的行是在别的列上采的。

---

## 9. 拒绝语义与 UI

| 结果 | 条件 | UI |
|---|---|---|
| `Pending` | 连续拒绝 < 5 | 边框不变、满帧率 |
| `NoMatch` | 连续拒绝 ≥ 5 | 边框变白红、降帧到 5 fps（约 170 ms @30fps） |
| `Duplicate` | 重复判定 | 什么都不发生 |

一次拒绝不是"内容停止重叠"的证据：渲染接缝、半帧重绘、低对比度区域都能让单帧歧义，下一帧可能完全正常。`REFUSAL_PATIENCE` 只属于**边框与帧率**这段用户承诺，不属于匹配器（匹配器在守卫没有答案时直接进锚点，§4.3）。

---

## 10. 常量表

| 常量 | 值 | 作用 | 改动风险 |
|---|---|---|---|
| `EDGE_BINS` | 48 | 每行描述子的列数 | 画布格式变化，全语料重跑 |
| `EDGE_MARGIN_*` | 每侧 5% | 跳过滚动条/边框 | 窄选区会没有 body |
| `EDGE_LEVEL_ONE/TWO` | 8 / 26 | \|ΔL\| 量化为 0/1/2 | 与 `session_gain` 耦合 |
| `EDGE_MAX_SCORING_ROWS` | 192 | 每帧参与评分的行数上限 | 越多越慢，越少越瞎 |
| `EDGE_MIN_COMPARABLE_ROWS` | 3（`min(3, 本帧边界行数)`） | 证据下限 | 降到 1：`real-1` 204→201、0→6 错位 |
| `EDGE_MAX_ACCEPT_COST` | 0.08 | 绝对代价上限 | 放宽会收下模糊匹配 |
| `EDGE_MIN_SECOND_OVER_BEST` | 1.5 | 歧义阈值（并列判定 + `window_used` 口径） | 抬高会拒绝真实并列 |
| `EDGE_CONFIDENT_SECOND_OVER_BEST` | 2.5 | 绕过绝对上限所需的优势比 | 降低会收下高代价赢家 |
| `EDGE_MAX_COLOUR_DIFFERENCE` | 30 | 每次接受的像素校验 | 真内容在动 40.6，假结构 82.2 |
| `EDGE_THIN_MAX_COLOUR_DIFFERENCE` | 1.0 | 薄证据的像素校验 | `real-2` 重复行 1.8–7.1 |
| `EDGE_SIGN_CHECK_MAX_USED` | 8 | 何时改用方向校验 | 越多越贵，越少越看不见色阶交换 |
| `EDGE_BASIN_DIVISOR` | 8 | 赢家盆宽 | 太窄把肩当对手 |
| `EDGE_ADJACENT_ROWS` | 2 | 盆宽下限 | — |
| `EDGE_IMPULSE_*` | 24 / 1.05 | 冲量半径 | 1.10 出现条带位移 |
| `EDGE_RING_GROWTH` | 4 | 锚点环增长 | 固定带丢 58 接受 |
| `REFUSAL_PATIENCE` | 5 | Pending→NoMatch | UI 语义 |

---

## 11. 这棵树够清晰吗（自评）

### 清楚的部分

- **顺序有语义**：冲量（上一帧的步长）→ 守卫（还是下一段吗）→ 锚点（在画布哪里出现过）→ 薄证据（重叠太少但答案确凿）。四段问的是四个不同问题，注释写明了"为什么不是别的顺序"，且都有语料实验兜底。
- **place 纯、commit 显式**：决策不产生副作用，画布只能由 commit 推进；这一点有 `engine_place_is_pure_and_does_not_mutate_state` 钉住。
- **闸门集中**：所有阶段共用 `decide_from_costs`，不存在"每个阶段一套阈值"。
- **可替换**：`PlacementEngine` trait + `ScriptedEngine` mock + `stitch-legacy` A/B，树本身不绑死实现。
- **验证闭环**：`verify-corpora` 持有 11 个语料的基线，任何树结构改动都能立刻看出代价。

### 不够清楚、值得改的部分（按价值排序）

1. **注释漂移**（本轮已修 4 处）：`pending_ladder` 的文档里混进了一段 `consecutive_refusals` 的说明；"Stage 4" 有一段自相矛盾的重复；`far_placement_supported` 还在用旧的 `used == scoring_rows` 说法；拒绝路径注释还写着"下面的 stale-edge recovery"（那段已删）。根因是"设计变更只改代码"。建议：闸门注释里不写具体数字，数字只进常量表。
2. **一个长函数**：`push_frame_sources` 约 550 行、6 个出口（初始化 / 重复 / 两个接受 / 两个拒绝）；`place` 反而是 160 行的五段式。树是清楚的，但它是**读出来的**，不是**看出来的**——`push_frame_sources` 里的"重复→转换→选轴→落盘"本可以是四行调用。建议按阶段抽函数：`duplicate_gate` / `prepare_frame` / `choose_axis` / `apply_placement`，让 `push_frame_sources` 只剩一棵 30 行的树。
3. **隐式不变量**：`pending_top` 只写不读；`last_engine_top`/`canvas_origin` 主要给诊断与几何编辑；`previous_perceptual_*` 只服务 legacy。本文 §2 的字段表补上了这层说明，但代码里没有单一出处。
3. **轴决策分居两处**：引擎会返回零位移的 `Place`，调用方再决定"要不要问另一轴"；规则靠两处注释 + 一个测试维系。建议收进 `choose_axis`，让"零位移不是答案"只出现一次。
4. **recovery 的命名**：`find_dirty_overlap_recovery` 是活的脏边修复（在 `Place` 之后）；它曾经与死掉的 `find_nomatch_recovery` 成对，名字高度相似、行为完全不同。删除之后这一点已经缓解，但"recovery"仍值得在函数名里带出"dirty edge"。
5. **命名四件套**：`place`（决策）/ `accept`（决策封装）/ `apply`（像素落盘）/ `commit`（描述子落盘）。没有对照表时，新人很容易把 `commit` 当成"写像素"（§0 的表就是为此写的）。

### 结论

**主干是清晰的，边缘也在收敛**：四段搜索 + 一套闸门的骨架稳定、有数据支撑。写这份文档时的两处硬伤——死分支、550 行的编排函数——已经处理（删死代码、按阶段拆成 6 个函数）。剩下的都是软性债务：注释漂移的机制、隐式不变量没有单一出处、以及"一帧的五个动词"需要术语表（§0）。

---

## 12. 修改这棵树的检查清单

1. **先存基线**：`cargo build --release --bins && ./target/release/verify-corpora --jobs 2`（11 个语料，约 46 s）。
2. **一次只动一个阶段**；用 `--features trace-logs` + `SCREENSHOT_TRACE_EDGE_GATE=1` 看每个窗口的 gate 数字，`SCREENSHOT_TRACE_PROFILE=1` 看阶段耗时，`SCREENSHOT_TRACE_VERBOSE=1` 看放置路径。
3. **常量的标定语料写在常量注释里**；改常量必须说明它动的是哪条语料的哪个数字。
4. **全语料 + 全特性组合 + clippy 45**：`cargo test`（261）、`--features stitch-legacy`（252）、`canvas-raw-tiles`、`trace-logs`。
5. **树变了，文档必须变**：这一份的 §3–§8 与 `AGENTS.md` 的 `stitch` 契约、`docs/performance.md` 的对应轮次同一次提交。
