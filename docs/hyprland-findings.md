# Hyprland 兼容性问题调研

- 日期：2026-10-12
- 范围：本项目（`screenshot-niri`，binary `screenshot`）在 Hyprland 上的行为问题
- 对照实现：DMS（DankMaterialShell）`dms screenshot scroll`
- 上游源码基线：Hyprland `main`（2026-10-12 时点）与最新发布版 `v0.56.2`（2026-08-05）

## 证据等级标记

- **[已核实]**：通过阅读源码/上游提交/官方记录确认。
- **[待实测]**：机制已明确，但尚未在本机 Hyprland 会话端到端复现。
- **[推断]**：由代码路径推导，未直接观察到。

本文档只记录**探究到的结论与证据**，不包含修复实现。修复方向见文末。

---

## 结论速览

| # | 问题 | 触发条件 | 性质 | 上游状态 |
|---|---|---|---|---|
| 1 | 长截图滚轮无法穿透到下层应用 | Hyprland 上长截图（overlay 处于 `Exclusive`） | 合成器把「键盘交互」当「指针交互」 | 已修（PR #15899），**未发布** |
| 2 | 请求「带光标帧」拿不到光标 | 光标在冻结那一刻处于隐藏状态 | 合成器「隐藏=删除光标图像」 | 部分场景相关（#15604/#15638，窗口捕获） |
| 3 | 请求「无光标帧」却仍带光标 | 输出处于**软件光标**模式 | `overlay_cursor` 只有「加」没有「减」 | 公认为已知问题，无 canonical issue |

**共同根因**：Hyprland 把「光标」与「帧缓冲」耦合、把「键盘交互」与「指针交互」耦合。

---

## 问题 1：Hyprland 上长截图滚轮不能穿透

### 现象
长截图时滚轮没有传给下层应用，页面不滚动，拼接拿不到新帧。

### 我们的实现（现状）
- 长截图期间所有 overlay 设 `KeyboardInteractivity::Exclusive`：`src/wayland/selection.rs:887`、`:987-992`。
- 依赖「空 input region」做指针穿透：`src/wayland/selection.rs:994-1006`（`create_region` + `set_input_region(Some(region))`）。
- axis 事件在 `WlPointer` 回调中被显式忽略：`src/wayland/selection.rs:1847-1850`。

### 为什么在 niri 有效、Hyprland 无效 **[已核实]**
协议规定两者解耦。niri/sway 按规范实现：空 input region 直接跳过该 surface、继续往下命中，于是滚轮穿透。Hyprland 发布版把键盘交互当成指针交互，有三处：

1. **进入「独占列表」的唯一入口用键盘字段判定** —— `src/desktop/view/LayerSurface.cpp`（v0.56.2 `:185-188`）：
   ```cpp
   const bool ISEXCLUSIVE = ...interactivity == ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_EXCLUSIVE;
   if (ISEXCLUSIVE) g_pInputManager->m_exclusiveLSes.push_back(m_self);
   ```
2. **映射时强塞指针焦点** —— `LayerSurface.cpp`（v0.56.2 `:195-206`）：`setPointerFocus(我们的surface, LOCAL)`，**不看 input region**。
3. **每次鼠标移动的「forced above all」** —— `src/managers/input/InputManager.cpp`（v0.56.2 `:453-465`），位于整个命中测试级联**最前面**：
   ```cpp
   // forced above all
   if (!g_pInputManager->m_exclusiveLSes.empty()) {
       if (!foundSurface) foundSurface = ...layerSurfaceAt(..., &g_pInputManager->m_exclusiveLSes, ...);
       if (!foundSurface) {
           foundSurface = (*g_pInputManager->m_exclusiveLSes.begin())->wlSurface()->resource(); // 无视 region 兜底
           surfacePos   = (*g_pInputManager->m_exclusiveLSes.begin())->position(...);
       }
   }
   ```
   第 2 步里的 `layerSurfaceAt` 会检查空 region（`ViewHitTester.cpp` v0.56.2 `:355-356`），但作用只是「把我从候选剔除」；剔完无人，就落入兜底分支，**仍然塞给我**。所以 `region=0` 被绕过。

链条补完：滚轮 → `onMouseWheel` → `sendPointerAxis`（只认 `pointerFocusResource`，不看键盘/region）→ 发给我们 → 我们把 axis 丢掉 → 黑洞。

### 上游修复 **[已核实]**
- 提交 `d29916a9` = PR **#15899**，标题 *"input: decouple pointer focus from layer-surface `keyboard_interactivity`"*，2026-08-21 合并进 `main`。
- 删除了上述第 2、3 处强塞，改为统一 `simulateMouseMovement()` 重新命中测试；`m_exclusiveLSes` 改名 `m_exclusiveKeyboardLSes`，只用于键盘。
- **最新发布版 `v0.56.2`（2026-08-05）早于该修复，不含此修复；截至 2026-10-12 无正式版包含它。**

### 规避（我们侧，不依赖上游）
长截图期间把所有 overlay 切 `None` + 输入区域只覆盖屏幕控件 + 点击控件时切回 `Exclusive`（`None → Exclusive` 的 `onCommit` 会 `takeKeyboardFocus()`，纯协议、无需 `hyprctl`）。`None` 不进 `m_exclusiveLSes`，三处全部失效。

---

## 问题 2：请求「带光标帧」却拿不到光标

> **重要更正**：本调研早期曾断言 `cursor:hide_on_key_press` 默认开启、启动快捷键即会隐藏光标。**该断言错误**。发布版默认值为 `false`（见下）。因此本问题的触发**需要用户在配置里启用隐藏类选项**（或用触摸输入），并非默认行为。

### 现象
启动截图时若光标处于隐藏状态，请求 `overlay_cursor=1` 的帧里也没有光标；`H` 键切换两帧无差异，表现为「再也看不到鼠标」。

### 相关默认值（发布版 v0.56.2 实测）**[已核实]**
`src/config/values/ConfigValues.cpp`（v0.56.2 `:588-608`）：

| 选项 | 默认值 | 含义 |
|---|---|---|
| `cursor:invisible` | `false` | 不渲染光标 |
| `cursor:no_hardware_cursors` | `2`（`Auto`，映射 `{Disabled:0, Enabled:1, Auto:2}`） | 是否禁用硬件光标 |
| `cursor:inactive_timeout` | `0` | **0 = 永不**按不活跃隐藏 |
| `cursor:hide_on_key_press` | `false` | 按键后隐藏，直到鼠标移动 |
| `cursor:hide_on_touch` | `true` | 触摸后隐藏，直到鼠标输入 |

### 根因 **[已核实]**
Hyprland 把「隐藏光标」实现为**删除光标图像**，而非「不绘制」：
- `IHyprRenderer::setCursorHidden`（`src/render/Renderer.cpp` `:3181-3199`）：`if (hide) { Pointer::mgr()->resetCursorImage(); return; }`。
- `CPointerManager::resetCursorImage`（`src/pointer/PointerManager.cpp` `:342-357`）清空 `surface`/`pBuffer`。
- `CPointerManager::hasCursor()`（`PointerManager.cpp` `:180-181`）随之返回 `false`。
- screencopy 的叠加入口第一条语句即检查：`renderSoftwareCursorsFor`（`PointerManager.cpp` `:734-783`）开头 `if (!hasCursor()) return;`（**在 `forceRender` 检查之前**）；且 `ScreenshareFrame.cpp:319` 调用时未传 `forceRender`（默认 `false`）。
- 即便客户端主动 `set_cursor`，隐藏状态下也会早退：`Renderer.cpp:3082`（`setCursorSurface`）与 `:3133`（`setCursorFromName`）的 `if (m_cursorHidden && !force) return;`。

**协议 vs 实现**：`zwlr_screencopy_manager_v1` 的 `overlay_cursor` 语义是 *"composite cursor onto the frame"*；Hyprland 的「光标图像不存在就无从叠加」破坏了该语义。

### 恢复条件 **[已核实]**
移动鼠标会解除隐藏：`Renderer.cpp:157-167`（`input.mouse.move` 事件清除 `hiddenOnKeyboard` 等并重算）。本项目**在 overlay 出现之前就把两帧冻结完**（`src/app.rs:74-75`），正好错过自动恢复窗口。

### 我们的相关缺口（独立的，不构成上述现象的原因）
`src/wayland/selection.rs` 全文**没有** `wl_pointer.set_cursor` 调用，`Cargo.toml` 也未启用 cursor-shape。这是「该补的功能缺失」（实时形状无法设为十字），但**不是**「输出无光标」的原因（隐藏态下调用同样被早退）。

### 上游状态 **[已核实]**
- Discussion **#15604**（0.55.2）：窗口捕获缺少请求的光标，而监视器/区域捕获包含。
- PR **#15638**（2026-08-01 合并，commit `3906d66`）：修复 toplevel/窗口捕获缺软件光标。作者原话印证 `forceRender` 语义混乱：
  > *"The function that was supposed to be forced to draw cursors still failed when it thought forcing was unnecessary."*
- #15638 只覆盖**窗口捕获**；「冻结时本就隐藏」的场景不在其中。

---

## 问题 3：请求「无光标帧」却仍带光标

### 现象
长截图恒传 `overlay_cursor=0`（`src/wayland/screencopy.rs:246-248`），拼出的图里仍带光标（软件光标模式下）。滚动时鼠标持续移动、光标恒可见，于是每帧都烧进同一个光标。

### 触发条件 **[已核实]**（`PointerManager.cpp` `:144-147`、`Monitor.cpp` v0.56.2 `:2290-2307`）
输出处于**软件光标**模式之一：
- `cursor:no_hardware_cursors = 1`（`Enabled` → 强制软件）；
- `cursor:no_hardware_cursors = 2`（`Auto`，**默认**）**且** `isNvidia() && (isMgpu() || 任一显示器 VRR 开启)` → **Auto 主动选择软件光标**；
- 硬件光标尝试失败自动回退：`PointerManager.cpp` `:324-327`（`!attemptHardwareCursor(state)` → `hardwareFailed = true`）；
- `softwareLocks > 0`（如镜像）；
- `cursor:invisible != 0` 或正在撕裂（`activelyTearing`）。

### 根因 **[已核实，有上游注释背书]**
长截图是 `SHARE_REGION`，走 `CScreenshareFrame::renderMonitor`（`src/managers/screenshare/ScreenshareFrame.cpp` `:184-321`）：
```cpp
auto TEXTURE = ctx.m_data.pMonitor->resources()->getMirrorTexture();        // :190
g_pHyprRenderer->draw(ctx, CTexPassElement::SRenderData{.tex = TEXTURE, ...}); // :218-224 整块拷贝
if (m_overlayCursor) {                                                       // :316 仅「额外补画」
    Pointer::mgr()->renderSoftwareCursorsFor(..., true);
}
```
它拷的是**显示器镜像纹理**，而**软件光标被画进显示器帧缓冲**（`Renderer.cpp:2411-2414`，普通渲染路径 `screencopy=false`）。因此 `overlay_cursor` 只有「补画」能力，**没有「擦除」能力**。

上游自证注释（`PointerManager.cpp` v0.56.2 `:654-658`）：
> *"don't render cursor on screencopy if using sw cursors / otherwise we draw the cursor again for screencopy when using sw cursors"*

| 模式 | 镜像纹理含光标 | `overlay_cursor=1` | `overlay_cursor=0` |
|---|---|---|---|
| 硬件光标 | 否 | 需合成器补画 | 干净 |
| 软件光标 | **是** | 天然有（补画被跳过防重影） | **去不掉** |

**结论**：`overlay_cursor` 在 Hyprland 里是**单向开关**（加法语义），无减法能力。

### 我们的相关实现
普通模式在 overlay 之前抓「带/不带光标」两帧：`src/app.rs:74-75`；带光标帧靠 `overlay_cursor=1`（`screencopy.rs:145`）。长模式恒传 0（`screencopy.rs:246-248`）。

### 上游检索结果 **[已核实]**
- Hyprland 论坛 *Grimblast cursor (#664)*：定性为 *"known issue with wlroots software cursors"*，解法是开回硬件光标（`no_hardware_cursors = 0`）。
- Reddit r/hyprland *「hide cursor on screenshots」*：Hyprshot 截图上残留光标碎片。
- OBS 论坛 *「Screen Capture (PipeWire) shows cursor no matter what」*：Hyprland 下取消勾选无效。
- **未在 Hyprland 仓库找到一个仍开着、专门针对「region 捕获无视 `overlay_cursor=0`」的 canonical issue**（搜索时 GitHub API 曾 403 限流，改用网页检索，不能 100% 排除）。
- 论坛把根因归给 wlroots；Hyprland 现改用 aquamarine，但行为继承。**跨合成器普遍存在**。

---

## 背景：硬件光标 vs 软件光标

- **硬件光标**：显示控制器的独立 cursor plane，扫描输出时叠加；**不在主帧缓冲里**。省资源、跟手。
- **软件光标**：光标图当普通内容**画进主帧缓冲**。兼容兜底，移动时产生 damage、整块重绘。

抓屏协议 `overlay_cursor` 的语义在两种模式下完全不同，这是问题 2/3 的根源。

**为何用户会关硬件光标**：NVIDIA 下硬件光标常出现花屏/黑方块/位置错，社区标准建议就是 `no_hardware_cursors = 1`（见上游 #16413、#13391、#14434 等讨论）。因此不能简单要求「用户自己打开硬件光标」。

---

## 上游状态汇总

| 问题 | 上游记录 | 结论 |
|---|---|---|
| 1 | PR #15899（`d29916a9`，2026-08-21 进 main） | 已修未发布，**不建议提 issue**（会被判已修） |
| 2 | Discussion #15604；PR #15638（2026-08-01）修窗口捕获 | 单提易被关（「光标本来隐藏」），仅可作 #3 附件 |
| 3 | 论坛/Reddit/其它项目零散报告；无 canonical issue | 可提，但社区默认答复是「开硬件光标」，可能 wontfix |

---

## 我们侧可做的规避（尚未实现）

| 问题 | 规避手段 | 代价 |
|---|---|---|
| 1 | 长截图期 overlay 切 `None` + 输入区域只覆盖控件 + 点控件切回 `Exclusive` | 需新增屏幕控件（完成/取消/编辑）；改成鼠标触发编辑 |
| 2 | 改冻结时机（overlay 就绪/按 H 后延迟抓「带光标帧」）；或检测到隐藏时明确提示 | 需重排冻结流程；纯提示不能真正解决 |
| 2' | overlay 补 `set_cursor`（十字） | 不解决「输出无光标」，只改善实时形状 |
| 3 | 提示软件光标用户；或长截图期设透明/无光标（会连带屏幕光标消失） | 与问题 2 冲突；后处理去光标不可靠 |

**核心矛盾**：软件光标模式下，「屏幕可见光标」与「帧无水印」在 Hyprland 里**互斥**。

---

## 待实测清单（本机 Hyprland）

```sh
hyprctl version                                   # 是否已含 PR #15899
hyprctl getoption cursor:no_hardware_cursors      # 0/1/2，判断是否 Auto→软件光标
hyprctl getoption cursor:hide_on_key_press
hyprctl getoption cursor:inactive_timeout
hyprctl getoption cursor:invisible
hyprctl systeminfo | head -3                      # 是否 NVIDIA / mgpu / VRR

# 问题 3 对照实验：
hyprctl keyword cursor:no_hardware_cursors 0      # 强制硬件光标，截图看是否干净
hyprctl keyword cursor:no_hardware_cursors 1      # 测完改回原值
```

判定：
- 问题 1：长截图时滚动，观察页面是否滚动（与光标设置无关，默认可复现）。
- 问题 2：先看隐藏类选项是否被改过；默认全关时**不应**触发，若仍无光标需另查。
- 问题 3：`no_hardware_cursors = 2`（Auto）+ NVIDIA + mgpu/VRR → 默认为软件光标 → 可复现。

---

## 未决问题

1. 上述三条机制**未在本机 Hyprland 会话端到端复现**（当前环境无 Hyprland 会话）。
2. 问题 3 未穷尽检索上游是否已有 canonical issue。
3. 本项目「运行时零外部命令依赖」原则与 DMS 用 `hyprctl` 临时绑键的做法冲突——Hyprland 规避必须走纯协议路线（屏幕控件 + `set_keyboard_interactivity`）。
