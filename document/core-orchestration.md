---
id: orchestration
title: "编排插件"
group: 后端模块
x: 970
y: 1500
link: ["plugins | 插件化设计"]
---

# 编排插件

编排插件是**编排能力**的载体：主插件 `echo-agent.orchestration` 提供后台任务/并行分支/
子代理/定时器/自更新；两个**互斥**模式子插件 `echo-agent.orchestration.single` /
`echo-agent.orchestration.chatbot` 决定该 agent 的编排模式（会话管理 UI 与回执分支可见性）。

> 编排模式是单一来源 `TeamMember::orchestration_mode()` 推导的（默认 chatbot）。
> 本节点与[插件化设计](./core-plugins.md)同属后端模块，仅通过插件节点衔接。

## echo-agent.orchestration

| 属性 | 值 |
| --- | --- |
| id | `echo-agent.orchestration` |
| kind | Orchestration |
| 说明 | 后台任务 / 并行分支 / 子代理 / 定时器 / 自更新 |
| 实化状态 | 名义挂载（实化依赖 echo-loop 迁移完成度），禁用 = 下次重启不装配 |

主要动态工具（loop 内联调度，按 persona 白名单过滤 `allows_dynamic_tool`）：

- `schedule_timer` / `list_timers` / `cancel_timer` —— 定时任务
- `run_subagent` —— 有界委派推理子代理
- `spawn_background_task` / `spawn_parallel_task` —— 后台/并行分支
- `list_background_tasks` / `cancel_background_task` —— 后台任务管理
- `framework_update` / `run_sudo` —— 自更新与人机交互 sudo（另有配置门控）

## 编排模式（互斥子插件）

| id | kind | 说明 |
| --- | --- | --- |
| `echo-agent.orchestration.single` | Orchestration | **编排模式（互斥）**：单任务编排——无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见 |
| `echo-agent.orchestration.chatbot` | Orchestration | **编排模式（互斥）**：多任务并行编排——会话列表/全局会话/可见回执分支（取代旧 `branch.reply`/`session.global`/`chatbot.sessions` 三插件） |

### 模式推导（单一来源）

- `enabled_plugins` 为空（=全部启用）→ **chatbot**
- 含 `orchestration.chatbot`（或旧特性 id，加载期自动迁移）→ **chatbot**；两者并含 chatbot 优先（迁移时 warn）
- 非空但无任何模式 id → **single**
- `disabled_plugins` 含 chatbot/旧 id → **single**；含 single id → no-op（禁用兜底无意义，warn）
- chatbot：会话卡/全局分组/`ReplyBranch*` 事件发射全开；single：全部关闭（分支照常执行合并，仅不可见——可见性开关而非执行开关）

## 动态工具与 per-persona

- 动态编排工具经 persona 白名单过滤（`allows_dynamic_tool`）；工具名表
  `ORCHESTRATION_TOOL_NAMES` 由单元测试守护与 schema 一致
- 后台编排提示词仅在该 agent 至少允许一个动态编排工具时注入（避免描述不可用工具）
