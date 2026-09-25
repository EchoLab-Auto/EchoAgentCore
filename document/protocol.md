---
group: 协议模块
x: 591
y: 442
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
    MenuAnswer(MenuAnswerSubmit),     // Panel → Core（专用选单通道）
}
```

```json
// Panel → Core
{"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
// Core → Panel
{"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"...","branch_id":null}}}
// Panel → Core（sudo 密码；仅此通道，绝不走 Command）
{"type":"sudo_password","payload":{"request_id":1,"password":"***"}}
// Panel → Core（选单应答；仅此通道，不开启新 turn）
{"type":"menu_answer","payload":{"request_id":1,"option_id":"b"}}
```

序列化助手：`serialize_command` / `serialize_event` / `deserialize_message`（`echo_protocol::bridge`）；无法解析的帧记日志后丢弃。

> **sudo 密码安全约定**：`SudoPasswordSubmit.password` 是 `Option<String>`（`Some` 授权 / `None` 拒绝）。密码帧由 management server **直接路由到 sudo broker**，不经过 agent 命令队列、会话日志与 LLM 上下文；专用反序列化器失败时只记错误、不记原始文本，`Debug` 输出打码。

> **选单应答约定**：`MenuAnswerSubmit.option_id` 是 `Option<String>`（`Some(id)` 选定 / `None` 取消）。选单内容与选择都不是秘密，但**同样不走命令队列**——命令队列是"用户输入"语义（会开启新 turn），而选单应答只是回填等待中的那次 `present_menu` 工具调用（复用同一次模型上下文继续下一步）。帧由 management server 直连 menu broker（`serialize_menu_answer` / `deserialize_menu_answer`）。

## 连接与 Bootstrap

连接成功后 Panel 发送 Bootstrap 命令组：`RequestState`、（有保存的 Agent 时）`RequestTrunkTimeline{team_id}`、`RequestAdapterStatus`、`RequestTeamsList`、`RequestShellSessions`。去主智能体后 `team_id` 必填且 QQ 不预取（详见 [Panel 布局与导航](./panel-layout.md) §一）。

## 命令（Client → Core）

`BackendCommand` 主要分三类：

- **会话类**：`SendMessage`、`CancelRequestedWork`、`ClearHistory`、`ArchiveHistory`、`CompactHistory`（协议不变：`keep_recent` 默认 40、clamp 10–500；2026-09 起服务端自动先归档再按会话生成 LLM 摘要，失败回退统计文案，见 [架构 §会话与持久化](./architecture.md)）、`RequestTrunkTimeline`（支持 `since_seq` 增量）——**`team_id` 必填**（2026-09-13 破坏性变更：无"主/默认智能体"，缺失直接回 `Error`）；`RequestContext` 增 `session_id`（多会话，2026-09：返回该会话的上下文快照，缺省回退本地 TUI 会话；`ContextSnapshot` 回带 `session_id`）
- **Shell 类**：`RequestShellSessions` / `ShellStart` / `ShellExec` / `ShellStop`（后台持久 bash，见 [工具系统](./core-tools.md)）
- **资源类**：`RequestSkillsList/ToolsList/PluginsList/TeamsList`、`ToggleSkill/Tool/Plugin`、`Save/DeleteSkill`（`SaveSkill.system` 声明系统提示词技能）、`InstallSkillFromGit`/`UpdateSkillFromGit`/`RemoveSkillSource`（Git 来源技能，见 [技能系统](./core-skills.md)）、`SaveTeam/DeleteTeam/ToggleTeam`（`SaveTeam.system_skills` 声明人格系统提示词技能；`TeamInfo.system_skills` / `SkillInfo.system` 随列表事件下发；`SaveTeam.api_profile` / `TeamInfo.api_profile` 声明与回推人格级 API 供应商引用；`PluginInfo.package` 回推插件所属包——横跨 plugin+tool+skill 的组合标签）
- **运维类**：
  - QQ：`Start/Stop/RestartAdapter`、`UpdateQqAllowlist/Denylist`、`SetQqGateMode`、`SetQqOwner`、`RequestQqLoginStatus`、`RequestQqQrcode`——**均带可选 `adapter`（实例名，`#[serde(default)]`）**：给定 = 精确寻址该实例；缺省 = 唯一 QQ 实例时回退，多实例时报错要求显式指定（旧 Panel 单实例部署行为不变）。登录由 Core 代理（`QqLoginStatus`/`QqQrcode` 事件回推）
  - 工作区：`RequestWorkspaceSessions` / `SaveWorkspaceSession` / `DeleteWorkspaceSession` / `ActivateWorkspaceSession` / `RequestWorkspaceGitStatus` / `RequestWorkspaceFiles`——会话类命令，**`team_id` 必填**并按 persona 路由；状态即改即存（`echo-workspaces-{id}.json`），git 与文件列表为只读采集（`RequestWorkspaceFiles.path` 以 canonical 前缀校验限定在会话目录及其子孙内，越界返回 `WorkspaceFiles.error`）。**激活 = 进入项目对话通道**（2026-09-14 重定义）：前端「本地当前对话」按 `active` 投影——选通道 = 激活、选默认本地会话 = 取消激活；`active` 变化必须广播 `WorkspaceSessions`
  - API：`UpdateApiConfig/SwitchApi/TestApi/QueryApiBalance/DeleteApi`（2026-09：`SwitchApi` 全局激活已被 persona 级选用取代——`SaveTeam.api_profile` 引用供应商池；协议字段保留兼容，UI 不再暴露；`QueryApiBalance` 查 DeepSeek 官方 `/user/balance`，回 `ApiBalanceResult`）

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
- **Shell**：`ShellSessionsList` / `ShellSessionStarted` / `ShellExecStarted` / `ShellExecOutput`（流式）/ `ShellExecDone` / `ShellSessionClosed`
- **状态快照**：`SessionUpdated`、`ContextSnapshot`、`TrunkTimeline`、`ApiConfigUpdated`、`ApiProfilesUpdated`、`ApiTestResult`、`ApiBalanceResult`、`Error`（也用于信息性 toast）
- **QQ 管理**：`GroupList`、`FriendList`、`QqFilterConfig`、`QqGateMode`、`QqLoginStatus`、`QqQrcode`（二维码 PNG base64；均带 `adapter` 实例名）
- **工作区会话**：`WorkspaceSessions`（列表 + 激活标记，`team_id` 归属；**`active` 是项目通道的单一事实来源**——面板/工具 `use` 等所有激活来源都必须广播，前端据此切换本地对话投影，2026-09-14）、`WorkspaceGitStatus`（某会话各目录的 git 快照：分支 / 领先落后 / 暂存·修改·未跟踪计数 / 最近提交 / 变更文件列表 / 错误）、`WorkspaceFiles`（某目录一层文件列表：条目含 name/path/is_dir/size，目录在前；隐藏项跳过、超 500 条截断；失败经 `error` 回传——文件浏览器数据源）
- **sudo 授权**：`SudoRequest`（请用户输入密码）/ `SudoResolved`（关闭弹窗/toast；所有退出路径恰好一次，由 run_sudo guard 保证——工具侧链路见 [工具系统](./core-tools.md)）
- **选单（menu 插件）**：`MenuRequest`（`title` / `description?` / `options[{id,label,description?}]` / `timeout_secs`，Panel 在会话区渲染内联选单卡片）/ `MenuResolved`（`accepted` + 一句话 `message`；选定/取消/超时/中断全部恰好一次，由 present_menu guard 保证——Panel 选单卡片不会挂在死请求上）

## 工具事件的精确配对

- `ToolCall` / `ToolResult` 均携带 `tool_call_id`（provider 签发的调用 id），前端据此**精确配对**——同名并行调用不再配错对；旧 core 无此字段时回退按名匹配
- `ToolResult` 携带 `timed_out`：执行被外圈超时守卫中止（notice 语义）时置位，前端渲染为失败而非成功

## 时间线增量同步

- Core 维护单调 `timeline_seq`（新增条目与**就地更新**都推进）；`TimelineMessage` 带条目级 `seq`
- `TrunkTimeline` 响应携带 `full` 标志：`true` = 完整快照（替换缓存），`false` = 相对 `since_seq` 的增量（追加/按 `tool_call_id` 就地修补）；前端不再凭 seq 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）
- 增量窗口不完整（条目滚出 1024 上限、游标超前于当前序号、重启前旧游标）时回退全量
- 前端按 agent 缓存时间线（`teamTimelines`），切换 agent 时**先用缓存渲染**，再以 `since_seq` 拉增量

## 归属与路由

- **无「主智能体」**（2026-09-13）：所有智能体平等；进程级职责（管理面、全局命令、插件宿主）由**核心服务代理**（非人格）承担，会话类命令必须显式 `team_id`
- `TrunkTimeline` 响应携带 `team_id`（= 请求值），前端按响应归属路由缓存/视图，**不用当前 activeTeamId 猜测**
- 所有人格的实时事件直投进程级事件汇聚点（`EventSink`），Panel 单连接收到全部；`MessageReceived` / `AgentReasoning` / `AgentOutput` / `ToolCall` / `ToolResult` / `AgentThinking` 均由 `annotate_team` 注入 `team_id`，前端按当前 team 过滤实时事件（跨 agent 不串显）

## 共享枚举

三者定义在 `echo-defs::mode`（Service Definition 层），`echo-protocol` re-export 保持 wire 路径：

| 类型 | 取值（线格式，snake_case） |
|---|---|
| `GateMode` | `"none"` / `"allowlist"` / `"denylist"` |
| `ThinkingMode` | `"enabled"` / `"disabled"` |
| `ReasoningEffort` | `"low"` / `"high"` / `"max"` |
| `OrchestrationMode` | `"single"` / `"chatbot"`（`#[default] = chatbot`；旧字段过渡期下发，`TeamInfo.is_default` 恒 false） |

`LoopMode` 定义在 `echo-defs::mode`（经 `echo-protocol` 再导出）：per-persona 循环模式，由 `enabled_plugins` 对互斥插件 `echo-agent.loop.{single,parallel}` 推导（单会话为默认与兜底；插件黑名单 `disabled_plugins` 已移除，`SaveTeam`/`TeamInfo` 不再携带该字段——旧端帧中的该字段被 serde 忽略，缺省按空表处理）。**2026-09 协议变更**：`TeamInfo` 新增 `loop_mode`（`"single"`/`"parallel"`）；面板 `loopModeOf()` 优先读 `loop_mode`，缺省按 single。

## 兼容性规则

1. **serde 表示即线格式**：变体名、字段名、rename 策略一律不得更改
2. 新增可选字段必须 `#[serde(default)]`（先例：`AgentOutput.branch_id`、`ToolResult.tool_call_id`——旧 Core 缺字段时新 Panel 按默认值解码，有回归测试守护）
3. 新增事件/命令变体：旧端遇到未知变体整条帧丢弃（不崩溃），前端应做能力兜底
4. 进程内 mpsc bridge（`create_bridge` / `BackendBridge` / `BackendHandle` / `FanoutHandle`）与 WS 共享同一组类型，主要用于测试；生产部署一律走 WS

## 媒体引用（2026-09-24）

时间线/事件里的图片是**引用**而非内嵌数据：`/media/<id>`（Core 媒体库，
Panel web 后端同源提供）。渲染侧（Panel）直接 `<img src="/media/...">` 懒加载；
模型侧在投影出口还原为 data URI（`echo-defs::media_store`），LLM 请求不变。
遗留会话的内嵌 data URI 由 Core 加载期迁移落盘（幂等）；落盘失败的图退化为
空串占位（前端渲染「图片已省略」，保留数组长度以便计数）。

## 独立通道

- **sudo 通道**：密码经专用帧提交（不进入 LLM 上下文/会话日志），事件为 `SudoRequest/SudoResolved`
- **QQ OneBot**：每个 QQ 实例一条反向 WS（legacy `:3131`；多实例自动分配 `3140-3399`，与 management WS 独立），QQ 消息以 `<qq_message_hook>` 标记进入**实例归属人格**
