---
id: panel-chat-input
title: "输入区与入口行"
group: 前端
x: 5120
y: 1060
---

# 输入区与入口行

> **定位**：会话视图底部的输入侧交互——发送与文本域、图片附件、取消任务的乐观中断，以及入口行按钮矩阵与弹出层的几何、关闭语义。读者：维护 `ChatView` 输入侧、`PanelPopover` 与各弹层的开发者。上游：[会话视图](./panel-chat.md)；入口按钮打开的弹层内容见[桌面端模态与覆盖层](./panel-desktop-overlays.md)、[实例管理·任务·Shell](./panel-instance-tasks.md)。

## 发送与文本域

- **发送**：Enter（IME 组合输入守卫，`isComposing`/229 不触发）；Shift+Enter 换行；空文本（trim 后）不发送；发送后清空
- **文本域**：1 行起自适应撑高，上限 `calc(8em + 20px)` 后内部滚动
- **断连禁用**：`disabled` + placeholder `未连接到 Core，暂时无法发送`

- 发送按钮为库 `#actions` slot 自绘（替换默认按钮，保持与输入区新拟态风格一致）

## 图片附件

- **图片**：粘贴监听挂在整个会话视图容器（剪贴板带文件即 `preventDefault`，`image/*` 过滤在入队时，`ChatView.vue` 的 `onPaste`）或附件按钮多选；统一 canvas 缩放至最长边 1600px 后重编码——PNG 仅在源码体积 ≤1.1MB 时保持 `image/png`（保透明），其余转 JPEG q0.85，编码结果超 `MAX_ATTACHMENT_CHARS`（1.5M 字符）则逐级缩小（×0.65，下限 320px；多模态端点的请求体积与文本 token 都受不了数 MB 的无损 PNG）；待发附件 64×64 缩略图 + `×` 移除；随 `SendMessage.images` 发送，发后清空

## 取消任务

- 仅当前会话忙碌时出现在操作区。点击 = **先本地乐观中断、再下发命令**（`App.vue` 的 `cancelWork`）：
  1. 本地 `cancelSessionWork`（`state.ts::cancelSessionWork`）立即生效——活动相位 → completed（浮条/取消按钮即时消失）；该会话 running 任务及其分支 → cancelled；主时间线与分支 tab 内 running 工具卡 → failed 并写入「（已取消）」
  2. 下发 `CancelRequestedWork{session_id, all:false, team_id: 当前Agent}`——`team_id` 必须携带，否则命令路由到默认 Agent，self-coding 场景下"取消 0 个任务"；后端取消成功即回 `AgentCompleted` 对齐状态
  3. 库组件自带的取消按钮不渲染（`cancelable: false`，`@cancel` 仅作转发）

## 入口行按钮

入口行位于输入区上方（`bottom = 输入区高度 + 24px`，ResizeObserver 跟踪）。**按钮组件化**：入口按钮统一使用 ui-frame `NeumorphismButton`（`variant=glass` 磨砂半透明、`shape=pill` 胶囊、`size=small`）；清单数字徽标使用 `NeumorphismBadge`（右上角、`showZero=false`、0 时隐藏）；模板 ref 取组件实例的 `$el` 定位弹层（`btnElement()` 辅助）。按钮只保留布局微调（图标 14px）；作用域内覆盖玻璃令牌使按钮更透（`.entry-row` 内 `--nm-glass-bg` 降至 45% 不透明度、`--nm-glass-blur` 提至 24px）：

| 按钮 | 行为 |
|---|---|
| Agent 切换 | 见[桌面端布局](./panel-desktop-layout.md)「Agent 切换器」；菜单自 2026-10 起与本节其它六项共用 `PanelPopover`（此前是入口行内的就地绝对定位弹层，窄屏会被横滚容器裁掉） |
| 配置 | 打开 AgentConfigModal（当前 Agent 的能力配置）；无激活 Agent 时回退 teams[0]（`ChatView.vue` 的 `configTeam` computed） |
| 上下文 | 打开 ContextView 弹层（**当前会话**的上下文明细；请求带 `session_id`，切换会话自动重拉、忽略他属陈旧快照；几何与「配置」弹层一致，点遮罩关闭，无返回按钮）。**面板默认展开全部块**（修正此前仅 base 展开、用户「看不到内容」的问题）；每块头部 = 种类徽标 + 名称 + token 数 + **占当前上下文总量的百分比**（<1% 显示 `<1%`；修正此前「相对最大块」口径的误读——小技能块显示 1%、大块显示 100%）；种类徽标覆盖 Core 全部 kind（含 `workspace`/`system-skill`，缺项会裸显英文）；名称自动剥掉与徽标重复的前缀（「常驻技能 · xx」→ 徽标「常驻技能」+ 名称「xx」）；**入口受 `echo-agent.management.panel` 插件对当前 Agent 门控**（`ChatView.vue` 的 `contextAvailable` computed） |
| 清单 | **仅当 `checklist` 工具对当前 persona 可用时显示**（插件维度已移除，只受工具级白/黑名单与内置工具集包门控，见 [插件化设计](../plugins/core-plugins.md)）；弹出清单卡（徽标 = 清单数）：内嵌 SidebarCard 折叠卡（默认展开、计数徽标），各清单进度条 + `PanelIcon check-square / square` 项（完成项加粗，tone 分别为 ok / muted），只读；工具禁用后入口、浮层与徽标立即移除并清空本地清单状态；空态「（暂无清单）」 |
| 会话 | 弹出 `SessionSwitcher`（多会话切换；**取代侧边栏会话卡**。显隐依赖会话归属完整——Core 侧三层保证见 [多 Agent](../core/core-agents.md)「会话模型」）：按平台分组列出当前智能体的会话（Local / 各平台私聊 / 各平台群聊 / 其他——当前只有 QQ），当前高亮、运行中带忙碌点，点击切换（聊天区按会话过滤；每个群/私聊有独立上下文，见 [多 Agent](../core/core-agents.md)「会话模型」）。**Local 分组含工作区通道**（`local:workspace:<id>:local_user`，昵称 = 工作区名，标签「工作区」）：选通道 = 激活对应工作区、选默认会话 = 取消激活（见 [多 Agent](../core/core-agents.md)「工作区会话与项目通道」）。**>1 个会话或并行多会话模式（含「全局」项）时显示**；切换 Agent 强制关闭 |
| 工作区 | 弹出工作区会话面板（`WorkspacePanel`，普通宽度）：多会话管理（新建/编辑/删除/激活；每会话多个工作区目录）**+ 每会话多对话**（新建/重命名/删除/切换，经 `SaveWorkspaceSession` 全量 upsert；删对话要发 `ClearHistory` 一并清历史） |
| 实例 | 弹出「实例管理」面板（见 [实例管理·任务·Shell](./panel-instance-tasks.md)）——内容全由插件描述符决定，Panel 无平台专属逻辑；仅当前 Agent 启用实例插件时显示；切换 Agent 强制关闭 |
| 任务 | 弹出 `TasksPanel`（替代原顶栏任务视图）：**只对应当前 agent 的当前会话**（`sessionId` 过滤，「全局」会话显示全部），徽标 = 当前会话运行中/等待整合任务数；显隐 = `echo-agent.subagent` 插件对当前 persona 启用或当前会话已有任务记录；切换 Agent / 插件禁用强制关闭。卡片/状态/取消语义见 [实例管理·任务·Shell](./panel-instance-tasks.md) |
| 模型 | `ModelSwitcher`：胶囊按钮显示当前生效模型名（`state.api.model`，与 Agent 卡片的模型名严格同源），点击弹出 `PanelPopover` 面板列出全局供应商池——首项「跟随默认配置」，其余每项 `provider / model` + 缺 key 提示；选中即下发 `SwitchApi{name}`（**persona 级** `active_api`，Core 重建 provider、对全部会话生效，会话级覆盖除外）。会话级覆盖仍归「会话」弹层管，两者不互相顶替 |

## 弹出层几何与关闭语义

弹出层默认宽 **520px**，视口不足时夹取到 `100vw - 24px`（`PanelPopover` 的 `MARGIN`）；锚定按钮上方 8px、相对输入区水平居中。**清单 / 会话 / 工作区 / 插件实例 / 任务 / 模型六个弹出层都没有全屏遮罩**——是 fixed 定位的内容尺寸面板，**点弹层外任意位置（含聊天区）即关闭**（`PanelPopover` 在 document 捕获阶段监听 `pointerdown`，目标不在弹层与锚点内就 `requestClose()`），`Esc` 亦可关闭。**「配置」「上下文」不是 `PanelPopover`**，而是会话区内的磨砂玻璃覆盖层（上/左/右 12px，底边 = 入口行高 + 42px；`z-index: 50`），点遮罩关闭、无返回按钮、不监听 Esc——契约见[桌面端模态与覆盖层](./panel-desktop-overlays.md)「会话区覆盖层」。
