---
id: orchestration
title: "编排插件"
group: 后端模块
x: 1283
y: 1759
---

# 编排插件

编排插件是**编排工具入口**的载体：主插件 `echo-agent.orchestration` 注册
`spawn_background_task` / `spawn_parallel_task` 等工具的 schema 与调度，
**执行机制**（分支并发、快照、有序合并、取消）见[后台任务与并行分支](./core-background-tasks.md)。

> 会话内的准入策略（单会话串行 / 并行多会话）是[Agent 循环](./core-agent-loop.md)的
> 循环模式，由互斥循环插件 `echo-agent.loop.{single,parallel}` 表达（默认单会话）。
> 本节点与[插件化设计](./core-plugins.md)同属后端模块，仅通过插件节点衔接。

## echo-agent.orchestration

| 属性 | 值 |
| --- | --- |
| id | `echo-agent.orchestration` |
| kind | Orchestration |
| 说明 | 后台任务 / 并行分支 / 子代理 / 定时器 / 自更新 |
| 实化状态 | 实化挂载（mount/unmount 启停编排事件循环）；禁用 = 停止所有 persona 的编排任务，进行中的任务继续完成 |

主要动态工具（loop 内联调度，按 persona 白名单过滤 `allows_dynamic_tool`）：

- `schedule_timer` / `list_timers` / `cancel_timer` —— 定时任务
- `run_subagent` —— 有界委派推理子代理
- `spawn_background_task` / `spawn_parallel_task` —— 后台/并行分支
- `list_background_tasks` / `cancel_background_task` —— 后台任务管理
- `framework_update` / `run_sudo` —— 自更新与人机交互 sudo（另有配置门控）
- `present_menu` —— 向 Panel 用户发起选单（menu 插件门控；应答走专用通道，见 [工具系统](./core-tools.md)）

## 循环模式（见 Agent 循环）

单会话 / 并行多会话的选择、串行排队语义与推导规则见
[Agent 循环](./core-agent-loop.md)§循环模式。

## 动态工具与 per-persona

- 动态编排工具经 persona 白名单过滤（`allows_dynamic_tool`）；工具名表
  `ORCHESTRATION_TOOL_NAMES` 由单元测试守护与 schema 一致
- 后台编排提示词仅在该 agent 至少允许一个动态编排工具时注入（避免描述不可用工具）
