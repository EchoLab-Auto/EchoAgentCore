---
id: federation
title: "联邦（多机去中心化）"
group: 后端模块
x: 1600
y: 955
---

# 联邦（多机去中心化 Agent）

Core↔Core 对等链路：每台机器运行完整、平等的 Core 节点，agent 可以
跨机器的工作区执行工具、委派子任务、做只读查询。典型场景：联合调试
（A 机改代码、B 机起服务）、跨机部署验证、能力互补（GPU/专有环境）。

实现分布：`source/federation/echo-federation`（帧与链路）+
`source/backend/echo-agent/src/packages/federation`（执行层）+
`source/core/src/main.rs`（组合根装配与路由泵）。

## 始终开启（无 `enabled` 开关）

联邦已**取消开关、恒定开启**：

- 不配 `listen` → 不监听；无 `[federation.peers.*]` → 不连出。单节点部署因此
  **零网络暴露**，但联邦管理面（状态查询 / 邀请串 / 添加 peer）始终可用。
- `RequestFederationStatus` 恒回 `enabled: true` 的正常快照。
- 一旦配置 `listen` 或 peer，互信边界即生效（≈ SSH 免密）——务必 per-peer
  token + `allow_tools`/`allow_queries` 收敛。
- **自更新与联邦独立**（曾共用处理器，已解耦）。
- 旧配置里的 `enabled = false` 会被忽略（字段已移除）。

## 设计原则

1. **节点对等**：无主从。任何 Core 都能扮演「大脑」（持有 agent 循环与
   会话）或「执行节点」（暴露工具执行能力），角色按会话动态决定
2. **大脑集中、能力分布**：一个会话的 LLM 上下文与事件日志只存在于
   **一个**节点（大脑）——保留单写者模型，不做状态跨机同步；跨机的
   只有**工具调用**、**子任务委派**、**只读查询**三类 RPC
3. **边界上线，内核留进程内**：Ctx/EventBus/CancellationToken 等进程内
   基元不网络化；联邦协议只存在于边界（工具路由、委派、查询、扇出）
4. **显式命名**：远程工具以 `<peer>:<tool>` 注册，模型显式选择目标
   节点；同名本机工具不做隐式路由

## 命名空间

- **NodeId**：节点唯一标识（`node-<ulid>`），首次启动写入配置同目录
  `echo-node.json`（tmp+rename 原子写），此后稳定；`node_name` 提供
  人类可读别名
- **全局引用**：`node://<node_id>/<local-ref>`；本机省略 scheme。
  `SessionKey::parse` 剥离前缀按本机处理（现存调用方零感知），
  `parse_qualified` 保留节点归属
- **工作区目录**：`WorkspaceSessionInfo.directories` 为
  `#[serde(untagged)]` 的 `WorkspaceDirectory`（本机纯字符串 /
  `{path, node}` 表），读旧写新自动迁移；远程目录在提示词/git/文件
  浏览中标注为占位（不参与本机采集）

## 链路与协议（`echo-federation` crate）

每节点同时监听（`[federation] listen`，惯例端口 :3133；缺省空 = 不监听，纯连出）与按 peers 连出；
连接后角色对称。

- **握手**：Hello/Welcome（NodeId + 能力 `NodeCaps{tools, subagent,
  workspaces}` + `protocol_version`）——`PROTOCOL_VERSION` 不相等即拒绝
  （单一 `u32` 精确匹配，无主次版本结构）、自连回环拒绝、
  双向同时 dial 按 node_id 字典序让路（顶掉旧链路前显式 Down）
- **认证**：per-peer 共享密钥（Bearer；双向相同）。**联邦互信 ≈ SSH
  免密**——对端拿到链路即可按白名单执行工具
- **保活**：30s 心跳 / 90s 判死；断线指数退避重连（1s→30s 封顶）；
  出站背压（256 帧上限）
- **帧（`FedFrame`）**：externally-tagged + `#[serde(default)]` 演进；
  `call_id = <origin_node>:<ts>-<seq>`（因果溯源，origin 本机帧即
  `LoopDetected` 拒绝）

### 三类 RPC

| 类别 | 帧 | 语义 |
|---|---|---|
| 工具调用 | Invoke → InvokeAccepted（裁决）→ InvokeOutput*（流式）→ InvokeResult（终态）/ Cancel | 大脑侧 `RemoteTool` 代理；执行端 per-peer 白名单裁决后经本机 `ToolRegistry` 执行；断链使在飞调用失败，重连整包恢复 |
| 委派 | SubagentSpawn / SubagentEvent | `spawn_subagent node=...`：大脑侧跑 LLM，工具视图剥前缀替换为该 peer 代理工具（执行时还原 `<peer>:` 前缀）；对端记审计观测 |
| 只读查询 | Query / QueryResult | `NodeStatus`（恒允许）/ `WorkspaceFiles`（工作区并集 canonical 校验）/ `SessionSnapshot`（trunk 快照 + since_seq/limit 分页）；per-peer `allow_queries` 白名单，默认仅 node_status |

## 安全模型

- per-peer 白名单：`allow_tools`（`*` 或显式列表）、`require_confirm`
  （v1 简化为拒绝）、`allow_queries`（敏感查询默认拒绝）
- 执行端裁决复用本机工具实现的安全边界（工作区根内 canonical 校验），
  大脑侧不做也不可能做远程 fs 校验
- 全部联邦调用记 `federation` 审计日志（tracing target）

## 配置与管理面

```toml
[federation]
# 始终开启（无 enabled 开关）；不配 listen / peer 时为空转（无网络暴露）
listen = "0.0.0.0:3133"
node_name = "workstation"

[federation.peers.gpu-box]
url = "ws://192.168.1.20:3133"
token = "<openssl rand -hex 32>"
allow_tools = ["*"]
allow_subagent = true       # 接受对端远程委派（Phase 3）
require_confirm = []        # 命中列表的调用拒绝并提示需确认（v1 简化为拒绝）
allow_queries = []          # 敏感查询需显式开启
```

**Panel 管理**（设置·联邦页）：`SaveFederationPeer` /
`DeleteFederationPeer` / `RequestFederationStatus` /
`RequestFederationInvite`（Frontend-only 命令）——peers 列表与在线
状态、在线增删（运行时生效 + ConfigStore 原子写回）、**邀请串配对**：
`echofed://host:port?name=<别名>#<token>`，A 机生成 → B 机粘贴自动
填充；占位 peer（`invite-*`）配对成功自动清理、不落配置（一次性邀请）。

## 代理工具与远程委派

- **代理注册**：链路 Up 时按对端 caps ∩ 首批支持集（bash/read_file/
  write_file/edit_file/search_code/list_files）给全部 persona 注册
  `<peer>:<tool>`（per-peer package 标签 `echo-agent.federation.<peer>`，
  Down 整包禁用、Up 幂等恢复）；shell 三件套与进程级 ShellManager
  耦合深，远程化留待后续
- **远程 subagent 失败语义（fail-closed）**：node 指定的 peer 离线/
  名称错误时，子任务**立即以 Failed 终态回报**（不会静默回本机执行
  造成错机操作）；聚合组同步闭合。peer 名来源：工具列表的
  `<peer>:<tool>` 前缀或工作区会话的 [remote:<peer>] 标注
- **远程 subagent**：`spawn_subagent({task, node})`——LLM 仍在大脑侧，
  tools 过滤为该 peer 代理工具并剥前缀呈现（子任务不感知"远程"），
  执行时还原前缀经 Invoke 路由；结果回灌完全复用本地
  `<subagent_event>` hook（8K 截断、单层委派）

## 只读查询授权姿势

`NodeStatus` 恒允许（无害遥测）；`WorkspaceFiles`/`SessionSnapshot`
默认拒绝，需 `[federation.peers.*] allow_queries = ["session_snapshot",
"workspace_files"]`（或 `*`）显式开启——对方能看到会话内容，安全
优先于便利。

## 跨机文件协作（P3-1）

文件工具（read_file/write_file/edit_file/list_files/search_code）的
`path` 参数支持 `node://<peer>/<绝对路径>` 前缀——agent 可直接操作
对端工作区文件，调用经联邦 Invoke 在对端沙箱内执行（沙箱裁决见
「安全模型」），结果透明返回。

- 工作区会话的远程目录（`{path, node}` 表条目）在提示词中标注
  `[remote:<peer>]` 并附 `node://` 操作说明——agent 据此知晓可操作
- 装配出口：`set_remote_invoker`（组合根注入；联邦关闭时调用报
  「联邦未接线」）
- 失败路径：peer 离线/不存在、链路不可用、对端拒绝（白名单/沙箱）、
  超时均原样透传为 ToolError。超时链：路由 invoke 120s → 外层 wait
  180s → 工具守卫 460s（timeout_hint）

## 会话迁移（P3-2，v1.1 分块搬运）

`MigrateSession { session_id, team_id, target_peer }`（Frontend-only）→
两条 `SessionMigrated` 事件（受理 + 终态）。语义：

1. 源侧定位该 `(team_id, session_id)` 的事件日志，序列化为 JSON 文本；
2. 按 UTF-8 边界切成 ≤192KB 的块（上限 4096 块），经
   `FedFrame::SessionImport` 送达目标；
3. 目标按 `transfer_id` 聚合分块，**每块回 `SessionImportAck`**（`acked_upto`
   = 连续收到的块数），重复块幂等覆盖；凑齐后在 `spawn_blocking` 里
   **导入同名 persona 的事实来源日志**（`TrunkStore::import_session_events`，
   物化历史句柄），回 `FedFrame::SessionImportResult`；
4. 源侧以 **32 块窗口**发送（联邦出站队列仅 256 帧且 `try_send` 满载即错，
   无流控大日志必失败）；窗口填满/队列满即等待 ack，超时（15s）从
   `acked_upto` 重发；总时限 900s。收到终态回执后发 `SessionMigrated`。

失败语义（fail-closed，源侧明确报错，不静默）：peer 离线、persona/会话
不存在、无事件、序列化/传输失败、目标无该 persona、事件解析失败、分块
超限、迁移超时。

> `MigrateSession.team_id` 为 `#[serde(default)]` 可选，兼容旧前端（缺省时
> 源侧逐 persona 查找会话）。

## 跨机只读查询的 team 维度

`QueryRequest.team_id`（`#[serde(default)]`）：`SessionSnapshot` 非空时**只
查该 persona**——同名会话（`local:tui::local_user` 在每个 persona 上都存在）
逐 persona 取首个会返回任意人格的历史，是此前的歧义来源；为空时退旧行为。

Panel 侧：设置 → 联邦页提供「迁移当前会话」（列出生效 peer，带当前会话 /
人格），点击即发 `MigrateSession`；受理与终态两条 `SessionMigrated` 以 toast
呈现（成功/失败不同语气）。

## 中继投递意图（不再按命令名猜语义）

中继是**纯透传**，命令投递范围由客户端显式声明：

- `{"core": "<name>", "frame": {...}}` → 定向该上游；
- `{"broadcast": true, "frame": {...}}` → 广播给全部在线上游（仅只读发现
  命令：`RequestState` / `RequestTeamsList` / 探活 `Ping` + 重连探测）；
- 无壳 → 仅单上游语义下直接转发；多上游下拒绝（不猜测命令语义）。

此前中继维护一份"只读命令白名单"，与 Core 的
`echo_protocol::BROADCAST_READONLY_COMMANDS` 手动同步——已删除。现在**语义
分类不再重复**：中继只认投递意图，前端 `sendCommand` 在多上游下总是解析出
具体 core（`resolveTargetCore` 兜底首个已知上游），需要广播的少数发现命令走
`broadcastEnvelope()` 显式声明。

## 跨机子代理结果聚合（P3-3）

并行 `spawn_subagent node=A + node=B` 时，大脑侧
`RemoteSubagentAggregator` 按父 turn 分组（session_id +
parent_branch_id → 挂起 call_id 集合）：受理时登记、终态时销账，
组内全部终态后产出一条聚合摘要（各节点状态 + 结果前 500 字 +
成功 x/y）投递父会话 timeline（system 消息）。

- 登记：`SpawnSubagentTool::spawn` 受理远程委派（node=Some）时
- 销账双路径：大脑侧本地完成（`notify_remote_subagent` 终态）+
  联邦泵收到对端 `SubagentEvent`（幂等——组销账后即移除，重复
  销账返回 None）
- 投递出口：`set_aggregate_deliver`（组合根注入；未装配时仅记日志）

## 分布式调度（P2）

Panel 侧调度器（`web/src/scheduler.ts`）在**新会话创建**时按策略
选节点：

- `least_busy`（默认）：按 `active_turns` 取负载最低（平局让远程
  空闲节点）；本机读 activities、peer 经 `FederationStatus` 回传
  （在线 peer 由 Core 侧 `Query(NodeStatus)` 3s 短超时实时拉取，
  失败退握手快照）
- `least_busy`（默认）：按 `active_turns` 取负载最低（平局让远程
  空闲节点）；本机读 activities、peer 经 `FederationStatus` 回传
  （在线 peer 由 Core 侧 `Query(NodeStatus)` 3s 短超时实时拉取，
  失败退握手快照）
- **⚠️ 与上游聚合的命名空间边界（2026-10 明确）**：调度器的"节点"=
  联邦节点（本机 `local` + peers，Core↔Core 横向对等）；Panel 上游聚合的
  "core" = `[[cores]]` 连接配置（Panel→Core 纵向）。两套名字**不互通**——
  `scheduleNode()` 返回的联邦 peer 名只在恰好等于某上游 core 名时才让调度
  真正生效，否则调度建议被路由层（`resolveTargetCore`）忽略。部署时应保持
  联邦 peer 名与上游 core 名一致；"联邦 peer 即上游候选"的统一模型是演进
  方向（详见 [Panel 概览](./panel.md) §多上游聚合）。
  会广播给全部在线上游，Panel 必须**按 core 合并**（不能整体覆盖），且命令
  路由按「会话归属 > 人格归属 > activeCore」并带一致性守卫——目标 core 不
  拥有人格时重定向到拥有人格的 core。否则会把 A core 的 `default` 发给
  B core，得到「智能体 default 不存在」。
- **运行态复合键 `(core, id)`**（2026-10 Phase 0）：会话 id
  （`local:tui::local_user`）、shell 会话 id（每 core 都从 `sh-1` 起）、人格
  id 在各 core 上可相同，因此 Panel 的 `activities` / `tasks` / `shellTerminals`
  / `teamTimelines` / `workspaceGitBySession` / `workspaceFiles` 及磁盘
  trunk 缓存都必须用复合键（`core 为空时退化为裸 id`，单上游零变化）；同名
  跨 core 不再互相覆盖/串显。引导期裸广播的只读命令白名单（中继
  `is_readonly_command`）须与 Panel 引导命令对齐，否则多上游下被静默拒收。
- **身份内建 `node_id` + 复合键收尾**（2026-10 Phase 2）：`SessionInfo` /
  `TeamInfo` 线上新增可选 `node_id`（进程级 NodeId，`echo_agent::set_node_id`
  由组合根注入；旧 Core 缺省 None，serde 默认兼容）——多节点聚合客户端不再
  只靠中继信封区分同名会话/人格。Panel 学习 `core → NodeId` 映射并在选择器
  展示；`branchTabs` 按 `(core, branch_id)` 键；等待态（pending）登记时记录
  目标 core，响应只销账同 core 的等待。
- **运行区域（agent 自带属性，Plan B，2026-10）**：不存在"管理目标 core"
  这种自由变量。每个 agent 自带**运行区域**——区域 = 承载它的 Core，id 为
  `NodeId`（稳定），展示名取 `[core].region_name` → `[federation].node_name`
  → 主机名 → NodeId 短码；`TeamInfo` / `SessionInfo` 线上携带
  `node_id`（= 区域 id）+ `region_name`，所以 agent/会话是**自描述**的。
  Panel 的 `activeRegion` 是**派生量**（只在选 agent / 会话归属 / TeamsList
  校正时写入），所有命令路由、事件过滤、设置/QQ/联邦等管理面归属都由它决定
  ——**选 agent 即选区域**。无 team/session 的管理事件（`ApiConfigUpdated` /
  `SkillsList` / `ToolsList` / `PluginsList` / `SystemPrompt` / `Adapter*` /
  `Federation*` / `SelfUpdateStatus`）只接受当前区域（`activeRegion`）的响应，
  避免广播/竞态响应串台；区域切换时 `resetManagementState` +
  `requestActiveRegionManagement` 定向重拉。QQ 管理态按 `(region, adapter)`
  存放（实例名跨区域可重复，默认都叫 `qq`）。
- **区域放置（Panel 统一管理）**：设置 → 智能体 详情展示该 agent 的
  「运行区域」，并可**迁移到其他区域**——Panel 编排 目标区域 `SaveTeam`
  （按当前 profile 部署/更新）+ 源区域 `DeleteTeam`，两条命令都带**显式
  targetCore**，不依赖任何全局路由变量。会话历史属于原区域，不随 agent
  迁移（跨区搬历史是 `MigrateSession`，需两区域已建联邦链路）。
- **连接状态卡**：逐区域列出全部已配置上游（`/api/upstreams` 轮询）；不再有
  "管理目标"选择器，也不再标记"当前区域"——当前 agent 所属区域已在 **agent
  菜单**（AgentSwitcher 的区域徽标）中展示，避免重复。
- `round_robin`：在线节点轮转
- `prefer:<name>`：亲和定向（离线退 least_busy）

策略存 localStorage（`echo-schedule-policy`），面板「设置 → 系统 →
节点调度」可切换并查看全节点负载一览。已有归属的会话经 P1 的
`coreForCommand` 按归属自动路由，调度不参与。
