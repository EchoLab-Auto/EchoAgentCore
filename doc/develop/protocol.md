# 前端 ⇄ Core 线协议（management WS）

本文档是 EchoAgentCore 与其前端（EchoAgentPanel TUI、或任何第三方客户端）之间的
**跨仓库契约**。类型定义的唯一来源是 `echo-protocol` crate
（`source/protocol/echo-protocol/`）；本文档是对它的说明性描述，修改协议必须先改 crate。

## 传输

- Core 监听 `[core] management_address`（默认 `127.0.0.1:3132`），由
  `source/core/src/management.rs` 提供。
- 每个前端建立一条 WebSocket 连接；帧为 **JSON 文本帧**。
- Core 侧事件经 `FanoutHandle` 广播给所有已连接前端；命令由各连接独立注入。
- 前端断连不影响 Core；前端实现通常带指数退避重连（1s→30s，见 echo-tui
  `backend.rs`）。

## 信封

```rust
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WsMessage {
    Command(BackendCommand),   // Panel → Core
    Event(BackendEvent),       // Core → Panel
}
```

```json
// Panel → Core
{"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
// Core → Panel
{"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"...","branch_id":null}}}
```

序列化/反序列化助手：`serialize_command` / `serialize_event` /
`deserialize_message`（`echo_protocol::bridge`）。无法解析的帧记日志后丢弃。

## BackendCommand（Panel → Core）

| 变体 | 载荷 | 说明 |
|---|---|---|
| `SendMessage` | `{session_id, content}` | 以前端本地用户身份向会话注入一条消息，触发 agent 回合 |
| `SwitchModel` | `{model}` | 切换当前模型 |
| `SwitchProvider` | `{provider}` | 切换当前 provider |
| `SetSystemPrompt` | `{prompt}` | 替换系统提示词 |
| `UpdateApiConfig` | `{name, provider, model, base_url, api_key, thinking?, reasoning_effort?}` | 新增/更新 API profile（`name` 为空 = 顶层默认）并激活；`api_key` 为空表示不改动现有 key |
| `SwitchApi` | `{name}` | 切换到指定 profile（空 = 顶层默认） |
| `DeleteApi` | `{name}` | 删除 profile |
| `ToggleSkill` | `{name, enabled}` | 启用/禁用技能 |
| `RequestState` | — | 请求全量状态快照（逐会话 `SessionUpdated` + 最新 `BackendState`） |
| `RequestContext` | — | 请求 trunk 上下文快照（→ `ContextSnapshot`） |
| `RequestTrunkTimeline` | — | 请求持久化的显示时间线（→ `TrunkTimeline`），TUI 启动时发送 |
| `StartAdapter` / `StopAdapter` / `RestartAdapter` | `{name}` | 适配器生命周期 |
| `RequestAdapterStatus` | — | → `AdapterList` |
| `RequestGroupList` | — | → `GroupList`（QQ） |
| `RequestFriendList` | — | → `FriendList`（QQ，用于名单选择器） |
| `StartAllAdapters` / `StopAllAdapters` | — | 批量生命周期 |
| `UpdateQqAllowlist` / `UpdateQqDenylist` | `{user_ids: [i64], group_ids: [i64]}` | 运行时更新 QQ 名单 |
| `RequestQqFilterConfig` | — | → `QqFilterConfig` |
| `SetQqGateMode` | `{mode: GateMode}` | 门控模式，`"none"/"allowlist"/"denylist"`（snake_case） |

## BackendEvent（Core → Panel）

分组列出（字段详见 `echo-protocol/src/event.rs`）：

- **适配器生命周期**：`AdapterStateChanged{adapter_name, connected, self_id}`、
  `AdapterList{adapters: [AdapterStatus]}`
- **入站消息**：`MessageReceived{session_id, adapter_name, platform, user_id,
  user_name, channel, group_name, content, timestamp, received_at_ms, message_sequence}`
- **Agent 处理**：`AgentThinking`、`LlmRequest{model}`、`LlmResponse{model,
  prompt_tokens, completion_tokens}`、`AgentReasoning{branch_id, content}`、
  `AgentOutput{content, branch_id?}`、`AgentCompleted`
- **编排**：`SubagentStarted/Completed`、`ReplyBranchStarted/Content/Completed`、
  `BackgroundTaskStarted/Completed/Integrated`
- **工具**：`ToolCall{tool_name, arguments}`、`ToolResult{tool_name, result}`、
  `ChecklistUpdated{state}`（工具自定义结构化快照）
- **状态快照**：`SessionUpdated{session: SessionInfo}`、
  `ContextSnapshot{messages: [ContextMessageInfo], total_tokens, limit_tokens}`、
  `TrunkTimeline{messages: [TimelineMessage]}`、
  `ApiConfigUpdated{... profiles: [ApiProfileInfo]}`、
  `ApiProfilesUpdated{active_api, profiles}`、`Error{session_id?, message}`
  （也用于信息性 toast）
- **QQ 管理**：`GroupList`、`FriendList`、`QqFilterConfig{allowlist_users,
  allowlist_groups, denylist_users, denylist_groups}`、`QqGateMode{mode}`

## 共享枚举

| 类型 | 取值（线格式，snake_case） |
|---|---|
| `GateMode` | `"none"` / `"allowlist"` / `"denylist"` |
| `ThinkingMode` | `"enabled"` / `"disabled"` |
| `ReasoningEffort` | `"low"` / `"high"` / `"max"` |

## 兼容性规则

1. **serde 表示即线格式**：变体名、字段名、rename 策略一律不得更改。
2. 新增可选字段必须 `#[serde(default)]`（既有先例：`AgentOutput.branch_id`——
   旧 Core 发出的载荷缺该字段时新 Panel 解码为 `None`，有回归测试
   `legacy_agent_output_without_branch_id_still_decodes` 守护）。
3. 新增事件/命令变体：旧端遇到未知变体会整条帧丢弃（`deserialize_message`
   返回 `None` 并记日志），不会崩溃——但功能上视为不支持，前端应做能力探测
   或版本兜底。
4. 进程内 mpsc bridge（`create_bridge`/`BackendBridge`/`BackendHandle`/
   `FanoutHandle`）与 WS 共享同一组类型，主要用于测试；生产部署一律走 WS。
