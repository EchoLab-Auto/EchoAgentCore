---
id: panel-mobile-overlays
title: "移动端模态与覆盖层"
group: 前端
x: 2240
y: 780
---

# 移动端模态与覆盖层

> **定位**：**<768px** 下 Panel 的模态与覆盖层**形态差异**——同一个入口在窄屏怎么摆、谁改成了贴边、谁被软键盘挡住、触控目标怎么保证 ≥44px。内容契约（上下文占用口径、Agent 配置字段、确认形式矩阵、分支查看器行模型、目录选择器浏览语义）与视口无关，见[桌面端模态与覆盖层](./panel-desktop-overlays.md)，本文只写「窄屏变了什么」。读者：维护窄屏形态的开发者；上游：[移动端布局](./panel-mobile-layout.md)；相关：[输入区](./panel-chat-input.md)、[入口行](./panel-chat-entry.md)、[交互定义](./panel-interaction.md)。

## 总则

1. **层级与关闭语义两端一致**——三级分层、Esc 归属、`confirmDialog` 的并发与结算规则不因视口改变；窄屏只改几何与尺寸。
2. **改法只有一种：贴边**。所有浮层在窄屏都从「居中/锚定 + 固定宽度」改成「左右贴边 + 自适应宽度」，把 375px 的每一像素给内容。
3. **断点同源**：CSS 用 `@media (max-width: 767.98px)`，模板用 `useTouchDevice().isMobile`（`innerWidth < 768`）——两者判定永不打架。所有窄屏规则收在媒体查询内，桌面零回归。
4. **触控目标 ≥ 44px 由库兜底**：`@media (pointer: coarse)` 下 `NeumorphismButton` 各尺寸的 `min-width/min-height` 提升到 44×44，所以确认框、Agent 配置、实例动作按钮的命中面积在触屏上自动达标，面板不需要逐个调大。
5. **第三类承载是 Sheet**：侧栏卡片与视图切换在窄屏不走浮层，而是底部 Sheet（库 `NeumorphismDrawer position="bottom"`）——视图 Sheet 三项（聊天 / 设置 / Shell）、卡片 Sheet 高 78%，两者共用 `mobile-sheets.ts` 的单值、互斥不叠层，卡片栈在 Sheet 内改纵向 `block`、滚动交给 Sheet 自身。几何见[移动端布局](./panel-mobile-layout.md)「视图切换 Sheet」「卡片 Sheet」，本文不重复。
6. **Toast 换位**：窄屏改 `top-center`（右上角贴近拇指操作区易误触遮挡），桌面维持 `top-right`；`max-count` 均为 8。

## 入口行覆盖面板

除 Agent 切换外的七项共用 `SessionOverlay`（2026-10 统一，见
[桌面端](./panel-desktop-overlays.md)「会话区覆盖面板」）。窄屏**没有独立的尺寸分支**：
几何仍按「消息区矩形」测量，而窄屏的消息区本来就接近全屏（左右贴边、上到顶栏下方、
下到输入区上沿）——

| 项 | 桌面（≥768） | 移动（<768） |
| --- | --- | --- |
| 宽度 | 消息区宽度 = 视口 − 边栏留白（`324px` ×已登记的列） | 视口全宽（边栏隐藏，留白为 0） |
| 纵向 | 上到会话框顶、下到输入区顶（`entryBottom` 之上） | 同左（输入区贴底，故面板即「顶栏下方到输入区上方」） |
| 圆角 | 24px | 同 24px（外壳统一；内部组件各自收紧内边距） |
| 内容溢出 | 外壳 `overflow: auto` | 同左；表格仍靠自身横向滚动兜底 |

- **不再有「贴底全宽、`max-height: 56vh`」的弹层分支**：面板高度由消息区决定，
  软键盘弹出时消息区变矮，`ResizeObserver` 触发重算，面板跟着收——不会被键盘压住。
- **互斥与关闭**两端一致：单一 `entryOverlay` 状态、点面板外 / `Esc` / 再点同一按钮；
  入口行按钮（`z-index 6`）浮在面板（5）之上，横滚工具条仍可点。
- **Agent 菜单**（`AgentSwitcher` 的 `PanelPopover`）窄屏仍是「贴底全宽、
  `max-height: 56vh`」——它不在本次统一范围内。

两条容易踩的坑：

- **面板随消息区一起收**：面板边界来自对 `.chat-view__tray` 的测量，而 tray 的下界是输入区顶；窄屏输入区用 `bottom: entryBottom + keyboardInsetPx` 随键盘抬升，tray 随之变矮 → 面板自动避开键盘。**不要**再给外壳写固定的 `bottom: calc(...)` 覆盖（会与测量值打架）。
- **没有遮罩**：顶栏（`☰` / 卡片 / 主题）与入口行保持可点；关闭判据两端一致（点面板外 / `Esc` / 再点同一按钮）。

## 应用级模态（库 `NeumorphismModal`）

分支查看器、全局确认框、Git 安装技能都走库模态。库的窄屏行为是**未做断点适配**的，面板侧也不覆写：

- **遮罩**：`position: fixed; inset: 0`，flex 居中，**无 padding** → 窄屏上模态 `width: 100%` 直接贴边
- **尺寸**：`max-width` 只是上限（`small 400` / `medium 560` / `large 720`），375px 下**等于全宽**；`max-height: 85vh`，正文区 `overflow-y: auto`
- 因此**分支查看器在手机上是整屏大卡**（`size=large` 的 720px 上限失效），元信息与 chip 行按可用宽度换行；全局确认框（`size=small`）同样是全宽横条
- **内容高度优先于「留边好看」**：不要给库模态加窄屏 margin/padding 覆写——`max-height: 85vh` 已保证顶栏与底部手势条区域不被吃满，加边距只会让 Markdown / 结论正文更挤

## 目录选择器

`DirectoryPicker` 是自有遮罩 + `fixed` 定位，尺寸用视口比例而非断点：`min(560px, 92vw)` × `max-height: 72vh`，居中。窄屏（320–430px）实际等价于 **92vw 贴边**，桌面则固定 560px。内部三行结构（头部位置下拉 + 面包屑 + 列表 + 底部确认）在窄屏靠列表自身纵向滚动，不折叠。

「为何必须 Teleport」「浏览根与 canonical 校验」见[桌面端模态与覆盖层](./panel-desktop-overlays.md)「目录选择器」——那部分与视口无关。

## 自检

| 断言 | 判据 |
| --- | --- |
| 浮层不横溢 | 打开任一入口行覆盖面板后 `document.scrollWidth === innerWidth` |
| 面板贴齐消息区 | 面板 rect 与 `.chat-view__tray` 减去边栏留白后的矩形逐边相差 ≤ 2px（实测 1600/1440/375 三档全等） |
| 触控达标 | 确认框两按钮、Agent 配置保存/取消、实例动作按钮的可点区域 ≥ 44×44 |
| 关闭判据 | 点面板外的输入区/顶栏即关闭；点入口行另一个按钮直接切换（同一时刻只有一个 `.session-overlay`） |
| 不叠层 | 卡片 Sheet 与视图 Sheet 同时最多一个打开 |
