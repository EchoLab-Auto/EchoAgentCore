# 0014 Multi-Agent System — 多 agent 对话架构

日期: 2026-08-25
状态: 已实现（Phase 1：多 agent 平行实例）

## 背景与目标

当前 Core 只有一个 Agent，所有会话共享单一 trunk 记忆与一个系统提示词
（`[plugins.system_prompt]`）。用户需求：**与拥有不同记忆、不同系统提示词
的多个 agent 对话**——每个 agent 应像独立人格：自己的记忆、自己的行为
准则、可独立启停。

## 设计决策

### 1. 多实例而非单实例多上下文

Option A（单 Agent + 上下文分桶）：内存省，但 trunk/事件溯源结构大量改动，
风险高。
Option B（多 Agent 实例，每实例独立 TrunkStore）：复用现有 `Agent::new`
与事件溯源会话日志，每个实例一辆"车"，互不干扰；代价是每个 agent 一份
上下文内存（可接受，agent 数量预期个位数）。

选 **Option B**。每个 Agent 就是一个独立人格：独立 trunk、独立
`echo-sessions-{id}.json`、独立 system_prompt、独立启停开关。

### 2. 配置：`[agent.profiles]`

```toml
[agent.profiles.assistant]
name = "助手"
description = "通用助手"
system_prompt = "你是通用助手…"
enabled = true

[agent.profiles.writer]
name = "写作助理"
description = "专注于写作"
system_prompt = "你是写作专家…"
enabled = true
```

- 配置文件里定义的人格列表；`enabled=false` 的跳过实例化
- 所有 profile 共享同一 provider/模型（版本 1；per-profile 模型留到
  Phase 2，与 API profiles 联动）

### 3. 运行态：AgentManager

组合根根据 profiles 实例化 N 个 `Arc<Agent>`，各自 `set_config_path`
（独立会话文件）、独立 `load_sessions`。`AgentManager` 持有：

```rust
struct AgentManager {
    default_id: String,             // "assistant" 或第一个启用的
    agents: RwLock<HashMap<String, Arc<Agent>>>,
    /// 事件聚合：所有 agent 的 BackendEvent 经统一桥接发给 Panel
}
```

- `SendMessage { agent_id, session_id, content, images }`：按 agent_id 路由，
  缺省走 default agent
- 所有 agent 的事件通过 `BackendBridge` fanout（Panel 侧事件带 agent_id
  标注：SessionInfo/AgentOutput/… 增加字段）

### 4. 协议扩展

```rust
// BackendCommand
SendMessage { agent_id: Option<String>, session_id, content, images }
RequestAgentsList                       // → AgentsList
ToggleAgent { id, enabled }             // 运行时启停（禁用则卸载）

// BackendEvent
AgentsList { agents: Vec<AgentInfo> }
AgentInfo { id, name, description, enabled, sessions: usize }

// SessionInfo / 相关事件加 agent_id
SessionInfo { agent_id: Option<String>, .. }
```

### 5. 前端体验

- 顶部/侧边栏加 agent 选择器（下拉或标签），切换后：
  - 聊天区只显示该 agent 的会话
  - 发送消息自动带当前 agent_id
- 每个 agent 独立会话列表（SessionGroups 显示 agent 分组）
- "资源 → 插件"视图可查看 agent 概览（后续）

### 6. 事件聚合与消息过滤

- management WS 仍是单一连接；所有 agent 的事件桥接到同一个 bridge
- 生命周期事件（AgentThinking/AgentOutput/…）带 agent_id，Panel 按当前
  选择过滤显示——避免"写作助理的回复出现在通用助手窗口"

## 边界与不做

- 不做 per-agent LLM 模型/密钥隔离（Phase 2）
- 不做 agent 间通信/委派（Phase 2+，与 Orchestration 结合）
- 不做动态创建/删除（配置驱动，改配置重启生效；运行时可启停但不增删）
- QQ/平台消息仍进 default agent（Phase 2 支持按群绑定人格）

## 兼容性

- 旧配置无 `[agent.profiles]` → 单 agent（id = "default"），行为与现在一致
- 旧协议 SendMessage 无 agent_id → default agent
- Panel 旧版本忽略 AgentsList 事件、未知字段
