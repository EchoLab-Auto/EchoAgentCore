---
id: adr-0017
title: "ADR-0017 Panel 时序与动画规范"
group: 架构决策
link: ["adr-index | 决策索引", "panel | Panel 前端"]
x: 48
y: 384
---
# ADR-0017：Agent 思考/工具调用/回答的面板时序与动画规范

日期：2026-08-30
状态：已采纳

> 原 echo-agent-panel 仓库 `doc/decisions/0004-panel-agent-visual-timeline.md`，
> 文档群合并时重编号为 0017（决策内容未改动）。

## 背景

用户在主会话中常看到"连续工具调用而没有任何过程反馈"，误以为界面卡住。
本决策定义 Panel 主时间线对 Agent 运行过程的展示规范：及时性、时序还原、
动画反馈，并作为后续改动的一致性基线。

## 规则

### 1. 及时性（实时渲染，不缓冲）

- AgentReasoning / ToolCall / ToolResult / AgentOutput 事件到达即渲染进主
  时间线（`state.trunk`），**不得**等回复完成后再一次性写入。
- 推理块不再缓冲到回复尾部（历史：`pendingReasoning` + 回复附加
  `reasoning` 字段）；改为独立 `reasoning` 角色消息按序插入。

### 2. 时序表现（还原真实顺序）

- 主时间线顺序 = 事件到达顺序：
  `用户消息 → 推理 → 工具调用 → 工具结果 → 推理 → 工具调用 → … → 正式回答`。
- 推理真实穿插在工具调用之间（每轮 LLM 响应产生的推理位于该轮工具调用
  之前）；正式回答总是最后一条。
- 历史回放（`loadTimeline` / `TrunkTimeline`）同样把 `backend.reasoning`
  拆分为独立的 reasoning 消息、插在回答之前，与实时路径时序一致。
- 分支详情（侧边栏"临时分支"）继承同一规则：`BranchMsg` 已按
  reasoning/tool/content 顺序排列，仅做连续 reasoning 的合并展示。

### 3. 动画反馈（对应位置对应动画）

| 阶段 | 位置 | 动画 |
| --- | --- | --- |
| 连接中 | 顶栏状态点 | 状态点呼吸 |
| 思考 / 调用工具 / 子代理 | 输入框上方活动浮条 | 旋转 spinner + 动态文案（正在思考… / 正在调用工具 X…） |
| 推理输出 | 推理块 | 打字机逐字显示（自适应速度，总时长约 6s 封顶）+ 光标/呼吸点 |
| 工具执行中 | 工具卡（库渲染） | running 状态 spinner（ui-frame 内置） |
| 消息到达 | 每条实时消息 | 0.28s 淡入上移入场动画 |
| 正式回答 | Agent 消息 | 入场淡入（内容由库直接渲染） |

- 打字机/入场动画**仅**对实时消息（`DisplayMessage.animate === true`）播放；
  历史回放、刷新加载不播动画，避免整屏滚动特效。
- 动画为纯视觉层：`animate` 是展示元数据，不进入任何数据/逻辑判断。

## 实现位置

- `src/state.ts`：`MsgRole` 增加 `'reasoning'`；`AgentReasoning` 直接
  `pushTo('reasoning', …, { animate: true })`；`AgentOutput` 不再附加缓冲推理；
  `DisplayMessage.animate` 标记实时消息。
- `src/state_domains/timeline.ts`：`loadTimeline` 拆分 `reasoning` 段。
- `src/chat-adapter.ts`：角色映射（reasoning 由库外渲染）。
- `src/components/ChatView.vue`：`#message` slot 拦截 `reasoning` 渲染
  `ReasoningBlock`；活动浮条；`.chat-view__item--enter` 入场动画。
- `src/components/ReasoningBlock.vue`：推理打字机组件（新）。

## 注意事项

- ui-frame `ChatRole` 不含 `reasoning`：必须在 `ChatView` slot 层拦截，
  否则 `ChatMessageItem` 会把未知角色渲染成 Agent 气泡。
- `pendingReasoning` / `completedReasoning` 保留（分支合并块归并仍消费）；
  主时间线不再读取。
