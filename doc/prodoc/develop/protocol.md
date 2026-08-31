---
id: protocol
title: "协议与数据流"
order: 3
x: 400
y: 300
group: 架构
link: ["agents | 多 Agent 与会话"]
---

# 协议与数据流

前后端通过 **management WebSocket**（默认 `127.0.0.1:3132`）通信，消息为 JSON。命令与事件定义在 `source/protocol/echo-protocol`（Core 与 Panel 共用同一 crate/类型）。

## 连接与 Bootstrap

- Panel 连 `/ws`；连接成功后发送 Bootstrap 命令组：`RequestState`、`RequestTrunkTimeline`（带上次 agent 的 `team_id`）、`RequestAdapterStatus`、`RequestQqFilterConfig`、`RequestTeamsList`
- 断线自动重连（500ms 起、指数退避、上限 30s）；重连后清空运行期状态再 Bootstrap

## 命令（Client → Core）

`BackendCommand` 主要分三类：

- **会话类**：`SendMessage`（带 `team_id` 路由到对应 agent）、`ClearHistory`、`ArchiveHistory`、`CompactHistory`、`RequestTrunkTimeline`（支持 `since_seq` 增量）
- **资源类**：`RequestSkillsList/ToolsList/PluginsList/TeamsList`、`ToggleSkill/Tool/Plugin`、`Save/DeleteSkill`、`SaveTeam/DeleteTeam/ToggleTeam`
- **运维类**：`Start/Stop/RestartAdapter`、`UpdateQqAllowlist/Denylist`、`SetQqGateMode`、`UpdateApiConfig/SwitchApi/TestApi/DeleteApi`

## 事件（Core → Client）

`BackendEvent` 核心事件流（一次对话的完整时序）：

```prodoc-flow
graph LR
  A[用户消息<br>MessageReceived] --> B[注册分支<br>ReplyBranchStarted]
  B --> C[系统提示词构建]
  C --> D[LLM 请求<br>LlmRequest]
  D --> E{有工具调用?}
  E -->|是| F[推理 AgentReasoning]
  F --> G[工具调用 ToolCall]
  G --> H[工具结果 ToolResult]
  H --> D
  E -->|否| I[正式回答 AgentOutput]
  I --> J[分支完成 ReplyBranchCompleted]
```

## 时间线增量同步

- Core 维护单调 `timeline_seq`（每次新增条目 +1）
- 前端按 agent 缓存时间线（`teamTimelines`），切换 agent 时**先用缓存渲染**，再以 `since_seq` 拉增量
- 增量窗口不完整（条目滚出 1024 上限）时自动回退全量

## 归属与路由

- `TrunkTimeline` 响应携带 `team_id`（= 请求值），前端按响应归属路由缓存/视图，**不用当前 activeTeamId 猜测**
- 非默认 agent 的实时事件经 `event_bus` 镜像到主 agent 连接，事件自带 `team_id` 标注

## 独立通道

- **sudo 通道**：密码经专用帧提交（不进入 LLM 上下文/会话日志），事件为 `SudoRequest/SudoResolved`
- **QQ OneBot**：QQ 适配器走反向 WS `:3131`（与 management WS 独立），QQ 消息以 `<qq_message_hook>` 标记进入 agent
