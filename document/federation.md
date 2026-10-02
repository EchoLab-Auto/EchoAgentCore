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

每节点同时监听（`[federation] listen`，默认 :3133）与按 peers 连出；
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
token = "shared-secret"
allow_tools = ["*"]
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
- **远程 subagent**：`spawn_subagent({task, node})`——LLM 仍在大脑侧，
  tools 过滤为该 peer 代理工具并剥前缀呈现（子任务不感知"远程"），
  执行时还原前缀经 Invoke 路由；结果回灌完全复用本地
  `<subagent_event>` hook（8K 截断、单层委派）

## 只读查询授权姿势

`NodeStatus` 恒允许（无害遥测）；`WorkspaceFiles`/`SessionSnapshot`
默认拒绝，需 `[federation.peers.*] allow_queries = ["session_snapshot",
"workspace_files"]`（或 `*`）显式开启——对方能看到会话内容，安全
优先于便利。
