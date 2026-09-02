---
id: agents
title: "多 Agent 与会话"
group: 后端模块
x: 970
y: 946
---

# 多 Agent 与会话

Core 支持**多 agent 人格**：`[agent.teams.*]` 每项 = 一个独立 Agent（独立 trunk、会话文件、系统提示词、能力白名单）。主 agent（default）由组合根选定（BTreeMap 首个配置项），受保护不可删除。

## Persona 装配

- `AgentSupervisor` 按配置创建所有人格；`make_agent` 闭包为每人格独立组装工具注册表（内置工具 + QQ 工具 + 包元数据）
- 每人格独立事件总线；非默认人格事件经 `event_bus.observe` 镜像到主 agent 的连接
- 会话文件隔离：`echo-sessions-{id}.json`（default 沿用 `echo-sessions.json`）

## 会话模型

- 会话（Session）= 对话身份，由 `SessionKey`（platform:scope:user_id）标识，带昵称/群名/最后活跃/team_id
- 同一 agent 内所有会话共享同一个 trunk 上下文；不同 agent 的 trunk 完全隔离
- `RequestState` 返回**所有人格**的会话（各会话携带自身 team_id），Panel 按当前 agent 过滤展示

## 临时分支

- 每条入站消息 = 一个临时回复分支：注册（记录快照 + 可取消令牌）→ 后台执行 → 按 `message_sequence` 有序合并回事件日志
- 分支可见性由 **回执分支能力插件**（`echo-agent.branch.reply`）控制：禁用时后端不发分支事件、侧边栏无分支卡，但分支照常执行
- 分支运行期活动实时进主时间线（推理/工具/回答按序穿插），结束仅清理标签

## 前端切换体验

- 当前 agent 持久化于 localStorage（`echo-panel-active-team`），刷新/重连后停留原 agent
- 切换 agent：先渲染该 agent 的时间线缓存（零等待），再以 `since_seq` 增量刷新
- 侧边栏会话/分支卡按当前 agent 的**会话系统开关**（`echo-agent.chatbot.sessions`）整体显示/隐藏

## 能力隔离

- `enabled_*` 白名单：非空时仅列出的能力可见；`disabled_*` 黑名单随后细化
- 插件/工具/技能三个维度独立配置；主 agent 未配置白名单时默认全部启用
- 全局 `[agent].disabled_tools`（Panel ToggleTool 持久化）启动时逐人格应用

## 设计取舍与边界

- **多实例而非单实例多上下文**：每个 Agent 一辆"车"（独立 `Agent::new` + 事件溯源日志），复用现有结构、互不干扰；代价是每 agent 一份上下文内存（数量预期个位数，可接受）。单实例 + 上下文分桶方案因 trunk/事件溯源改动面大、风险高被否决
- **配置驱动**：人格在配置文件中定义，`enabled=false` 跳过实例化；运行时可通过 `ToggleTeam` 启停，但不动态增删（改配置重启生效）
- **兼容性**：无 `[agent.teams]` 的旧配置 = 单 agent（default），行为与多 agent 之前一致；旧协议 `SendMessage` 无 `team_id` 时路由到 default agent
- 所有 persona 共享同一 provider/模型；不做 per-agent 模型/密钥隔离
- QQ/平台消息仍进 default agent（按群绑定人格为后续方向）
- 每人格可独立配置记忆预算：`[agent.teams.{id}].memory_limit_tokens` / `context_window_tokens`（未配置 = 继承全局）

## 典型配置

```toml
[agent.teams.alix]
name = "Alix"
description = "管理主 agent"

[agent.teams.self-coding]
name = "self-coding"
enabled_plugins = ["echo-agent.orchestration", "echo-agent.tools.builtin", "echo-agent.management.panel"]
```
