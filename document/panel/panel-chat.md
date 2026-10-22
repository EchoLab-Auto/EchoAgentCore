---
id: panel-chat
title: "Panel 会话视图"
group: 前端
x: 3840
y: 780
link: ["panel-chat-messages | 消息区", "panel-chat-input | 输入区", "panel-chat-entry | 入口行", "panel-chat-rail | 双侧边栏"]
---

# Panel 会话视图

> **定位**：会话（聊天）视图的总览——视图构成、三个分区的职责、组件与数据流地图，以及十篇子契约的入口：消息区（角色与渲染、列表滚动、历史合并、时序动画、工作行）、输入区、入口行、双侧边栏。读者：维护 `ChatView` 及其组件的开发者；实现位置以 `web/src/` 相对路径标注。相关：[交互定义](./panel-interaction.md)、[桌面端布局](./panel-desktop-layout.md)、[桌面端模态与覆盖层](./panel-desktop-overlays.md)。

> **多接入点**：会话/人格身份是 `(core, id)`——同名会话（每个 core 都有 `local:tui::local_user`）与同名人格跨 core 可重复。会话视图内所有运行态（activities / branchTabs / 时间线缓存 / 忙碌点）一律按复合键（`keys.ts::sessionKey/teamKey`，`\u0000` 分隔，单接入点退化为裸 id）索引；会话列表与 Agent 菜单在跨 core 同名时显示 core 徽标消歧（`SessionSwitcher` / `AgentSwitcher`，见 [Panel 概览](./panel.md)「配置与运维」）。

## 视图构成

会话视图由**消息区 + 吸底输入区**构成，左右两侧各有一列常驻侧栏（chat-rail）；消息行由面板自渲染（滚动区由 ChatView 自组，`.chat-view__list` **自身即滚动容器**——库 `ChatTray` 组装件已解散），以拦截扩展角色（`reasoning` / `subagent`）的渲染。

**只有三个分区**：消息区、输入区、侧栏。「工作行」（工具 / 推理 / 子代理的 chip 行）不是独立分区——它是**消息列里的一种行**：`ChatView` 的 `messageRows` 在构建行列表时就地合并相邻工作项，随后由同一套滚动容器、渲染窗口切片、历史合并与缓存机制承载。因此它的契约挂在[会话消息区](./panel-chat-messages.md)之下。

```prodoc-flow
graph LR
  View[会话视图] --> Msg[消息区|/panel/panel-chat-messages.md]
  View --> Inp[输入区|/panel/panel-chat-input.md]
  View --> Ent[入口行|/panel/panel-chat-entry.md]
  View --> Rail[双侧边栏|/panel/panel-chat-rail.md]
  Msg --> List[消息列表与滚动|/panel/panel-chat-list.md]
  Msg --> Hist[历史快照与实时窗口|/panel/panel-chat-history.md]
  Msg --> Time[主时间线时序与动画|/panel/panel-chat-timeline.md]
  Msg --> Work[工作行|/panel/panel-chat-work.md]
  Work --> Tool[工具卡与图标行|/panel/panel-chat-tools.md]
  Work --> Rea[推理块|/panel/panel-chat-reasoning.md]
```

## 分区与子契约

| 分区 | 子契约 | 覆盖 |
|---|---|---|
| 消息区 | [会话消息区](./panel-chat-messages.md) | 消息角色与渲染分工、气泡几何、渲染对象缓存 |
| ↳ 列表 | [消息列表与滚动](./panel-chat-list.md) | 跟随与回到底部、容量与渲染窗口、流式与重算成本、空态 |
| ↳ 历史 | [历史快照与实时窗口](./panel-chat-history.md) | 双副本合并、落位与去重、条目身份、渐进式分页、刷新与落盘 |
| ↳ 时序 | [主时间线时序与动画](./panel-chat-timeline.md) | 及时性、时序表现、动画反馈与边界 |
| ↳ 工作行 | [工作行（工具 / 推理 / 子代理）](./panel-chat-work.md) | 工作项模型与相邻合并、chip 与开合策略、读秒、子代理委派 |
| ↳↳ 工具 | [工具卡与图标行](./panel-chat-tools.md) | 工具卡生命周期、配对与收尾、图标行折叠/动画/展开 |
| ↳↳ 推理 | [推理块](./panel-chat-reasoning.md) | 推理块实时与历史、折叠与跟随、落位与可见性 |
| 输入区 | [输入区（ChatComposer）](./panel-chat-input.md) | 发送与文本域、外壳（卡片 / 超椭圆轮廓）、状态行、图片附件、取消任务 |
| 入口行 | [入口行（功能条）](./panel-chat-entry.md) | 按钮矩阵与显隐门控、六个内容弹层、弹层几何与关闭语义 |
| 侧栏 | [双侧边栏（chat-rail）](./panel-chat-rail.md) | 两列布局与列显隐、卡片拖动、避让与间距、卡片清单与门控 |

## 组件与数据流

- **容器与行渲染**：`ChatView.vue`（消息区 + 吸底输入区 + 入口行 + 双侧栏的组织者，`messageRows` / `renderWindow` / `historyLimit` / `railRegistered` 等都在此）、`MessageItem.vue`（气泡自绘 + `MarkdownLazy` 延迟 Markdown）、`WorkRunGroup.vue`（工作行：`tool` / `reasoning` / `subagent` 三类工作项的 chip 行）、`ReasoningText.vue`、`ToolIcon.vue`
- **状态与投影**：`state.ts` + `state_domains/timeline.ts`（事件 reducer 与时间线投影）、`chat-adapter.ts`（`toChatMessage` / 时间标签 memo）、`chat-message-cache.ts`（渲染对象缓存）、`trunk-cache.ts`（时间线落盘与还原）、`keys.ts`（`sessionKey` / `teamKey` 复合键）
- **输入侧与弹层**：`ChatComposer`（库机制原语，`#actions` 由面板自绘）、`SessionOverlay`（会话区覆盖面板外壳：几何 / 关闭 / 互斥）、`ContextView` / `AgentConfigModal` / `WorkspacePanel` / `SessionSwitcher` / `TasksPanel`
- **侧栏**：`RailStack.vue`（卡片栈、折叠与分隔条）+ `rail-drag.ts`（跨列拖动）+ `rail-cards.ts`（卡片登记与门控）+ `rail-layout.ts`（列归属与顺序持久化）
- **单一事实来源**：一切展示由 Core 的 WS 事件流驱动，前端时间线缓存只是加速副本（断连重连按游标补齐）——原则与失败语义见[交互定义](./panel-interaction.md)「核心原则」

## 阅读路径

- **要改消息怎么画**：本页 → [会话消息区](./panel-chat-messages.md) → [消息列表与滚动](./panel-chat-list.md) → [历史快照与实时窗口](./panel-chat-history.md)
- **要改运行过程怎么呈现**：本页 → [会话消息区](./panel-chat-messages.md) → [工作行](./panel-chat-work.md) → [工具卡与图标行](./panel-chat-tools.md) / [推理块](./panel-chat-reasoning.md)；时序与动画基线见[主时间线时序与动画](./panel-chat-timeline.md)
- **要改输入**：本页 → [输入区](./panel-chat-input.md)
- **要改入口行与弹层**：本页 → [入口行](./panel-chat-entry.md)；弹层内容契约见[桌面端模态与覆盖层](./panel-desktop-overlays.md)
- **要改侧栏**：本页 → [双侧边栏（chat-rail）](./panel-chat-rail.md)；外壳几何见[桌面端布局](./panel-desktop-layout.md)
