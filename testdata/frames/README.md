# testdata/frames — 真实抓屏语料

拼接器的验收语料。**PNG 不进 git**（50 MB），只把 `session.json`、`truth.tsv` 和本文件纳入版本管理；`truth.tsv` 记录每帧的真值位移，重新分析不必再算一遍。

丢掉了也能重建，见文末。

## 语料清单

| 目录 | 帧数 | 尺寸 | 来源 | 用途 |
|---|---|---|---|---|
| `real-1` | 462 | 644×254 | **真实 screencopy**（`capture-frames`，终端文本页面，30 fps，手动滚动） | 主验收语料；§13 的两个致命 bug 就是它暴露的 |
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

穷举部分（oracle 与画布定位）按候选位移拆分到所有核心，结果与线程数无关。`real-1` 全量 462 帧约 **5 秒**（32 核）；串行实现约 30 分钟。

判据是三条，只有后两条算错位：

1. `delta differs from the pairwise truth` —— 恢复时把拒绝期间漏掉的位移一次补齐，**不是**错位
2. `placements whose pixels do not fit the canvas` —— 回投影逐像素比对
3. `placements far from their neighbours` —— 帧在画布中的真实行是否落在邻居的连续序列上

## 当前基线

| 语料 | 接受 | no-match | 错位 |
|---|---|---|---|
| `real-1` | 204 | 0 | 0 |
| `mini` | 3 | 0 | 0 |
| `real-long` | 97 | 0 | 0 |
| `pipeline-test` | 7 | 0 | 0 |
| `pipeline-bad` | 7 | 0 | 0（7 对 ambiguous，符合预期） |

修复前 `real-1` 是 **2 接受 / 405 no-match / 1 错位**。任何一项退化都是回归。

## 重建

`real-1` / `mini`：

```bash
./target/release/capture-frames --out testdata/frames/real-1
# 拖选区，手动滚动，Enter 结束
```

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
