# DMS 拼接器对比 harness

把 `testdata/frames/` 的真实帧语料喂给 **DMS v1.6.3 原样拼接器**（`stitch.go` +
`scroll.go` 状态机逐行复刻，逻辑零改动），逐帧计时并落盘画布与落位表，供与
本仓库生产引擎做同输入对比。方法与结果见
`docs/dms-stitch-comparison.md` §15（2026-10-20 首测）与 §16（2026-10-24 复测）。

## 一次性准备（DMS 源码不入库）

```bash
cd tools/dms-cmp
curl -sSLo dms.tar.gz https://codeload.github.com/AvengeMedia/DankMaterialShell/tar.gz/refs/tags/v1.6.3
tar xzf dms.tar.gz --strip-components=4 DankMaterialShell-1.6.3/core/internal/screenshot/stitch.go
sed -i 's/^package screenshot$/package main/' stitch.go
go build -o cmp .
```

`helpers.go` 补齐 stitch.go 原包的两个依赖（`scrollSeamTicks`、`clamp`，均取自
DMS 源码同版本）。`main.go` 复刻 `scroll.go:546-581` 的状态机（dup/unmatched/
seamAppend 三分支，逐行对应）。

## 运行

```bash
DIR=../../testdata/frames/real-1 ./cmp
```

输出：终端统计 + `placements.tsv`（逐帧 dup/placed/seam/added/anchor/耗时）+
`dms_canvas.png`（最终画布）。

## 口径

计时只含拼接器每帧工作（rowSamples + frameSig + duplicateFrame +
pushFrame/seamAppend），PNG 解码在外。与本仓库侧 `replay-frames` 的
`push timing` 同口径（完整推送调用，不含解码与 oracle）。

**计时必须在空载机器上顺序跑**：§16 首轮数字曾在与 `replay-frames` 并行时采集，
DMS 均值被抬高了约 1.8×（0.18 → 0.32 ms）；空载复测才作数。

## 落位判定：`analyze.py`

```bash
# 先让本仓库侧算好逐对像素真值（缓存为 .truth-replay-oracle.cache）
../../target/release/replay-frames --dir ../../testdata/frames/real-1
# 再判 DMS 的落位表
python3 analyze.py ../../testdata/frames/real-1 placements.tsv
# 几何编辑语料按等尺寸段喂（DMS 画布定宽，吃不了变宽帧）：
python3 analyze.py ../../testdata/frames/real-2 placements.tsv SEGDIR "real-2 s0"
```

判据与 `replay-frames` 同源：一对帧**严格**当且仅当逐像素穷举无平局且
cost ≤ 1.0（滚动过渡帧不判）；DMS 的 anchor 位移落在平局集 ±1 之外即被像素
反驳。产出分类：`ok / CONTRA（错放）/ STALE（旧位重放）/ DUP-DROP（动了却判
重复）/ REFUSE（拒收丢内容）`，以及严格对的像素覆盖率（静默丢失比例）。**断链
后的重新锚定放置单列**：进入 seam 的前帧必是拒绝帧，其位置无法用单对真值判定
（harness 把 seam 行记成 `placed=false`，但 `seamAppend` 确实推进了 anchor——
判读 placements.tsv 时以状态机为准，不要按列名猜）。seam 总数看 `run.txt` 的
harness 统计。
