---
id: agents
title: "多 Agent 与会话"
group: 后端模块
x: 955
y: 1512
---

# 多 Agent 与会话

Core 支持**多 agent 人格**：`[agent.teams.*]` 每项 = 一个独立 Agent（独立 trunk、会话文件、系统提示词、能力白名单）。主 agent（default）由组合根选定（BTreeMap 首个配置项），受保护不可删除。

## Persona 装配

- `AgentSupervisor` 按配置创建所有人格；`make_agent` 闭包为每人格独立组装工具注册表（内置工具 + QQ 工具 + 包元数据）
- 每人格独立事件总线；非默认人格事件经 `event_bus.observe` 镜像到主 agent 的连接
- 会话文件隔离：`echo-sessions-{id}.json`（default 沿用 `echo-sessions.json`）；会话持久化路径与配置 TOML **解耦**（`set_config_store` 只管 `[agent]` TOML，`set_session_persist_path` 另设 JSON 事件溯源文件，绝不可同路径互写）
- **Persona 级 API**（2026-09）：`[agent.teams.{id}].api_profile = "<供应商池名>"` 只做**引用**，值存于全局 `[agent].api_profiles`；配置了引用的人格启动时用 `apply_named_profile` 解析并**独立构建自己的 provider**（不共享默认 provider）；未配置 = 跟随全局默认（顶层 + active_api）

## 会话模型

- 会话（Session）= 对话身份，由 `SessionKey`（platform:scope:user_id）标识，带昵称/群名/最后活跃/team_id
- 同一 agent 内所有会话共享同一个 trunk 上下文；不同 agent 的 trunk 完全隔离
- `RequestState` 返回**所有人格**的会话（各会话携带自身 team_id），Panel 按当前 agent 过滤展示

## 临时分支

- 每条入站消息 = 一个临时回复分支：注册（记录快照 + 可取消令牌）→ 后台执行 → 按 `message_sequence` 有序合并回事件日志
- 分支可见性由**循环模式**控制：并行多会话模式发射 ReplyBranch* 可见性事件、侧边栏有分支卡；单会话模式不发射、无分支卡，但分支照常执行合并（可见性开关而非执行开关）；单会话模式还会排队同一会话的 turn（串行准入，见 [Agent 循环](./core-agent-loop.md)§循环模式）
- 分支运行期活动实时进主时间线（推理/工具/回答按序穿插），结束仅清理标签

## 前端切换体验

- 当前 agent 持久化于 localStorage（`echo-panel-active-team`），刷新/重连后停留原 agent
- 切换 agent：先渲染该 agent 的时间线缓存（零等待），再以 `since_seq` 增量刷新
- 侧边栏会话/分支卡与「全局」分组仅**并行多会话模式**显示；单会话模式整体隐藏

## 能力隔离

- `enabled_*` 白名单：非空时仅列出的能力可见；`disabled_*` 黑名单随后细化
- 插件/工具/技能三个维度独立配置；主 agent 未配置白名单时默认全部启用
- **保存即热生效**（2026-09）：`SaveTeam` 后目标人格立即重算门控（工具/技能逐名双向：取消勾选禁用、重新勾选恢复；插件包维度按 `GATED_PLUGIN_IDS`）；`ToggleTeam` 重新启用的人格也会补应用门控。全局禁用（`ToggleTool`/`ToggleSkill`/`TogglePlugin` 卸载）优先级高于 persona 名单，且这些全局开关会逐 persona 重算而不是只作用于管理面
- **Package 跨维度**（2026-09）：勾选/取消勾选一个门控插件 = 启停整个包（plugin + 同名包工具 + 同名包技能，QQ 包示例：send_* 工具与 qq-management/qq-transport 一起开关）；详见 [插件化设计](./core-plugins.md)「Package」章节
- **循环模式**（互斥插件 `echo-agent.loop.{single,parallel}`）由白名单推导：空表 = single（默认）；含 `loop.parallel`（或旧编排模式 id）= parallel；黑名单含 parallel/旧 id = single；两者并含 parallel 优先。推导单一来源 `TeamMember::loop_mode()`
- 全局 `[agent].disabled_tools`（Panel ToggleTool 持久化）启动时逐人格应用

## 设计取舍与边界

- **多实例而非单实例多上下文**：每个 Agent 一辆"车"（独立 `Agent::new` + 事件溯源日志），复用现有结构、互不干扰；代价是每 agent 一份上下文内存（数量预期个位数，可接受）。单实例 + 上下文分桶方案因 trunk/事件溯源改动面大、风险高被否决
- **配置驱动**：人格在配置文件中定义，`enabled=false` 跳过实例化；运行时可通过 `ToggleTeam` 启停，但不动态增删（改配置重启生效）
- **兼容性**：无 `[agent.teams]` 的旧配置 = 单 agent（default），行为与多 agent 之前一致；旧协议 `SendMessage` 无 `team_id` 时路由到 default agent
- ~~所有 persona 共享同一 provider/模型~~（2026-09 起支持 persona 级引用，见上「装配」）；运行期 `SaveTeam` 改 `api_profile` 立即经 `apply_persona_api` 重建该人格 provider（全局 `UpdateApiConfig`/`SwitchApi` 只作用于管理面/全局默认，不再自动广播到其他人格）
- QQ/平台消息仍进 default agent（按群绑定人格为后续方向）
- 每人格可独立配置记忆预算：`[agent.teams.{id}].memory_limit_tokens` / `context_window_tokens`（未配置 = 继承全局）

## 典型配置

```toml
[agent.teams.alix]
name = "Alix"
description = "管理主 agent"
# 空 enabled_plugins = 全部启用 → 单会话（默认）循环模式

[agent.teams.self-coding]
name = "self-coding"
# 显式开启并行多会话循环模式（会话卡/全局分组/可见分支）
enabled_plugins = ["echo-agent.orchestration", "echo-agent.tools.builtin", "echo-agent.management.panel"]
# 可选：persona 级 API（引用全局供应商池 [agent].api_profiles 中的 profile 名）
# api_profile = "openai"   # 不配置 = 跟随全局默认配置
# 显式写法（等价）：追加 "echo-agent.loop.single"；并行多会话模式则列
# "echo-agent.loop.parallel"。旧编排模式 id（orchestration.*/branch.reply 等）
# 与旧驱动 id（loop.runner）在加载期自动迁移。
```
