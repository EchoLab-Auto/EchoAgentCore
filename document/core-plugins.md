---
id: plugins
title: "插件系统"
group: 后端模块
link: ["adr-0013 | 插件化决策", "core | Core 框架"]
x: 48
y: 1896
---

# 插件系统

插件化是 Core 的组合方式：每个内置模块以 `PluginManifest` 注册进 `PluginHost`（`source/backend/echo-agent/src/plugins.rs`），有权启用/禁用/热加载，且纳入 persona 能力白名单语义。

## 插件模型

- 核心类型：`PluginManifest`（id/name/version/kind/entry/description）+ `BuiltinPlugin` + `MountContext`
- 注册为**可逆**副作用：`register_and_mount` 返回 disposer，禁用即卸载注册
- 数据插件（skill/tool）支持热重载（插件目录 5s 轮询）；代码插件需二进制重载

## 内置插件清单

| id | kind | 说明 |
| --- | --- | --- |
| `echo-agent.tools.builtin` | Tool | 内置工具集（计算/搜索/清单/编码/适配器管理） |
| `echo-agent.adapter.qq` | Adapter | QQ 适配器（OneBot v11 反向 WS，含 QQ 管理工具） |
| `echo-agent.skills.dir` | Skill | SKILL.md 技能目录（热重载） |
| `echo-agent.orchestration` | Orchestration | 后台任务/并行分支/子代理/定时器/自更新 |
| `echo-agent.provider.llm` | Provider | LLM 提供方工厂 |
| `echo-agent.loop.runner` | Loop | turn/step 状态机与工具管道 |
| `echo-agent.management.panel` | Management | Panel 桥接/sudo 授权通道 |
| `echo-agent.branch.reply` | Orchestration | 回执分支（临时分支可见性开关） |
| `echo-agent.session.global` | Management | 全局会话视图开关 |
| `echo-agent.chatbot.sessions` | Management | 会话系统总开关（会话/分支卡） |

## 能力开关（per-persona）

- 每个插件 id 均可放入 agent 的 `enabled_plugins`（白名单）或 `disabled_plugins`（黑名单）
- 白名单非空 = 只启用列出的插件；黑名单优先
- 前端资源页可按 kind 勾选（adapter/management 类），保存后写回 TOML

## 动态编排工具

- 编排类工具（`schedule_timer`、`run_subagent`、`spawn_background_task`、`spawn_parallel_task`、`framework_update`、`run_sudo` 等）由 loop 内联调度，按 persona 白名单过滤（`allows_dynamic_tool`）
- 工具 schema 与处理函数同源（`ORCHESTRATION_TOOL_NAMES`），避免漂移

## 用户扩展方式

- **技能**：在 skills_dir 放置 `SKILL.md`（带 frontmatter：name/description/keywords/always/category/package），运行时自动发现、热重载
- **数据插件**：在 plugins_dir 放置 `plugin.toml`（skill/tool kind），5s 热加载
- **代码插件**（provider/loop/adapter）：需修改源码并走自更新流程
