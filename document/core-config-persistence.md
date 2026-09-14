---
id: config-persistence
title: "配置持久化"
group: 后端模块
x: 955
y: 2047
---
# 配置持久化

## 概述

EchoAgentCore 的配置通过一个共享的 `ConfigStore` 进行原子化读写。

| 类型 | 机制 | 写入 section |
|---|---|---|
| Agent 配置（API key、模型、profiles） | `ConfigStore::patch()` 替换 `[agent]` | `[agent]` |
| 全局系统提示词 | `persist_system_prompt_plugin()` → `ConfigStore::patch()` | `[plugins.system_prompt].text` |
| QQ 门控配置（gate mode、白名单、黑名单） | `ConfigStore::patch()` 替换 `[adapters.qq]` | `[adapters.qq.gate]` + `[adapters.qq.filter]` |

`ConfigStore` 内部使用 `std::sync::Mutex` 序列化所有写入，读写一次全文件，
两个调用者（Agent 和 QqAdapter）共享同一实例，**杜绝并发写互相覆盖**。

写入流程：`read TOML → apply patch → write to .tmp → rename`（原子替换）。

## Agent 配置持久化

### 触发时机

| 操作 | 触发 |
|---|---|
| `/api switch <name>` | 切换 API profile → `persist_config()` |
| `/api key <key>` | 修改 API key → `persist_config()` |
| `/api model <name>` | 修改模型 → `persist_config()` |
| `/api provider <name>` | 修改 provider → `persist_config()` |
| `/api prompt <text>` / Panel 系统提示词保存 | 修改 system prompt → `persist_system_prompt_plugin()`（写 `[plugins.system_prompt].text`，不动 `[agent]`） |
| `/api new <name>` | 新建 profile → `persist_config()` |

### 数据流

```text
Panel 操作 (/api ...)
  → BackendCommand::UpdateApiConfig / SwitchApi / ...（经 management WS）
  → Core 的 Agent::apply_command()
    → 更新内存中的 AgentConfig
    → Agent::persist_config(&cfg)
      → tom_l::Value::try_from(cfg)
      → ConfigStore::patch(|root| root.insert("agent", value))
        → 读取全文件 → 修改 [agent] → 写入 .tmp → rename
```

Agent 始终使用单一全局 Trunk（每个 persona 独立一份）。各平台会话只保留独立的来源身份、投递目标和授权
范围，共享一份受 token 预算限制的历史记录及轮次锁。

### 写入的文件位置

```text
Core 启动时 --config 指定的文件（如 config/echo-agent-core.local.toml）

ConfigStore 由 main.rs 创建一次，分发给各 persona 的 Agent 与 QqAdapter（都指向同一 core.toml 的 `[agent]` / `[adapters.qq]` section）：
  let config_store = echo_adapter::ConfigStore::new(args.config_path());
  qq_adapter.set_config_store(config_store.clone());
  // make_agent 内：每个 persona 共享同一个 config_store（agents_config_store.clone()），
  // 会话文件路径独立设置（JSON，见下）。

⚠️ **会话持久化与配置持久化解耦**（2026-09 修复）：`Agent::set_config_store` 只接管 TOML 配置，
不再连带把 trunk 会话路径设成同一文件；会话路径由 `Agent::set_session_persist_path` 单独设置
（`echo-sessions-{id}.json`）。历史上二者曾共用 ConfigStore 路径——patch 读 JSON 文件时
TOML 解析失败，所有 `/api` 与 Panel API 保存静默丢失（日志 `failed to persist agent config:
config parse failed ... line 1, column 1 ... invalid key`）。
```

Panel 自身不读写 Core 配置文件——所有配置操作都通过 Core 的 management
WebSocket API 下发，持久化统一在 Core 进程内完成。

### 加载期迁移（内存迁移，保存自愈）

`CoreConfig::load()` 在反序列化后做一次性修正，**不回写文件**；下次
`persist_config()`/`ConfigStore::patch` 时自然落盘。现有迁移：

- legacy `[server]`/`[bot]` → `[adapters.qq]`（有显式值才触发，打印提示）
- `api_profiles` 按名去重（历史持久化 bug 自愈）
- **循环模式插件 id**（`migrate_orchestration_mode_plugins`）：teams 各成员白名单与全局
  `[agent].disabled_plugins` 中的旧 id 归一化为循环模式插件 id——旧编排模式 id
  （`echo-agent.orchestration.chatbot` / `branch.reply` / `session.global` /
  `chatbot.sessions`）→ `echo-agent.loop.parallel`；`orchestration.single` →
  `echo-agent.loop.single`；`echo-agent.loop.runner` 剔除（模式插件取代）——旧 id
  不再注册，不迁移则 `apply_disabled` 静默失效；白名单同时含 single+parallel 记
  warn（parallel 优先）。运行期 `SaveTeam`
  （`AgentManager::save_profile`）入口做同样归一化，防御旧 Panel 回写旧 id
- **per-persona 插件黑名单移除**（2026-09-11，同函数内）：`[agent.teams.*].disabled_plugins`
  的语义物化进 `enabled_plugins` 白名单（`convert_plugin_blacklist_to_whitelist`）——
  空白名单 + 黑名单 → 「全部内置插件 − 黑名单 − parallel 模式 id」；非空白名单 → 剔除
  黑名单项（黑名单含 parallel id 时同时剔除白名单的 parallel id，保持单会话推导）。
  迁移后字段清空、序列化不再写回（`#[serde(default, skip_serializing)]`），下次保存自愈

### 不持久化的内容

以下配置只在启动时从文件读取，运行时修改不持久化：
- `[adapters.qq.server]`（bind_address、access_token、heartbeat_interval）
- `[adapters.qq.trigger]`（dm_auto_reply、group_at_reply）
- `[logging]`

## QQ 门控配置持久化

### 触发时机

| 操作 | 触发 |
|---|---|
| `/qq setting` 切换门控模式 | `set_gate_mode()` → `persist_filter()` |
| `/qq setting` → 白名单 → 切换群 | `update_allowlist()` → `persist_filter()` |
| `/qq setting` → 黑名单 → 切换群 | `update_denylist()` → `persist_filter()` |
| 手动输入 QQ 号添加/移除用户 | `update_allowlist()` / `update_denylist()` → `persist_filter()` |

### 数据流

```text
Panel 操作 (/qq setting)（经 management WS）
  → BackendCommand::SetQqGateMode / UpdateQqAllowlist / UpdateQqDenylist
  → Agent::apply_command()
    → Adapter::set_gate_mode() / update_allowlist() / update_denylist()
    → 更新内存中的 runtime 字段
    → rebuild_filter_pipeline()（立即生效）
    → persist_filter()
      → get_filter_config() + get_gate_mode()
      → ConfigStore::patch(|root| {
           在 root 中更新 adapters.qq.gate.mode
           在 root 中更新 adapters.qq.filter.allowlist/denylist
           Ok(())
        })
        → 读取全文件 → 修改 [adapters.qq] → 写入 .tmp → rename
```

> 注：`persist_filter` 和 `Agent::persist_config` 共享同一个 `ConfigStore` 实例。
> 两者在不同时机调用，但通过 `ConfigStore` 内部的 `Mutex` 保证不会并发覆写。

### 写入的 section

```toml
[adapters.qq.gate]
mode = "allowlist"  # "none" | "allowlist" | "denylist"

[adapters.qq.filter.allowlist]
user_ids = [123456, 789012]
group_ids = [111222]

[adapters.qq.filter.denylist]
user_ids = []
group_ids = []
```

### 线程安全

- `QqInner` 中所有运行时字段使用 `StdMutex` 保护
- `persist_filter()` 锁定 `config_store`（共享的 `ConfigStore` 内部 Mutex），不阻塞消息处理
- 过滤管道使用 `Arc<FilterPipeline>`，读取时克隆 Arc（O(1)），不持锁
- `ConfigStore` 的 Mutex 是 `std::sync::Mutex`（非 tokio），一次 patch 持锁时间仅限文件 I/O

### 不持久化的内容

- `rebuild_filter_pipeline()` 不触发持久化（只重建管道，不修改配置）
- `get_filter_config()` 只读不写

## 原子写入

两个持久化路径都使用相同的原子写入模式：

```text
1. 读取当前文件内容
2. 在内存中修改目标 section
3. 写入 {path}.tmp（临时文件）
4. std::fs::rename({path}.tmp, {path})（原子替换）
```

`rename` 在同一文件系统上是原子操作（POSIX 保证），因此：
- 写入中途崩溃 → tmp 文件残留，原文件完整无损
- 不会出现"文件写一半"的损坏状态

## Session 持久化

会话（消息历史）的持久化由 `TrunkStore` 管理，与上述配置持久化是独立的子系统。

| 属性 | 说明 |
|---|---|
| 文件 | `~/.config/echo-agent-core/echo-sessions-{id}.json`（每个 persona 独立文件，严禁合并；旧默认专用的 `echo-sessions.json` 仅当存在 `default` 人格时于首次启动自动改名迁移，配置里没有 `default` 时旧文件保持原样不动） |
| 格式 | **v6**（2026-09 多会话）：`events`（append-only 事件日志）为权威，每事件带 `session` 归属；`trunk_histories`（按会话投影映射）/`identities`/`timeline` 为持久化投影。v5 及更旧格式加载时自动迁移（无归属事件按 hook 内容归因：QQ 私聊/群/backend 推导 + 粘滞继承 + 兜底本地会话），下次保存写回 v6；v1-v4 旧格式先经事件化迁移再归因 |
| 触发 | 30s 周期保存（dirty 时）+ 关闭时 flush |
| 加载 | 启动时自动恢复；显示时间线的 `timeline_seq` 从条目最大 seq 重建，悬空 running 工具条目标注为"已中断" |

工作区会话（`echo-agent.workspace` 插件）独立持久化于
`~/.config/echo-agent-core/echo-workspaces-{id}.json`（每 persona 一份；
`{active, sessions[]}` 文档，任何变更即时原子写回）——与配置 TOML、会话
JSON 均不共用路径。`active` 同时是「项目通道」的单一事实来源（激活 = 本地
对话切换 + 提示词注入，见 [多 Agent 与会话](./core-agents.md)§工作区会话与项目通道）；
**所有激活来源（面板/工具 `use`）都必须广播 `WorkspaceSessions`**。
