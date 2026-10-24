# DMS 拼接器对比 harness

把 `testdata/frames/` 的真实帧语料喂给 **DMS v1.6.3 原样拼接器**（`stitch.go` +
`scroll.go` 状态机逐行复刻，逻辑零改动），逐帧计时并落盘画布与落位表，供与
本仓库生产引擎做同输入对比。方法与结果见
`docs/dms-stitch-comparison.md` §15。

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
