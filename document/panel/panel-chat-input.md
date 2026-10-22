---
id: panel-chat-input
title: "输入区与入口行"
group: 前端
x: 5120
y: 1060
---

# 输入区

> **定位**：会话视图底部输入区的交互——发送、文本域、图片附件、取消任务的乐观中断，以及文本域下方的运行状态行（会话生效 API + 相位文案）。读者：维护 `ChatView` 输入侧与库 `ChatComposer` 集成的开发者。上游：[会话视图](./panel-chat.md)；上方的功能条与弹层见本文「入口行」一节。

## 发送

- **键位**：Enter 发送；Shift+Enter 换行。IME 组合输入期间不触发（判 `isComposing` 与 `keyCode === 229`——中文/日文输入法按 Enter 选词时**不能**被当成发送）
- **准入**：`trim()` 后为空不发；`disabled`（未连接 Core）时不发
- **发送后**：清空文本、清空待发附件；会话区滚到底部
- **按钮**：库 `#actions` slot 自绘（替换库默认发送按钮，保持与输入区新拟态风格一致）；`canSubmit` 为假时 `disabled`

## 文本域

- **自适应高度**：1 行起随内容撑高，上限 `calc(8em + 20px)`（≈5 行 ×1.6em 行高 + 上下 padding 10×2）后转为内部滚动——上限同时写在 `max-height` 与 `overflow-y: auto` 上，缺一不可
- **字号**：桌面 14px；**iOS 上 16px**（<16px 聚焦时 Safari 会自动放大整个页面，属框架级对策，见[移动端布局](./panel-mobile-layout.md)）
- **高度联动**：`--chat-control-h = 内边距×2 + 字号×1.6` 是「单行内框高」的唯一事实来源，附件/发送/取消三个按钮与它等高，字号一变自动跟随（见「入口行」无关，纯输入区内部几何）
- **断连占位**：placeholder 换成 `未连接到 Core，暂时无法发送`，同时 `disabled`

## 状态行

文本域（含其右侧操作行）**下方**的一行运行态提示，两段：

```
[ deepseek / deepseek-flash ]  ◌ 正在思考…
└────── API 徽标 ──────┘        └─ 相位文案 ─┘
```

**整行常驻**（2026-10 用户要求）：只要拿得到 API 信息，`idle` 时也显示 API 徽标 ——
「随时能看出这条回答是哪个 API 产出的」比"空闲时省 23px"重要。右侧相位文案仍只在
忙碌时出现；只有两者都为空（`apiLabel` 与 `activityText` 皆空）时整行才不渲染。

高度在两种状态下**完全一致**（`.chat-status { min-height: 17px }`）：否则开始回答
的瞬间输入区会突然长高 23px、入口行跟着跳一下。代价是输入区常驻 91px（原 68px），
消息区 `padding-bottom` 的余量仍够（402 宽实测占用 108 ≤ 130 − 8）。

- **位置**：`.chat-composer-float__glass` 内部的最后一项——即在输入区自己的圆角轮廓里、文本域之下（实测文本域底 823 → 状态行顶 829，间隙 6px）。**不在入口行里**：入口行是 `display:flex` 的横滚工具条，插入/移除子项会推着 Agent 切换器到「模型」整排按钮左右移动，文案变长还会抖动；搬进输入区后入口行排布恒定

### API 徽标（左）

显示**本会话实际生效**的 `provider / model`（`ChatView.vue` 的 `apiLabel`），解析顺序与会话弹层的「会话级 API」下拉严格同源：

1. 会话级覆盖 `state.sessionApiOverrides[sessionId]`
2. persona 级 `state.api.active_api`
3. 顶层默认配置（`state.api.provider` / `state.api.model`）

为什么必须按这个顺序而不是直接读入口行的「模型」按钮：那个按钮显示的是 **persona 级** `active_api`，而工作区对话可以单独指定 profile（会话级覆盖），两者会不一致——看按钮会看错「这条回答是谁在产出」。悬停 `title` 补两层信息：`来源：本会话覆盖 / persona 激活 / 顶层默认配置 · profile「名字」`。

几何：`flex: 0 0 auto`（不参与收缩，避免被相位文案挤掉）+ `max-width: 55%`（超长 provider 名内部省略）。55% 不是随手取的——`deepseek / deepseek-flash` 在 320 宽下需要 ~148px，45% 会吃掉最后一两个字符。

### 相位文案（右）

`ChatView.vue` 的 `activityText`，按 `state.activities[key].phase` 三分支：

| phase | 文案 | 由谁置位（`state.ts`） |
|---|---|---|
| `thinking` | `正在思考：{detail}…`（无 detail 时「正在思考…」） | `LlmRequest`（发出请求、等模型）；`ToolResult`（工具跑完、等模型继续，detail 为空）；`SubagentCompleted`（detail = 「正在整合 subagent 结果」/「subagent 失败，正在恢复」） |
| `tool` | `正在调用工具 {detail}…`；该工具对当前 persona 已禁用时降级为「正在处理…」 | `ToolCall`（`tool_name` 非 `run_subagent`） |
| `subagent` | `子代理工作中：{detail}…`（无 detail 时「子代理工作中…」；detail 截 72 字符） | `ToolCall`（`run_subagent`）或 `SubagentSpawn` |

`completed` / `failed` 不产生文案 —— 整行卸载。

- **截断**：单行长 `text-overflow: ellipsis`；窄屏 `max-width: 62vw`——子代理任务名上限 72 字符，不截断会撑高输入区、吃掉消息区可视高度
- **无障碍**：live region 只包相位文案（`.chat-status__live`，`role="status"` + `aria-live="polite"`）——API 名是静态信息，重复播报只会吵；spinner 为装饰，`aria-hidden`
- **带来的一次位移**：状态行出现/消失让输入区高 68 → 91px，入口行按 `entryBottom = 输入区高 + 24` 同步上移（ResizeObserver），二者间隙恒定（桌面 7px / 移动端 10px，均等于 `24 − 屏幕留白`）。消息区 `padding-bottom` 是固定值（桌面 190 / 移动 130），状态行只占 23px，余量足够

## 图片附件

- **图片**：粘贴监听挂在整个会话视图容器（剪贴板带文件即 `preventDefault`，`image/*` 过滤在入队时，`ChatView.vue` 的 `onPaste`）或附件按钮多选；统一 canvas 缩放至最长边 1600px 后重编码——PNG 仅在源码体积 ≤1.1MB 时保持 `image/png`（保透明），其余转 JPEG q0.85，编码结果超 `MAX_ATTACHMENT_CHARS`（1.5M 字符）则逐级缩小（×0.65，下限 320px；多模态端点的请求体积与文本 token 都受不了数 MB 的无损 PNG）；待发附件 64×64 缩略图 + `×` 移除；随 `SendMessage.images` 发送，发后清空

## 取消任务

- 仅当前会话忙碌时出现在操作区。点击 = **先本地乐观中断、再下发命令**（`App.vue` 的 `cancelWork`）：
  1. 本地 `cancelSessionWork`（`state.ts::cancelSessionWork`）立即生效——活动相位 → completed（状态行/取消按钮即时消失）；该会话 running 任务及其分支 → cancelled；主时间线与分支 tab 内 running 工具卡 → failed 并写入「（已取消）」
  2. 下发 `CancelRequestedWork{session_id, all:false, team_id: 当前Agent}`——`team_id` 必须携带，否则命令路由到默认 Agent，self-coding 场景下"取消 0 个任务"；后端取消成功即回 `AgentCompleted` 对齐状态
  3. 库组件自带的取消按钮不渲染（`cancelable: false`，`@cancel` 仅作转发）

# 入口行

> **定位**：输入区上方的功能条——按钮矩阵、显隐门控，以及各弹出层的几何与关闭语义。读者：维护 `ChatView` 入口行、`PanelPopover` 与各弹层的开发者。上游：[会话视图](./panel-chat.md)；弹层**内容**契约见[桌面端模态与覆盖层](./panel-desktop-overlays.md)、[实例管理·任务·Shell](./panel-instance-tasks.md)。

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
