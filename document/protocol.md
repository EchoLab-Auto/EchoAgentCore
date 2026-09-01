---
group: 协议模块
x: 606
y: 528
---

# 协议与数据流

前后端通过 **management WebSocket**（默认 `127.0.0.1:3132`）通信，消息为 JSON 文本帧。命令与事件定义在 `source/protocol/echo-protocol`（Core 与 Panel 共用同一 crate/类型）——**类型定义的唯一来源是该 crate，修改协议必须先改 crate**；本文档是对它的说明性描述。

## 仓库拆分与契约归属

- 仓库拆分为 **EchoAgentCore**（后端 agent 服务）与 **EchoAgentPanel**（前端）两个独立仓库，两端可独立构建、发布、演进，仅通过 `echo-protocol` 契约耦合
- `echo-protocol` 是前端 ⇄ Core **线契约的唯一来源**：只定义 `BackendCommand` / `BackendEvent` / `WsMessage` / bridge / 共享枚举（`GateMode` / `ThinkingMode` / `ReasoningEffort`），不依赖任何 agent、平台或 UI 代码；serde 表示即线格式
- Panel 是纯粹的「协议客户端」：仅依赖 `echo-protocol` 中的类型，不包含任何 agent 或 QQ 逻辑；后端各 crate 通过 re-export 保持 `echo_agent::…` 路径兼容
- Panel 以相对路径依赖 `echo-protocol`（要求两个仓库并排克隆，CI 须将 EchoAgentCore 作为 sibling 检出），或改指 git 依赖
- 协议演进必须两端同步：新增字段向后兼容（见文末「兼容性规则」）；语义变更需双端协同合入

## 传输与信封

- Core 监听 `[core] management_address`（默认 `127.0.0.1:3132`），由 `source/core/src/management.rs` 提供；每个前端建立一条 WS 连接
- Core 侧事件经 `EventBroker` 扇出到每条连接的独立订阅通道；命令由各连接独立注入
- 服务端 30s Ping 心跳（90s 无任何入站活动判死断开）；Panel 转发层对双向心跳做透传，空闲连接不会被中间层悄悄断开
- 断线自动重连（500ms 起、指数退避、上限 30s）；重连后清空运行期状态与时间线缓存再 Bootstrap

```rust
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WsMessage {
    Command(BackendCommand),          // Panel → Core
    Event(BackendEvent),              // Core → Panel
    SudoPassword(SudoPasswordSubmit), // Panel → Core（专用 sudo 通道）
}
```

```json
// Panel → Core
{"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
// Core → Panel
{"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"...","branch_id":null}}}
// Panel → Core（sudo 密码；仅此通道，绝不走 Command）
{"type":"sudo_password","payload":{"request_id":1,"password":"***"}}
```

序列化助手：`serialize_command` / `serialize_event` / `deserialize_message`（`echo_protocol::bridge`）；无法解析的帧记日志后丢弃。

> **sudo 密码安全约定**：`SudoPasswordSubmit.password` 是 `Option<String>`（`Some` 授权 / `None` 拒绝）。密码帧由 management server **直接路由到 sudo broker**，不经过 agent 命令队列、会话日志与 LLM 上下文；专用反序列化器失败时只记错误、不记原始文本，`Debug` 输出打码。

## 连接与 Bootstrap

连接成功后 Panel 发送 Bootstrap 命令组：`RequestState`、`RequestTrunkTimeline`（带上次 agent 的 `team_id`）、`RequestAdapterStatus`、`RequestQqFilterConfig`、`RequestTeamsList`。

## 命令（Client → Core）

`BackendCommand` 主要分三类：

- **会话类**：`SendMessage`（带 `team_id` 路由到对应 agent）、`ClearHistory`、`ArchiveHistory`、`CompactHistory`、`RequestTrunkTimeline`（支持 `since_seq` 增量）
- **资源类**：`RequestSkillsList/ToolsList/PluginsList/TeamsList`、`ToggleSkill/Tool/Plugin`、`Save/DeleteSkill`、`SaveTeam/DeleteTeam/ToggleTeam`
- **运维类**：`Start/Stop/RestartAdapter`、`UpdateQqAllowlist/Denylist`、`SetQqGateMode`、`SetQqOwner`、`UpdateApiConfig/SwitchApi/TestApi/DeleteApi`

完整变体与载荷见 `echo-protocol/src/command.rs`；QQ 管理类还有 `RequestGroupList` / `RequestFriendList` / `RequestQqFilterConfig` 等查询命令。

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

其他事件分组（字段详见 `echo-protocol/src/event.rs`）：

- **适配器生命周期**：`AdapterStateChanged`、`AdapterList`
- **编排**：`SubagentStarted/Completed`、`ReplyBranchStarted/Content/Completed`、`BackgroundTaskStarted/Completed/Integrated`
- **状态快照**：`SessionUpdated`、`ContextSnapshot`、`TrunkTimeline`、`ApiConfigUpdated`、`ApiProfilesUpdated`、`Error`（也用于信息性 toast）
- **QQ 管理**：`GroupList`、`FriendList`、`QqFilterConfig`、`QqGateMode`
- **sudo 授权**：`SudoRequest`（请用户输入密码）/ `SudoResolved`（关闭弹窗/toast；所有退出路径恰好一次，ADR-0012 + run_sudo guard）

## 工具事件的精确配对

- `ToolCall` / `ToolResult` 均携带 `tool_call_id`（provider 签发的调用 id），前端据此**精确配对**——同名并行调用不再配错对；旧 core 无此字段时回退按名匹配
- `ToolResult` 携带 `timed_out`：执行被外圈超时守卫中止（notice 语义）时置位，前端渲染为失败而非成功

## 时间线增量同步

- Core 维护单调 `timeline_seq`（新增条目与**就地更新**都推进）；`TimelineMessage` 带条目级 `seq`
- `TrunkTimeline` 响应携带 `full` 标志：`true` = 完整快照（替换缓存），`false` = 相对 `since_seq` 的增量（追加/按 `tool_call_id` 就地修补）；前端不再凭 seq 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）
- 增量窗口不完整（条目滚出 1024 上限、游标超前于当前序号、重启前旧游标）时回退全量
- 前端按 agent 缓存时间线（`teamTimelines`），切换 agent 时**先用缓存渲染**，再以 `since_seq` 拉增量

## 归属与路由

- `TrunkTimeline` 响应携带 `team_id`（= 请求值），前端按响应归属路由缓存/视图，**不用当前 activeTeamId 猜测**
- 非默认 agent 的实时事件经 `event_bus` 镜像到主 agent 连接；`MessageReceived` / `AgentReasoning` / `AgentOutput` / `ToolCall` / `ToolResult` / `AgentThinking` 均由 `annotate_team` 注入 `team_id`，前端按当前 team 过滤实时事件（跨 agent 不串显）

## 共享枚举

| 类型 | 取值（线格式，snake_case） |
|---|---|
| `GateMode` | `"none"` / `"allowlist"` / `"denylist"` |
| `ThinkingMode` | `"enabled"` / `"disabled"` |
| `ReasoningEffort` | `"low"` / `"high"` / `"max"` |

## 兼容性规则

1. **serde 表示即线格式**：变体名、字段名、rename 策略一律不得更改
2. 新增可选字段必须 `#[serde(default)]`（先例：`AgentOutput.branch_id`、`ToolResult.tool_call_id`——旧 Core 缺字段时新 Panel 按默认值解码，有回归测试守护）
3. 新增事件/命令变体：旧端遇到未知变体整条帧丢弃（不崩溃），前端应做能力兜底
4. 进程内 mpsc bridge（`create_bridge` / `BackendBridge` / `BackendHandle` / `FanoutHandle`）与 WS 共享同一组类型，主要用于测试；生产部署一律走 WS

## 独立通道

- **sudo 通道**：密码经专用帧提交（不进入 LLM 上下文/会话日志），事件为 `SudoRequest/SudoResolved`
- **QQ OneBot**：QQ 适配器走反向 WS `:3131`（与 management WS 独立），QQ 消息以 `<qq_message_hook>` 标记进入 agent
