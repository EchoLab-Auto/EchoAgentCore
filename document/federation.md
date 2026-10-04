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
  workspaces}` + `protocol_version`）——版本不符拒绝、自连回环拒绝、
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
enabled = true
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

## 会话迁移（P3-2，v1.0）

`MigrateSession { session_id, target_peer }`（Frontend-only）→
`SessionMigrated` 事件。v1.0 语义：目标 peer 在线可达性确认 + 指引
（历史经 `SessionSnapshot` 跨机可查，新对话在目标侧继续）；完整
timeline 搬运需要分块传输协议（单帧体积/重传语义），列入 P3-2b。

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
- **人格身份 `(core, team_id)`**（2026-10 修复）：`RequestTeamsList` 无壳时
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
- **管理目标 core**（2026-10 Phase 1）：`store.dispatch` 给**每个**事件载荷
  注入来源 core；无 team/session 的管理事件（`ApiConfigUpdated` / `SkillsList`
  / `ToolsList` / `PluginsList` / `SystemPrompt` / `Adapter*` / `Federation*`
  / `SelfUpdateStatus`）只接受 `activeCore` 的响应（`managementCoreCurrent`），
  避免广播/竞态响应里其他 core 的配置覆盖当前视图。Panel 侧栏「连接状态」
  卡提供**管理目标选择器**（`state.knownCores`）；切换时 `resetManagementState`
  + `requestActiveCoreManagement` 定向重拉。QQ 管理态按 `(core, adapter)`
  存放（实例名跨 core 可重复，默认都叫 `qq`）。
- `round_robin`：在线节点轮转
- `prefer:<name>`：亲和定向（离线退 least_busy）

策略存 localStorage（`echo-schedule-policy`），面板「设置 → 系统 →
节点调度」可切换并查看全节点负载一览。已有归属的会话经 P1 的
`coreForCommand` 按归属自动路由，调度不参与。
