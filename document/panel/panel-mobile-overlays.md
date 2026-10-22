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

## 入口行弹层

`PanelPopover` 同一份逻辑按 `innerWidth < 768` 分支（`updatePosition`）：

| 项 | 桌面（≥768） | 移动（<768） |
| --- | --- | --- |
| 宽度 | `width` prop（缺省 520px），夹到 `100vw - 24px` | `left: 8px; right: 8px; width: auto`（贴边） |
| 纵向 | 锚点上方 8px，水平居中并夹在 12px 边距内 | `bottom: max(8, vh - rect.top + 8)`——同样浮在按钮上方，但贴底 |
| 内容高度 | `max-height: 68vh` | `max-height: 56vh`（键盘/小屏下留出上下文） |
| 圆角/描边 | 24px | 20px（`.panel-popover--mobile`，同时解除 `max-width`） |

关闭语义不变：无全屏遮罩、`pointerdown` 落在弹层与锚点之外即关、`Esc` 亦可关。

## 会话区覆盖层

上下文占用与 Agent 配置在窄屏从「贴会话框 12px」收为**近全屏**（`ChatView.vue` 与 `AgentConfigModal.vue` 各自的媒体查询）：

| 项 | 桌面（≥768） | 移动（<768） |
| --- | --- | --- |
| 外边距 | 上/左/右 12px | 上/左/右 **6px** |
| 底边 | 入口行底 + 30 + 12 | `entryBottom + 24px + 安全区`（入口行更矮、间隙同步收小；安全区不可省，否则 Home 指示条压住覆盖层底部） |
| 圆角 | 20px（外壳）/ 16px（内部面板） | 18px / 16px |
| 内容溢出 | 表格正常铺开 | 头部内边距收紧（`10px 12px 8px`），**表格靠自身横向滚动兜底** |

两条容易踩的坑：

- **覆盖层不跟随软键盘**：入口行与输入区在窄屏用 `bottom: entryBottom + keyboardInsetPx` 随键盘抬升，而 `.ctx-overlay` 的底边只算 `entryBottom + 24`。键盘弹起时覆盖层底部会被输入区/键盘盖住——设计如此（覆盖层内容自身可滚，不跟键盘做二次布局），改动时不要顺手加上 `keyboardInsetPx`，否则面板会在焦点切换时上下跳。
- **遮罩仍在会话区之内**：窄屏也没有全屏遮罩，顶栏（`☰` / 卡片 / 主题）保持可点。点遮罩空白关闭的判据两端一致。

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
| 浮层不横溢 | 打开任一入口行弹层后 `document.scrollWidth === innerWidth` |
| 浮层贴边一致 | 320px / 375px / 430px 三档下弹层左右各 8px、会话区覆盖层左右各 6px |
| 触控达标 | 确认框两按钮、Agent 配置保存/取消、实例动作按钮的可点区域 ≥ 44×44 |
| 遮罩可点 | 点会话区覆盖层遮罩空白即关闭（不误触发下层消息） |
| 不叠层 | 卡片 Sheet 与视图 Sheet 同时最多一个打开 |
