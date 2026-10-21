---
id: protocol
title: "协议与数据流"
group: 工程
x: 1920
y: 1790
---

# 协议与数据流

> **定位**：本文是前后端线协议（management WebSocket 契约）的参考——契约归属与兼容规则、传输与信封（含事件扇出序号、Bootstrap、共享枚举）、命令、事件（含时间线增量同步）、归属与路由、管理通道与快照。读者：Core / Panel 双端开发者与协议维护者。
> 相关文档：[架构总览](../overview/architecture.md)、[Panel 交互定义](../panel/panel-interaction.md)、[桌面端布局](../panel/panel-desktop-layout.md)、[联邦](../federation/federation.md)、[部署与自更新](../ops/ops-deploy.md)。

前后端通过 **management WebSocket**（默认 `0.0.0.0:3132`，开箱即可被局域网连接；`[core] management_access_token` 提供 Bearer 认证，首装自动生成）通信，消息为 JSON 文本帧。令牌缺失/不匹配时在握手阶段返回**标准 HTTP 401**（客户端据此归类为认证失败并提示排查令牌——此前该拒绝路径因响应状态默认 200 被 tungstenite 静默丢弃，客户端只能看到「握手未完成」，令牌问题极难排查）。命令与事件定义在 `source/protocol/echo-protocol`（Core 侧直接使用该 crate；Panel 以 `web/src/protocol.ts` 镜像线格式）——**类型定义的唯一来源是该 crate，修改协议必须先改 crate**；本文档是对它的说明性描述。

## 仓库拆分与契约归属

- 仓库拆分为 **EchoAgentCore**（后端 agent 服务）与 **EchoAgentPanel**（前端）两个独立仓库，两端可独立构建、发布、演进，仅通过 `echo-protocol` 契约耦合
- `echo-protocol` 是前端 ⇄ Core **线契约的唯一来源**：只定义 `BackendCommand` / `BackendEvent` / `WsMessage` / bridge / 共享枚举（`ThinkingMode` / `ReasoningEffort` / `LoopMode`），不依赖任何 agent、平台或 UI 代码；serde 表示即线格式
- Panel 是纯粹的「协议客户端」：不包含任何 agent 或 QQ 逻辑（Rust 中继按原文转发帧、不解析负载；线格式以 `web/src/protocol.ts` 镜像）；后端各 crate 通过 re-export 保持 `echo_agent::…` 路径兼容
- 协议演进只服务 Panel ⇄ Core 两端；**全网同步更新（不做旧版本兼容）**——语义变更单侧收紧亦可，两端同版本部署（见 §兼容性规则）

### 兼容性规则

1. **serde 表示即线格式**：变体名、字段名、rename 策略一律不得更改
2. **全网同步更新（不做旧版本兼容）**：字段/变体变更可直接收紧（如去 `serde(default)`）；`serde(default)` 仅保留给语义上真正可省略的字段（`Option` + `skip_serializing_if` 等）；两端同版本部署，不设新旧混部容忍
3. 新增事件/命令变体时两边同时合入；未知变体由 serde 整帧丢弃（不崩溃）
4. 进程内 mpsc bridge（`create_bridge` / `BackendBridge` / `BackendHandle` / `FanoutHandle`）与 WS 共享同一组类型，主要用于测试；生产部署一律走 WS
5. **信封字段与事件契约分离**：`seq` / `entry` 是 WS 帧的**顶层**信封字段，不属于 `BackendEvent` 任何变体——增删它们不触发变体兼容规则，也不会让事件反序列化失败（缺省时消费端走 `ignored` 分支 / 回退启发式）；同理，信封级元数据不应塞进事件变体
#### 协议名迁移（2026-10，已完成）

Core 在 `2d46b7d` 把「适配器」这一平台专用抽象整体改名为「插件实例」，**线协议变体名同步改了**，
Panel 随后在同批迁移中跟齐（Panel `7e8f765`）：

| 旧 | 新 |
| --- | --- |
| `StartAdapter` / `StopAdapter` / `RestartAdapter` | `StartInstance` / `StopInstance` / `RestartInstance` |
| `RequestAdapterStatus` | `RequestInstanceStatus` |
| `StartAllAdapters` / `StopAllAdapters` | `StartAllInstances` / `StopAllInstances` |
| `AdapterList` / `AdapterState` / `AdapterOpResult`（事件） | `InstanceList` / `InstanceState` / `InstanceOpResult` |
| `AdapterStateChanged { adapter_name }` | `InstanceStateChanged { instance_name }` |
| `AdapterStatus`（结构） | `InstanceStatus` |

**没有变体级 alias**：serde 的外部标签就是变体名，字段级 alias（`InvokeInstanceOp.instance` 的
`alias = "adapter"`）救不了它。所以这次改名必须双端同批落地——中间状态下的失败方式极难排查：

- **Panel → Core**：`{"StartAdapter":{…}}` 解析失败 → `echo-plugin-management` 的 `handle_inbound_text` 返回
  `None` → **整帧静默丢弃**（该函数注释即「malformed payloads → ignored」）。表现是「点击启停
  没有任何反馈」，日志里也不会有红色错误。
- **Core → Panel**：Core 只发 `Instance*`，Panel 的 `case` 还在等旧名 → 实例状态、登录态、
  门控快照全部收不到。

**防回归**：两端各有一条契约测试钉住名字——Core 的
`command::tests::every_broadcast_readonly_name_is_a_real_command_variant` 与 Panel 的
`protocol-sync.test.ts`（7 个命令名 + 4 个事件名逐字断言，且旧名必须归零）。


## 传输与信封

- Core 监听 `[core] management_address`（默认 `0.0.0.0:3132`），由 `echo-plugin-management` 提供；每个前端建立一条 WS 连接
- Core 侧事件经 `EventBroker` 扇出到每条连接的独立订阅通道；命令由各连接独立注入
- 服务端 30s Ping 心跳（90s 无任何入站活动判死断开）；Panel 转发层对双向心跳做透传，空闲连接不会被中间层悄悄断开
- 断线自动重连（500ms 起、指数退避，**上限按页面可见性分档**：可见 3s / 隐藏 30s——部署或 Core 重启后前台几秒内即恢复，后台省电）；重连后清空运行期状态（分支/任务/活动/工作区缓存）再 Bootstrap——**时间线刻意保留**（刷新加速：内存 + 磁盘缓存按游标 `since_seq` 增量续拉，Core 缺口检测兜底、响应 `full` 时整体替换）

```rust
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WsMessage {
    Command(BackendCommand), // Panel → Core
    Event(BackendEvent),     // Core → Panel
}
```

```json
// Panel → Core
{"type":"command","payload":{"SendMessage":{"session_id":"...","content":"..."}}}
// Core → Panel（事件帧另带扇出序号 seq，见 §事件扇出序号）
{"type":"event","payload":{"AgentOutput":{"session_id":"...","content":"...","branch_id":null}},"seq":4127}
```

序列化助手：`serialize_command` / `serialize_event` / `serialize_event_sequenced` / `serialize_event_identified` / `deserialize_message`（`echo_protocol::bridge`）；无法解析的帧记日志后丢弃。

### 事件扇出序号

Core → Panel 的每条事件帧带一个**顶层** `seq` 字段（`SequencedEvent { seq, event }`，`echo-protocol/src/bridge.rs`）：

- **语义**：Core **进程内**单调递增序号（从 1 起，重启归 1），标识"这条 Core 发出的第 N 条事件"，不标识任何业务对象。由 WS 扇出点 `echo-plugin-management` 的 `EventBroker` 在**持订阅者表锁之前** `fetch_add` 分配，因此编号与投递顺序严格一致——多订阅者（多窗口 / 多 Panel / 中继的多个上游连接）看到同一套编号，不会错位
- **编码**：`serialize_event_sequenced` 把序号作为顶层字段附加（`{"type":"event","payload":{…},"seq":N}`）；`serialize_event` 保留不带序号的形态，两者共用同一实现。放顶层而不是塞进 `BackendEvent` 变体，是为了不动事件契约——新增事件变体不必记得带序号，旧 Panel 的 serde 也只会忽略这个多余字段
- **条目身份**：`serialize_event_identified` 在序号之外附加 `entry`（`{"type":"event","payload":{…},"seq":N,"entry":"…"}`）。语义 = **同一条逻辑条目的所有事件共享同一 id**：一段流式输出的全部增量与它的终值同 id（`backend:<session>:<branch>:<n>` / `reasoning:<session>:<branch>:<n>`），工具调用与它的结果同 id（`tool:<session>:<tool_call_id>`，id 为空时不分配）。`<n>` 是 Core 进程级单调计数。由扇出层（`echo-plugin-management::EventBroker` 持有的 `echo_protocol::entry_identity::EntryTracker`）在**持锁投递之前**推进，因此多订阅者看到同一套 id；缺省（旧版 Core / 无身份概念的事件）时不写该键，Panel 全部消费点回退原判据
- **时间线分页**：首屏 `RequestTrunkTimeline { since_seq, limit }` 只取**最新** N 条（渲染窗口通常 200 级），响应 `TrunkTimeline { …, has_more }` 表示是否被截断；更早的内容按 `RequestTimelineEarlier { team_id, before_ord, limit }` → `TimelineEarlier { messages, has_more }` 向上翻页——`before_ord` 是已加载最老条目的**位置序号**（`TimelineMessage.ord`，push 时分配且永不改变），消费端把 `messages` **前插**到已加载列表头部、**不动同步游标**。**锚点不能用 `seq`**：运行中的工具条目会被就地更新并推进 `seq`（位置不变、序号变大），实测 1024 条里 46 处非单调，按 `seq` 定位会过早截断并漏条目（逐页拼接只还原 842/1024）；也不能用「已加载条数」——分页期间的新增会让「从尾部数 N 条」整体错位。`limit` 服务端 clamp 1–500。两条命令分开的理由：`TrunkTimeline.full` 的语义是「替换缓存」，而翻页是「前插」——混在一条里消费端难以判断该替换还是该前插。`limit` 只作用于全量路径：增量是缺口补齐，截断会让游标与实际内容不一致。
- **流式增量合批**：扇出层（`echo-plugin-management::EventBroker`）在 **60 ms 窗口**内把**连续同键**增量（会话 + 分支 + 角色）合并成一帧再下发（`echo_protocol::delta_batch::DeltaBatcher`）。语义不变——合并后的 `delta` 就是原增量的顺序拼接，消费端本来就是「追加到当前流式行」，逐字到达与成块到达的最终文本一致；条目身份也随合并后的帧保留。三条硬约束：① 只合并**连续同键**增量，不同分支 / 不同 entry 的文本绝不进同一帧（否则消费端分不清属于哪一行）；② 任何**非增量**事件到达时先冲出全部挂起再加它自己——工具开始是「分步边界」（Panel 的 `freezeStreams`），不能让它越过前面的叙述；③ deadline 从**第一条**增量算起（不随合并顺延），最坏延迟 = 窗口 + 20 ms 检查间隔。合批只发生在前端扇出这一层：时间线投影等进程内观察者收到的仍是原始事件（它们本就忽略增量）
- **消费方契约（Panel）**：按 `(core, seq)` 分桶观察（`web/src/event-seq.ts` 的 `observeEventSeq`）——`baseline` 首帧建基线、`contiguous` 连续、`gap` 跳号（`missed` = 缺失条数）即**按 timeline 游标补拉**（`RequestTrunkTimeline{ team_id, since_seq }`，`connection.ts` 的 `requestTimelineResync`，带最小间隔限流）、`reset` 序号回退（Core 重启归 1、中继补发旧帧）只重取基线不报警、`ignored` 帧上无序号（旧 Core）跳过校验。判定的职责边界：只观察不修复，修复动作由连接层发起
- **为什么要它**：中继半死、重连窗口、标签页后台节流都可能让事件**静默丢失**；没有序号时前端无从得知，某条 `ToolResult` 丢了工具行就永远停在「运行中」，只能刷新页面。回合收尾兜底只能止住读秒，救不回丢失的内容（工具输出、回答片段、列表更新）
- **与时间线条目 `seq` 无关**：帧序号标识 Core → Panel 的事件流连续性（进程内、重启归 1），`TimelineMessage.seq` 标识时间线条目（`RequestTrunkTimeline{since_seq}` 用的是它）；两者不可互作游标
- **只在 WS 扇出点分配**：进程内 mpsc bridge（`FanoutHandle`）不带序号，测试路径与生产路径的差异仅此一处

### 连接与 Bootstrap

连接成功后 Panel 发送 Bootstrap 命令组：`RequestState`、（有保存的 Agent 时）`RequestTrunkTimeline{team_id}`、`RequestInstanceStatus`、`RequestTeamsList`、`RequestShellSessions`。去主智能体后 `team_id` 必填且 QQ 不预取（详见 [Panel 交互定义](../panel/panel-interaction.md)、[桌面端布局](../panel/panel-desktop-layout.md) §一）。

### 共享枚举

`ThinkingMode` / `ReasoningEffort` / `LoopMode` 定义在 `echo-defs::mode`（Service Definition 层），`echo-protocol` re-export 保持 wire 路径：

| 类型 | 取值（线格式，snake_case） |
|---|---|
| `ThinkingMode` | `"enabled"` / `"disabled"` |
| `ReasoningEffort` | `"low"` / `"high"` / `"max"` |

> **门控模式不是共享枚举**（泛化后）：历史上 `GateMode`（`none`/`allowlist`/`denylist`）是 `echo-protocol` 的强类型枚举，现已随 QQ 专属命令一并物理删除——门控取值改为 `InvokeInstanceOp{method:"set_gate_mode", payload:{mode}}` 的**参数字符串**，由实例插件自行解释，Core 与 Panel 均不校验取值集合。

`LoopMode` 定义在 `echo-defs::mode`（经 `echo-protocol` 再导出）：per-persona 循环模式，由 `enabled_plugins` 对互斥插件 `echo-agent.loop.{single,parallel}` 推导（单会话为默认与兜底；插件黑名单 `disabled_plugins` 已移除，`SaveTeam`/`TeamInfo` 不再携带该字段——旧端帧中的该字段被 serde 忽略，缺省按空表处理）。**协议变更**：`TeamInfo` 携带 `loop_mode`（`"single"`/`"parallel"`）；面板 `loopModeOf()` 缺省按 single。

## 命令（Client → Core）

`BackendCommand` 主要分四类：

- **会话类**：`SendMessage`、`CancelRequestedWork`、`ClearHistory`（**`session_id` 可选**：存在 = 仅清该会话的事件/投影/时间线条目，缺省 = 清空该人格全部会话——此前该命令无会话维度、点一次误清整人格）、`ArchiveHistory`（**要求 `session_id`**：仅归档并清空该会话——此前无会话维度、点一次误清整人格；归档数据为内存快照、保留最新 20 份；缺失 = 拒绝（fail-closed：防降级为旧的全量清空））、`CompactHistory`（协议不变：`keep_recent` 默认 40、clamp 10–500；服务端自动先归档再按会话生成 LLM 摘要，失败回退统计文案，见 [架构 §会话与持久化](../overview/architecture.md)）、`RequestTrunkTimeline`（支持 `since_seq` 增量）——**`team_id` 语义必填**（破坏性变更：无"主/默认智能体"；线格式上为 `Option<String>` + `#[serde(default)]`，缺失时由服务端运行期校验回 `Error`，协议层不做反序列化拒绝）；`RequestContext` 增 `session_id`（多会话：返回该会话的上下文快照，缺省回退本地 TUI 会话；`ContextSnapshot` 回带 `session_id`）
- **Shell 类**：`RequestShellSessions`（可选 `team_id` = 只看该 persona 的会话）/ `ShellStart` / `ShellExec` / `ShellStop`（后台持久 bash，见 [工具系统](../plugins/core-tools.md)；会话按 persona 归属，列表事件回带 `team_id`）
- **资源类**：`RequestSkillsList/ToolsList/PluginsList/TeamsList`、`ToggleSkill/Tool/Plugin`、`Save/DeleteSkill`（`SaveSkill.system` 声明系统提示词技能）、`ReloadSkills`（重载技能目录——广播所有运行中人格 + 管理代理，见 [技能系统](../plugins/core-skills.md)）、`InstallSkillFromGit`/`UpdateSkillFromGit`/`RemoveSkillSource`（Git 来源技能，见 [技能系统](../plugins/core-skills.md)）、`SaveTeam/DeleteTeam/ToggleTeam`（`SaveTeam.system_skills` 声明人格系统提示词技能；`TeamInfo.system_skills` / `SkillInfo.system` 随列表事件下发；`SaveTeam.api_profile` / `TeamInfo.api_profile` 声明与回推人格级 API 供应商引用；`PluginInfo.package` 回推插件所属包——横跨 plugin+tool+skill 的组合标签）
- **运维类**：
  - 插件实例：`Start/Stop/RestartInstance`（按 `name` = 插件实例名寻址）、`StartAllInstances`/`StopAllInstances`（全部插件实例）；**管理面统一通道 `InvokeInstanceOp { instance, method, payload }`**——一切平台专属动作（门控 / 名单 / owner / 登录 / 目录查询）经 `method` 透传（插件侧 `custom` op），响应 `InstanceOpResult`；状态经 `InstanceState` 推送。`instance` 为**必填**实例名（泛化；旧 QQ 专属命令变体已物理删除，旧客户端请迁移）
  - **线字段兼容**：`instance` 接受旧名 `adapter`（`#[serde(alias = "adapter")]`，2026-10 去适配器化）——**但这只救字段名，救不了变体名**：`StartAdapter` 这类旧命令 Core 已不认识，Panel 必须同批升级（见 §协议名迁移）。同理插件协议的 `IncomingMessage.instance` 也带同一别名，插件宿主回呼的 `submit_input` / `report_*` 载荷则**双读**（`instance` 优先、回退 `adapter`）
  - 工作区：`RequestWorkspaceSessions` / `SaveWorkspaceSession` / `DeleteWorkspaceSession` / `ActivateWorkspaceSession` / `RequestWorkspaceGitStatus` / `RequestWorkspaceFiles`——会话类命令，**`team_id` 语义必填**（线格式同为 `Option<String>` + 服务端校验）并按 persona 路由；状态即改即存（`echo-workspaces-{id}.json`），git 与文件列表为只读采集（`RequestWorkspaceFiles.path` 以 canonical 前缀校验限定在会话目录及其子孙内，越界返回 `WorkspaceFiles.error`）。**激活 = 进入项目对话通道**（重定义）：前端「本地当前对话」按 `active` 投影——选通道 = 激活、选默认本地会话 = 取消激活；`active` 变化必须广播 `WorkspaceSessions`
  - API：`UpdateApiConfig/SwitchApi/TestApi/QueryApiBalance/QueryApiMetrics/DeleteApi/RenameApi`（`SwitchApi` 全局激活已被 persona 级选用取代——`SaveTeam.api_profile` 引用供应商池；该命令仍可经协议调用，面板 UI 不再暴露；`QueryApiBalance` 查 DeepSeek 官方 `/user/balance`，回 `ApiBalanceResult`〔含 `granted`/`topped_up` 分账〕；`QueryApiMetrics` 读本地积累的余额/用量时间序列〔近 7 天〕，回 `ApiMetrics`；`RenameApi{from,to}` 原子改名——单次 ConfigStore patch 重写池名/`active_api`/`teams[*].api_profile` 并重绑运行中引用，失败零写入，语义见 [配置持久化](../core/core-config-persistence.md)§改名语义）

### 多接入点聚合：可广播只读命令

广播由**客户端显式声明**，中继不再按命令名猜只读（变更）：

```json
{"broadcast": true, "frame": {"type": "command", "payload": {"RequestTeamsList": null}}}
```

- Panel 前端以 `broadcast: true` 信封发送发现类命令（`RequestState` / `RequestTeamsList` 等），中继见该标志即转发给**全部在线上游**；不带标志的命令按信封/会话归属路由到单一目标节点，不会扇出。
- 中继侧实现见 `echo-web-server/src/proxy.rs`（`{"broadcast": true, "frame": …}` 分支）与 `web/src/connection.ts`（`broadcastEnvelope`）。**中继端当前不维护只读白名单**——判断权在前端调用方；新增可广播命令只需在前端用该信封发送，中继零改动。
- Core 侧 `BROADCAST_READONLY_COMMANDS` / `is_broadcast_readonly()`（`echo-protocol/src/command.rs`）保留为**命令分类的语义快照**（供测试与文档引用），不再是中继的运行时开关。

### 完整变体清单

按功能域列出全部 `BackendCommand` 变体（载荷字段见 `echo-protocol/src/command.rs`）：

| 域 | 变体 |
|---|---|
| 会话 | `SendMessage`、`CancelRequestedWork`、`ClearHistory`、`ArchiveHistory`、`CompactHistory`、`MigrateSession`、`RequestTrunkTimeline`、`RequestTimelineEarlier`（更早一页，按 `before_ord` 锚定、`limit` clamp 1–500）、`RequestSessionHistory`（按会话派生显示历史，`limit` 缺省 500 / clamp 1–2000）、`RequestContext`、`RequestState` |
| 会话级 API 覆盖 | `SetSessionApi`（该会话的 LLM 请求走指定 profile；`profile` 空串 = 清除）、`ClearSessionApi` |
| Shell | `RequestShellSessions`、`ShellStart`、`ShellExec`、`ShellStop` |
| 资源 | `RequestSkillsList`、`RequestToolsList`、`RequestPluginsList`、`RequestTeamsList`、`ToggleSkill`、`ToggleTool`、`TogglePlugin`、`RestartPlugin`、`SaveSkill`、`DeleteSkill`、`ReloadSkills`、`InstallSkillFromGit`、`UpdateSkillFromGit`、`RemoveSkillSource`、`SaveTeam`、`DeleteTeam`、`ToggleTeam` |
| 插件实例 | `StartInstance`、`StopInstance`、`RestartInstance`、`StartAllInstances`、`StopAllInstances`、`RequestInstanceStatus`、`InvokeInstanceOp` |
| 工作区 | `RequestWorkspaceSessions`、`SaveWorkspaceSession`、`DeleteWorkspaceSession`、`ActivateWorkspaceSession`、`RequestWorkspaceGitStatus`、`RequestWorkspaceFiles`、`RequestBrowseDirectories`（目录选择器浏览：列「浏览根」内子目录——与文件浏览器不同，不依赖会话已声明的工作区目录，避开「要浏览才能选、要选才能浏览」的循环） |
| API 供应商 | `UpdateApiConfig`、`SwitchApi`、`TestApi`、`QueryApiBalance`、`QueryApiMetrics`、`DeleteApi`、`RenameApi`、`SwitchModel`、`SwitchProvider`（模型/供应商直切，面板 UI 以 persona 级 `api_profile` 选用为主路径） |
| 系统提示词 | `SetSystemPrompt`（替换 Core 插件持有的系统提示词）、`RequestSystemPrompt` |
| 联邦 | `RequestFederationStatus`、`RequestFederationInvite`、`SaveFederationPeer`、`DeleteFederationPeer`、`RequestRemoteApiProfiles`（查远端节点的**脱敏**供应商池，回 `RemoteApiProfiles`；api_key 明文永不过线） |
| 运维 | `RequestSelfUpdate`、`RequestSelfUpdateStatus` |

实例管理方法名（`set_gate_mode` / `update_allowlist` / `login_qrcode` …）由 `echo-defs` 的 `InstanceManagement` 定义。

## 事件（Core → Client）

`BackendEvent` 核心事件流（一次对话的完整时序）：

```prodoc-flow
graph LR
  A[用户消息<br>MessageReceived] --> C[系统提示词构建]
  C --> D[LLM 请求<br>LlmRequest]
  D --> E{有工具调用?}
  E -->|是| F[推理 AgentReasoning]
  F --> G[工具调用 ToolCall]
  G --> H[工具结果 ToolResult]
  H --> D
  E -->|否| I[正式回答 AgentOutput]
```

> 分支事件 `ReplyBranchStarted` / `ReplyBranchCompleted` 只在 **parallel 循环模式**下发（`Agent::shows_reply_branches()` 仅对 `LoopMode::Parallel` 为真）；默认的 `single` 模式不注册分支，事件流是 `MessageReceived → … → AgentOutput → AgentCompleted`。

其他事件分组（字段详见 `echo-protocol/src/event.rs`）：

- **插件实例生命周期与管理**：`InstanceStateChanged`（连接态）、`InstanceList`（实例/能力/UI 描述符快照）、`InstanceOpResult`（`InvokeInstanceOp` 响应）、`InstanceState`（状态文档推送）
- **编排**：`SubagentStarted/Completed`、`ReplyBranchStarted/Content/Completed`、`AgentCompleted`（turn 收尾——含空输出的完成信号）；**推理有两条通路**：实时 `AgentReasoning`（及 `AgentReasoningDelta` 流式）之外，正文同时以 `SessionEvent::Reasoning` 落会话事件日志（**模型不可见**）——历史回填与实时投影形状因此一致，思考条目不会在刷新后被重排（见 [会话记忆](../core/core-memory.md)§可见性边界）
- **流式输出**：`AgentContentDelta` / `AgentReasoningDelta`（逐段增量，~40ms 合并批；终值仍以 `AgentOutput` / `AgentReasoning` 全量收敛——消费端按 (core, session, branch) 对账，全量到达替换流式内容）。受 `[agent].stream_output` 控制（缺省开）。消费方关闭时 provider **即中止上游读流**（巡检：此前静默读完整流白烧 token；接收端 drop 后首个增量即断，附假 SSE 服务器回归测试）。
- **Shell**：`ShellSessionsList` / `ShellSessionStarted` / `ShellExecStarted` / `ShellExecOutput`（流式）/ `ShellExecDone` / `ShellSessionClosed`
- **状态快照**：`SessionUpdated`、`ContextSnapshot`、`TrunkTimeline`、`SessionHistory`（某会话的显示历史派生：`messages` / `total` / `truncated` / `max_seq`；`max_seq` 是**合并水位线**，显示端按 `seq ≤ max_seq` 判"快照已覆盖"，推理条目不计入——见 [会话记忆](../core/core-memory.md)§显示时间线）、`ApiConfigUpdated`、`ApiProfilesUpdated`、`ApiTestResult`、`ApiBalanceResult`（`granted`/`topped_up` 分账字段恒下发，失败时为空串）、`ApiMetrics`（余额/用量时间序列——`entries: [{name, balance, usage, cost_currency}]`，见 `event.rs` 的 `ApiMetricsEntry`）、`Error`（也用于信息性 toast）
- **实例管理（泛化后）**：状态统一经 `InstanceState.state` 文档；查询结果经 `InstanceOpResult.result`。旧 QQ 事件（`GroupList` / `FriendList` / `QqFilterConfig` / `QqGateMode` / `QqLoginStatus` / `QqQrcode` / `QqOwner`）已物理删除
- **工作区会话**：`WorkspaceSessions`（列表 + 激活标记，`team_id` 归属；**`active` 是项目通道的单一事实来源**——面板/工具 `use` 等所有激活来源都必须广播，前端据此切换本地对话投影）、`WorkspaceGitStatus`（某会话各目录的 git 快照：分支 / 领先落后 / 暂存·修改·未跟踪计数 / 最近提交 / 变更文件列表 / 错误）、`WorkspaceFiles`（某目录一层文件列表：条目含 name/path/is_dir/size，目录在前；隐藏项跳过、超 500 条截断；失败经 `error` 回传——文件浏览器数据源）

### 工具事件的精确配对

- `ToolCall` / `ToolResult` 均携带 `tool_call_id`（provider 签发的调用 id），前端据此**精确配对**——同名并行调用不再配错对
- `ToolResult` 携带 `timed_out`：执行被外圈超时守卫中止（notice 语义）时置位，前端渲染为失败而非成功

### 时间线增量同步

- Core 维护单调 `timeline_seq`（新增条目与**就地更新**都推进）；`TimelineMessage` 带条目级 `seq`
- `TrunkTimeline` 响应携带 `full` 标志：`true` = 完整快照（替换缓存），`false` = 相对 `since_seq` 的增量（追加/按 `tool_call_id` 就地修补）；前端不再凭 seq 大小猜测（全量误当增量会整段重复，空增量误当全量会清空聊天）
- 增量窗口不完整（条目滚出 1024 上限、游标超前于当前序号、重启前旧游标）时回退全量
- 前端按 agent 缓存时间线（`teamTimelines`），切换 agent 时**先用缓存渲染**，再以 `since_seq` 拉增量

### 完整变体清单

按功能域列出全部 `BackendEvent` 变体（字段见 `echo-protocol/src/event.rs`）：

| 域 | 变体 |
|---|---|
| 会话与时间线 | `MessageReceived`、`AgentCompleted`、`SessionUpdated`、`ContextSnapshot`、`TrunkTimeline`、`TimelineEarlier`（更早一页，前端前插、不换同步游标）、`SessionHistory`（按会话派生显示历史；`max_seq` 为合并水位线，推理条目不计入）、`ActiveTurnsSnapshot` |
| 模型与工具 | `AgentThinking`、`AgentReasoning`、`AgentReasoningDelta`、`AgentOutput`、`AgentContentDelta`、`LlmRequest`、`LlmResponse`、`ToolCall`、`ToolResult` |
| 分支与子代理 | `ReplyBranchStarted`、`ReplyBranchContent`（**保留变体，当前无生产发射方**——「临时等待回复」机制已删除；分支正文走 `AgentContentDelta` 的 `branch_id`）、`ReplyBranchCompleted`、`SubagentStarted`、`SubagentCompleted` |
| Shell | `ShellSessionsList`、`ShellSessionStarted`、`ShellExecStarted`、`ShellExecOutput`、`ShellExecDone`、`ShellSessionClosed` |
| 资源清单 | `SkillsList`、`ToolsList`、`PluginsList`、`TeamsList`、`PluginOpResult`（插件操作回执：`{id, op, ok, message}`） |
| API 供应商 | `ApiConfigUpdated`、`ApiProfilesUpdated`、`ApiTestResult`、`ApiBalanceResult`、`ApiMetrics`、`RemoteApiProfiles`（联邦远端节点的**脱敏**供应商池；peer 离线/未实现查询时 `profiles` 为空、`error` 带原因） |
| 插件实例 | `InstanceStateChanged`、`InstanceList`、`InstanceOpResult`、`InstanceState` |
| 工作区 | `WorkspaceSessions`、`WorkspaceGitStatus`、`WorkspaceFiles`、`BrowseDirectories`（`RequestBrowseDirectories` 响应，目录选择器数据源） |
| 提示词与清单 | `SystemPrompt`（当前系统提示词插件文本）、`ChecklistUpdated`（工具发布的结构化可视化快照，如清单工具 → 清单面板） |
| 联邦与运维 | `FederationStatus`、`FederationInvite`、`SessionMigrated`、`SelfUpdateStatus`、`Error` |


## 归属与路由

- **无「主智能体」**：所有智能体平等；进程级职责（管理面、全局命令、插件宿主）由**核心服务代理**（非人格）承担，会话类命令必须显式 `team_id`
- `TrunkTimeline` 响应携带 `team_id`（= 请求值），前端按响应归属路由缓存/视图，**不用当前 activeTeamId 猜测**
- 所有人格的实时事件直投进程级事件汇聚点（`EventSink`），Panel 单连接收到全部；`MessageReceived` / `AgentReasoning` / `AgentOutput` / `ToolCall` / `ToolResult` / `AgentThinking` 均由 `annotate_team` 注入 `team_id`，前端按当前 team 过滤实时事件（跨 agent 不串显）
- **接入点 core 归属（Panel 中继侧）**：多接入点时每个 Core 节点的事件经中继加壳 `{core, frame}`——**core 归属是 Panel 中继信封层的概念，不在本协议帧内**（Core 自身不感知接入点数量，帧格式与单接入点完全一致）；身份因此升级为 `(core, id)`：会话 id（`local:tui::local_user`）与人格 id 在每个节点上可重复，前端一律按复合键路由（详见 [Panel 概览](../panel/panel.md) §接入联邦网络）

## 管理通道与快照

本组为管理面与附属通道的协议约定——媒体引用、独立链路（QQ OneBot / 联邦）、自更新与刷新恢复快照。

### 媒体引用

时间线/事件里的图片是**引用**而非内嵌数据：`/media/<id>`（Core 媒体库，
Panel web 后端同源提供）。渲染侧（Panel）直接 `<img src="/media/...">` 懒加载；
模型侧在投影出口还原为 data URI（`echo-defs::media_store`），LLM 请求不变。
遗留会话的内嵌 data URI 由 Core 加载期迁移落盘（幂等）；落盘失败的图退化为
空串占位（前端渲染「图片已省略」，保留数组长度以便计数）。

### 独立通道

- **QQ OneBot**：每个 QQ 实例一条反向 WS（legacy `:3131`；多实例由**插件**在 `3140-3190` 内自动分配、持久化到实例 state 文件的 `[deploy.ports]`，与 management WS 独立——Core 不感知端口），QQ 消息以 `<qq_message_hook>` 标记进入**实例归属人格**

### 联邦管理

Core↔Core 联邦（`echo-federation` crate，惯例端口 :3133；`[federation].listen` **缺省合并至管理端口**——单监听按令牌分流，显式地址 = 独立监听、显式空串 = 纯连出）是**独立链路**——与
Panel 的 management WS 并存，帧类型为 `FedFrame`（工具调用 Invoke 三段式、
委派 SubagentSpawn/Event、只读查询 Query/QueryResult）。契约见
[联邦](../federation/federation.md)；演进规则与本协议一致（externally-tagged +
`#[serde(default)]`）。

Panel 侧的联邦管理命令（Frontend-only）：`SaveFederationPeer` /
`DeleteFederationPeer` / `RequestFederationStatus` /
`RequestFederationInvite` / `MigrateSession { session_id, team_id?, target_peer }`
——Panel 侧全部镜像进 `web/src/protocol.ts`（补齐；unit 变体
字符串形态，双侧各有形态契约测试：Core 本仓 `echo-plugin-management`、Panel
`protocol-sync` 守护「禁止双重断言绕过」）；
事件 `FederationStatus`（token 脱敏为 `token_set`；含 per-peer
`active_turns` 负载，在线 peer 实时拉取）/ `FederationInvite` /
`SessionMigrated { session_id, target_peer, new_session_id?, message,
success }`。邀请串格式 `echofed://host:port?name=<别名>#<token>`
（一次性，配对成功自动清理）。

### 自更新

Panel 侧的框架自更新命令（Frontend-only）：`RequestSelfUpdate`（触发——
启动 `echo-agent-core-update.service` oneshot 单元，脱离 agent 进程执行
构建→替换→重启；Graceful Drain 保当前回复完整）/
`RequestSelfUpdateStatus`（查询）；事件 `SelfUpdateStatus { state, phase?,
revision?, message?, updated_at? }`（state: idle/running/restarting/updated/
failed/rolled_back；phase: initializing/building/installing/restarting/
verifying）。

### 刷新恢复快照

运行状态（thinking/tool/subagent）本是瞬时事件流，刷新重连后前端只靠
`TrunkTimeline` 看不到正在运行的会话。Core 在响应
`RequestTrunkTimeline` 后立即补发 `ActiveTurnsSnapshot { session_ids,
team_id }`——前端清空本地 activity 按列表重建 running 集合。
