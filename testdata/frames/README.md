# testdata/frames — 真实抓屏语料

拼接器的验收语料。**PNG 不进 git**（50 MB），只把 `session.json`、`truth.tsv` 和本文件纳入版本管理；`truth.tsv` 记录每帧的真值位移，重新分析不必再算一遍。

丢掉了也能重建，见文末。

## 语料清单

| 目录 | 帧数 | 尺寸 | 来源 | 用途 |
|---|---|---|---|---|
| `real-1` | 462 | 644×254 | **真实 screencopy**（`capture-frames`，终端文本页面，30 fps，手动滚动） | 主验收语料；§13 的两个致命 bug 就是它暴露的 |
| `real-2` | 1291 | 674×300 / 1242×300 / 486×300 / 486×383 | **真实 screencopy**（`capture-frames`，终端页面含大片空白，30 fps，含 3 次 `E` 编辑选区） | 快滑后的大幅错位（guard 交集 bug + 远放置切片证据）；教训 33 |
| `real-3` | 166 | 272×300 | **真实 screencopy**（`capture-frames`，终端页面几乎全空：一条文本行 + 一块黑色区域，60px 步长，中段有整帧亮度跳变） | 重叠区只有一两条边界时的薄证据路径；`thin_continuation` 由它落地 |
| `mini` | 12 | 644×254 | 从 `real-1` 抽帧 | 快速回归（跨前插、拒绝、恢复三个转折点） |
| `real-long` | 98 | 960×700 | CDP `Page.captureScreenshot`（headless Chrome，`docs` 页） | §12 的门限标定语料；**不是**产品实际消费的帧 |
| `real-scroll` | 8 | 960×700 | 同上，短序列 | 同上 |
| `pipeline-test` | 8 | 900×600 | 合成，内容逐行唯一 | 回放工具自检：必须 0 错位 |
| `pipeline-bad` | 8 | 900×600 | 合成，60 行周期 | 回放工具自检：必须报 ambiguous |

`real-long` / `real-scroll` 的文件名自带真实滚动位置（`f000_00600.png` → 600），`truth.tsv` 最后一列就是它。

## truth.tsv

每帧一行，由**相邻帧之间的逐像素穷举搜索**得出，不依赖拼接器：

```
# index  delta_px  match_cost  cumulative_px
0        0         0.0000      0
37       -60       0.0000      -60
```

- `delta_px`：相对上一帧的位移（负 = 向下滚）
- `match_cost`：该位移下的平均亮度差。**大于 1.0 表示这一帧是滚动动画的中间帧**（运动模糊，像素在任何位移下都对不齐）。`real-1` 有 21 帧属于这类。
- `cumulative_px`：累计位移

## 怎么用

```bash
cargo build --release --bins
./target/release/replay-frames --dir testdata/frames/real-1
./target/release/replay-frames --dir testdata/frames/real-1 --save /tmp/canvas.png
```

穷举部分（oracle 与画布定位）按候选位移拆分到所有核心，结果与线程数无关。`real-1` 全量 462 帧约 **1 秒**（32 核），串行实现约 30 分钟。默认每 4 像素采样一次（`ORACLE_STEP`）；`--exact` 改成逐像素（`real-1` 约 6 秒、`real-2` 约 9 秒），只在需要给判定做最终裁定时用——实测两种采样的接受数、错位数、条带数完全相同，只有"同一偏移集合"的计数会差几帧。

判据是三条，只有后两条算错位：

1. `delta differs from the pairwise truth` —— 恢复时把拒绝期间漏掉的位移一次补齐，**不是**错位
2. `placements whose pixels do not fit the canvas` —— 回投影逐像素比对
3. `placements far from their neighbours` —— 帧在画布中的真实行是否落在邻居的连续序列上
4. `coverage check` —— 画布内不得存在没有任何被接受帧覆盖过的行（恢复跳变丢内容会在这里现形）
5. `frames with displaced bands` —— 每个被接受帧分 4 条带独立定位，条带内容不得落在远离自己帧顶的位置（跳变过渡帧写错行会在这里现形）
6. `chain drift` —— 引擎链与逐帧真值链的累计偏差；跳变过渡帧（一帧含两个滚动位置）上出现有限漂移是预期行为，只要条带判据为 0

## 当前基线

| 语料 | 接受 | no-match | 错位 | 未覆盖行 | 位移条带 |
|---|---|---|---|---|---|
| `real-1` | 204 | 0 | 0 | 0 | 0 |
| `real-3` | 65 | 0 | 0 | 0 | 0 |
| `mini` | 3 | 0 | 0 | 0 | 0 |
| `real-long` | 97 | 0 | 0 | 0 | 0 |
| `real-scroll` | 7 | 0 | 0 | 0 | 0 |
| `pipeline-test` | 7 | 0 | 0 | 0 | 0 |
| `pipeline-bad` | 7 | 0 | 0 | 0 | 0（7 对 ambiguous，符合预期） |
| `lowcontrast-2b` | 78 | 0 | 0 | 0 | 0（灰度差仅 2 级，随机长度色块） |

`real-2` 不进这张基线表：它含大片空白与 3 次选区编辑，而且**页面内容在录制过程中会变化**（聊天界面流式输出），所以"帧像素与画布不一致"本身不能当作错位判据。它的 `pending`/`no-match` 分界随 `REFUSAL_PATIENCE` 变化，两个数字都不是判据。现在的判据是**按内容定位**后算的：230 接受 / 远放置 **0** / 位移条带 **1** / 未覆盖行 **0**；另有 154 帧"记录行 ≠ 内容行"，那是引擎页坐标被恢复路径重新定基的诊断项，不是错位（2026-10-24 之前报的 `MISPLACED 200 / 条带 9` 是坐标基准错误的产物，见 found.md 教训 34）。逐帧位移的判据也改成"落在像素允许的**平局集合**里"：1290 对里 303 对内容本身多解（大片空白），230 个接受帧中 32 个的 delta 落在平局集合之外（恢复补步或真正放错），另有 241 帧被拒但该对本身唯一可解——这是"画布没用上已知步长"的诊断，不是错位。

### 全局真值（`truth-frames`）

`replay-frames` 逐帧判断拼接器的决定；`truth-frames` 回答另一个问题：**只看像素，这一帧应该放在哪里，答案是否存在**。它刻意慢、刻意不依赖拼接器：相邻帧全分辨率穷举（不做 4 像素采样）→ 每个位移的完整代价曲线 → **平局集合**；只有"一步唯一可判"的相邻对才把页面串起来（平局步会切断坐标系，`real-2` 上强行串会凭空造出 9 276 行页面），每个 grounded run 内用除本帧外所有帧建共识画布（leave-one-out），以**中位数**残差 + 逐帧 gain/offset 拟合判定位置。

判据（`--tsv` 每帧一行）：

| verdict | 含义 | 对拼接器的要求 |
|---|---|---|
| `determinate` | 只有一个位置与共识吻合 | 必须放在那里（步长为 0 时必须是 Duplicate） |
| `ambiguous` | 多个位置同样吻合 | 放其中任何一个都对，拒绝也对 |
| `no-consensus` | 没有任何位置吻合（模糊、内容变了、两套渲染） | 判据弃权；**不是**缺陷计数 |
| `inconsistent` | 共识说另一个位置更好 | 放错了 |
| `edge` | 与本帧重叠的其他帧太少 | 不判 |

当前结果：`pipeline-test` 8/8 determinate；`real-long` 98/98；`real-scroll` 8/8；`real-1` 394 determinate / 68 no-consensus（同一页面被两遍经过、内容已变）；`real-3` 134 / 6 ambiguous / 26 no-consensus（整帧亮度跳变造成的两套渲染）；`real-2` 974 determinate / 249 ambiguous（步长本身多解，其中 191 帧落在 −288 的重复结构上）/ 68 no-consensus。

`real-3` 是**薄证据**语料：页面几乎全空（一条文本行 + 一块黑色区域），帧与画布的重叠区常常只有一条边界行。它守的是 `thin_continuation`：结构必须完全吻合（cost 恰为 0）、同一窗口内没有第二个完美答案、整段重叠的像素差 ≤ 1.0、且偏移落在 `impulse` 窗口内。修复前 4 接受 / 96 no-match / 画布停在 −240 行；修复后 65 接受 / 0 no-match / 链漂移 −3px。把这条规则放宽（去掉像素确认或去掉 impulse 约束）会让 `real-2` 的重复行产生 1.8–7.1 的像素差却仍被接受，实测错位与位移条带上升，因此两项都不可省。

修复前 `real-1` 是 **2 接受 / 405 no-match / 1 错位**。任何一项退化都是回归。

`lowcontrast-2b` 由 `gen-lowcontrast.py` 生成：横向交替两种灰（128 / 130，**仅差 2 级**），段长随机 12–48 px（非周期，且不会出现长纯色块），模拟极低对比度页面。它守的是两件事：① 近重复判定必须相对于内容对比度，否则所有移动帧都被当重复；② 边界量化必须自适应，否则所有边界都落到 level 0、每帧都被拒。`delta=1` 时引擎会拒绝（40 no-match / 0 错位）——对比度低于可分辨下限时宁可拒绝也不猜。

`real-1` 的输入形态是跳变滚动（461 对相邻帧中 348 对 delta=0，其余 60–120px 档位跳动）——重复帧多是正常现象，不是缺陷信号；引擎按设计去重并全部连上。

## 重建

`real-1` / `mini`：

```bash
./target/release/capture-frames --out testdata/frames/real-1
# 拖选区（或按 L），手动滚动，Enter/Space 结束，Esc 取消
```

操作流程与正式工具完全一致：`E` 进入/退出编辑模式（边缘缩放、整框拖动，拖动期间不抓帧，松手后按新矩形继续）、方向键给滚动方向提示、`--fps` 缺省读配置文件的 `max_fps`、选区边框宽度读 `border_width`。只有两件事不同：不拼接、不显示拼接预览。

每帧都走正式的 `prepare_capture_clean` → 抓帧 → `complete_capture` 握手，所以边框不会进画面、编辑中的中间矩形不会被抓、几何在抓帧期间变化的那一帧会被丢弃（并在终端说明）。抓到的帧**不做去重**（重复帧是正常输入，拼接器自己会判 `Duplicate`）。PNG 由写盘线程用 `Compression::Fast` + `Filter::Sub` 编码（4K 一帧约 27 ms），抓帧循环不会因为编码掉速；`session.json` 每帧重写但不 `fsync`，结束时才落盘。输出目录里若已有 `frame-*.png` 会直接报错，避免两次抓帧混在一起。

然后重算真值：

```bash
python3 - <<'PY'
# 见 git 历史中 testdata/frames/*/truth.tsv 的生成脚本
PY
```

真值脚本依赖 `numpy` + `Pillow`。用项目内 venv，不要装到系统：

```bash
uv venv .venv-analysis && uv pip install --python .venv-analysis numpy pillow
```

`pipeline-test` / `pipeline-bad` 是合成语料，脚本在 `docs/edge-stitch-design.md` §14 有说明；它们的唯一目的是验证回放工具本身能把「错位」和「歧义」区分开。

`real-long` / `real-scroll` 来自 CDP，重建需要 headless Chrome + `websocket-client`。**注意**：这条路径不经过 niri 合成器，不能用来定阈值（见 `found.md` 教训 20）。
