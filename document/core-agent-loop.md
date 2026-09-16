---
id: agent-loop
title: "Agent 循环"
group: 后端模块
x: 1283
y: 1526
---

# Agent 循环

Agent 循环是 `echo-agent.loop.{single,parallel}` 插件（kind=Loop，互斥二选一）
承载的**可插拔驱动**：turn/step 状态机 + 工具管道，经 [插件化设计](./core-plugins.md)
节点统一治理（启用/禁用/挂载可逆）。本文档描述其机制细节与当前实化状态。

## 循环模式（单会话 / 并行多会话）

循环模式是**会话内的准入策略**，二选一，由互斥循环插件在 persona 白名单里
表达（`echo-agent.loop.single` 默认 / `echo-agent.loop.parallel`）：

| 模式 | 插件 id | 会话内准入 | 分支可见性 | 会话管理 UI |
| --- | --- | --- | --- | --- |
| 单会话（默认） | `echo-agent.loop.single` | turn 串行排队（FIFO） | 不发射 `ReplyBranch*` | 隐藏 |
| 并行多会话 | `echo-agent.loop.parallel` | 各 turn 并发分支 | 发射 `ReplyBranch*` | 显示 |

- **串行语义**：单会话下 `Session.turn_queue`（tokio Mutex，公平 FIFO）是准入
  闸门；turn 在拿到闸门后才记录输入并取上下文快照，因此排队的第二个 turn 看到
  的是第一轮结束后的上下文。不同会话互不排队——**另启一个会话就是另一条并行
  通道**。并行模式下不使用闸门（到达即快照，与旧行为一致）。
- **排队期间可取消**：turn 在等待闸门**之前**就注册进 `active_inbound_turns`，
  取消命令能中断排队（该 turn 不执行、不留残回复）；QQ 入站注册在分支任务内
  完成，适配器入站永不因排队阻塞。
- **推导**：`TeamMember::loop_mode()`（单一来源）——白名单含 `loop.parallel`
  → parallel；其余（含白名单为空 = 默认）→ single。
  插件黑名单已移除（2026-09-11），推导只看白名单。旧 id 迁移见
  [配置持久化](./core-config-persistence.md)。
- **落地**：两个插件 mount 的是同一个 `TurnRunner`（驱动本体），模式只改策略；
  两个都卸载才回退内置循环。模式只能经面板「循环模式」分段单选修改（写白名单）。

## Turn 循环

- 每条入站消息经[多 Agent 与会话](./core-agents.md)的临时分支机制进入
  `process_message_inner`；本层只负责循环本身
- 系统提示词按块构建（[插件化设计](./core-plugins.md) 的能力门控作用于各层）：
  基础提示词、**system 技能层**（system:true，见 [技能系统](./core-skills.md)）、
  技能清单、常驻/触发技能、**后台编排说明（仅该 agent 启用了编排工具时注入**——
  白名单无编排工具的 agent 不注入描述不可用工具的规则）、输入边界规则
- 循环迭代（`max_tool_iterations`，默认 1024）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 输出预算：`[agent].max_tokens`（0 = 无上限，后端回退 `DEFAULT_MAX_TOKENS = 128K` 实用上限）；响应被 `max_tokens` 掐断（finish_reason = length/max_tokens）时自动续跑（最多 4 次，提示"继续上次输出"喂回模型；半截工具调用丢弃后重发完整调用）
- 达上限未完成时报错收尾；工具的超时/失败不中断 loop（见 [工具系统](./core-tools.md)）

## echo-loop：可替换的默认驱动（TurnRunner）

`echo-loop` crate 把驱动抽象为 **turn/step 状态机**（对齐 dsh 的 turn/step 模型）：turn 消化一条输入直到不再欠债；step = 一次模型请求 + 它引发的工具执行。组合根经 `ctx.loop` 注册 `TurnRunner`（`LoopOptions`：`max_tool_iterations`/`tool_timeout` 来自配置），挂载不同实现即可改变驱动行为；消费方（agent、UI、hook）只依赖生命周期事件。

- **生命周期事件**（全部经 `EventBus` 分发）：`TurnStart` / `AgentPreStep`（waterfall，可改写消息或拒绝 step）/ `StepStart` / `AgentRequest`（waterfall，可改写请求）/ `ToolCallRequested` / `ToolResult` / `StepEnd` / `TurnStopping`（serial）/ `TurnEnd`
- **工具执行管道**（`ToolPipeline`）：`pre-execute → execute → post-execute` 的 waterfall around-middleware；每个阶段收 `&ToolCall` + `next()` 句柄，不调 `next()` 即短路。超时、审批、审计、限流都是注册进管道的中间件，不是循环代码——新策略 = 一次 `push_pre`/`push_post`
- 工具经 harness 提供的 `ToolExecutor` 闭包执行，runner 本身不含任何策略代码

### 实化与分派（插件的运行期效果）

`echo-agent.loop.single` / `echo-agent.loop.parallel` 是**实化插件**（见
[插件化设计](./core-plugins.md)「能力开关」）：mount 把组合根共享的 `TurnRunner`
注入各 agent（`set_loop_runner`）并置位 `use_echo_loop` 开关——两者 mount 同一
驱动，差异只在循环模式（策略）；全部卸载才复位（回退内置循环）。分派规则
（`process_message_inner` 开头）：

- **普通输入**（非 QQ hook / 定时器 / QQ 会话）→ `process_via_echo_loop`：
  经 TurnRunner 的 turn/step 状态机 + `ToolPipeline` 执行；
  `RunExtras` 携带工具 schema（模型可见）+ 推理回调（`on_reasoning` → 发
  `AgentReasoning` 事件）；工具执行经 `block_in_place` 同步桥接 `run_tool`
  （ToolCall/ToolResult 事件与事件日志与内置循环同路径）
- **QQ hook / 定时器 / QQ 会话** → 内置循环：边界语义（QQ 边界块、定时器
  回投）由内置循环注入；投递纪律由提示词与 qq-transport 技能引导（2026-09
  移除 send 工具声明校验与纠偏提醒），echo-loop 不接管。QQ 并发回复的细节

两套驱动并行、同一工具路径（`run_tool`）与提示词构建（`build_prompt_blocks`）
复用；卸载全部循环插件（配置 `disabled_plugins` 或面板 TogglePlugin）即恢复
内置循环。`generate_wait_reply` 是无工具单请求，保持自身实现。

## 取消与中断

- 用户取消（`CancelRequestedWork`）时正在执行的工具被外层 select 中止，
  turn 以取消收尾
- **按 agent 路由**：`CancelRequestedWork` 携带 `team_id`（`#[serde(default)]` 向后兼容），
  命令泵按 team 路由到对应 persona——self-coding 的消息只有在 self-coding 的 Agent 中才能取消
- **取消即收尾**：TUI 路径取消同样 emit `AgentCompleted`（与 QQ 路径一致），前端据此把
  activity phase 置 completed；前端同时做乐观中断（见 [Panel 前端](./panel.md)）
- 优雅排空（见 [部署与自更新](./ops-deploy.md)）：收到 SIGTERM 后排空模式拒绝新消息、等待活跃 turn 完成（最多 120s）
- 重启后 turn 不恢复；事件日志的悬空调用在加载时由 `repair_tool_pairing` 补合成结果，显示时间线的悬空 running 条目标注为"已中断"
