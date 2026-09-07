---
id: agent-loop
title: "Agent 循环"
group: 后端模块
x: 970
y: 1241
---

# Agent 循环

## Turn 循环

- 每条消息注册一个**临时分支**（可取消），拿历史快照后进入 `process_message_inner`
- 系统提示词按块构建：基础提示词、**system 技能层（system:true，见 [技能系统](./core-skills.md)）**、技能清单、常驻/触发技能、后台编排说明（**仅该 agent 启用了编排工具时注入**）、输入边界规则
- 循环迭代（`max_tool_iterations`，默认 1024）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 达上限未完成时报错收尾；工具的超时/失败不中断 loop（见 [工具系统](./core-tools.md)）

## echo-loop：可替换的默认驱动（TurnRunner）

`echo-loop` crate 把驱动抽象为 **turn/step 状态机**（对齐 dsh 的 turn/step 模型）：turn 消化一条输入直到不再欠债；step = 一次模型请求 + 它引发的工具执行。组合根经 `ctx.loop` 注册 `TurnRunner`（`LoopOptions`：`max_tool_iterations`/`tool_timeout` 来自配置），挂载不同实现即可改变驱动行为；消费方（agent、UI、hook）只依赖生命周期事件。

- **生命周期事件**（全部经 `EventBus` 分发）：`TurnStart` / `AgentPreStep`（waterfall，可改写消息或拒绝 step）/ `StepStart` / `AgentRequest`（waterfall，可改写请求）/ `ToolCallRequested` / `ToolResult` / `StepEnd` / `TurnStopping`（serial）/ `TurnEnd`
- **工具执行管道**（`ToolPipeline`）：`pre-execute → execute → post-execute` 的 waterfall around-middleware；每个阶段收 `&ToolCall` + `next()` 句柄，不调 `next()` 即短路。超时、审批、审计、限流都是注册进管道的中间件，不是循环代码——新策略 = 一次 `push_pre`/`push_post`
- 工具经 harness 提供的 `ToolExecutor` 闭包执行，runner 本身不含任何策略代码

现有 `Agent::process_message_inner` 仍是生产循环（新驱动并行存在、逐步取代）。三处形似循环的评估结论：`run_subagent` 与 `generate_wait_reply` 是无工具单请求，保持自身实现；`run_background_branch` 收敛到 TurnRunner 需工具白名单与独立事件桥，列为后续工作。

## 取消与中断

- 临时分支持有 `CancellationToken`：用户取消（`CancelRequestedWork`）时正在执行的工具被外层 select 中止，turn 以取消收尾
- **按 agent 路由**：`CancelRequestedWork` 携带 `team_id`（`#[serde(default)]` 向后兼容），
  命令泵按 team 路由到对应 persona——self-coding 的消息只有在 self-coding 的 Agent 中才能取消
- **取消即收尾**：TUI 路径取消同样 emit `AgentCompleted`（与 QQ 路径一致），前端据此把
  activity phase 置 completed；前端同时做乐观中断（见 [Panel 前端](./panel.md)）
- 优雅排空（见 [部署与自更新](./ops-deploy.md)）：收到 SIGTERM 后排空模式拒绝新消息、等待活跃 turn 完成（最多 120s）
- 重启后 turn 不恢复；事件日志的悬空调用在加载时由 `repair_tool_pairing` 补合成结果，显示时间线的悬空 running 条目标注为"已中断"
