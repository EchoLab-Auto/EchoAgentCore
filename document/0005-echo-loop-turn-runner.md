---
id: adr-0005
title: "ADR-0005 echo-loop 轮次执行器"
group: 架构决策
x: 2076
y: 552
---
# ADR-0005: echo-loop TurnRunner(默认 agent 驱动,可替换)

状态: accepted

## 问题

Agent 核心循环 `process_message_inner`(约 270 行)是一个不可替换的巨型
函数:一次模型请求 + 工具执行在一个 for 循环里内联完成,生命周期事件
(`LlmRequest`/`ToolCall`/`ToolResult`)在循环内部硬编码 emit,超时也是循环级
`tokio::select`。同样的"请求-工具"循环还在 `run_subagent`、
`run_background_branch`、`generate_wait_reply` 三处重复实现,规则各异——
驱动不可替换、扩展点缺失、逻辑散落。

## 决策

新建 `echo-loop` crate,实现 **TurnRunner(默认驱动)** 与 **工具执行管道**,
对齐 dsh 的 turn/step 模型与扩展点:

- **生命周期事件**(`event.rs`,全部可经 `EventBus` 分发):
  `TurnStart`/`AgentPreStep`(waterfall,可改写消息或拒绝 step)/
  `StepStart`/`AgentRequest`(waterfall,可改写请求)/`ToolCallRequested`/
  `ToolResult`/`StepEnd`/`TurnStopping`(serial,无 next)/`TurnEnd`。
- **工具执行管道**(`pipeline.rs`):`ToolPipeline` 承载 pre/post 两个阶段列
  ——`tools/pre-execute → tools/execute → tools/post-execute` 的 waterfall
  around-middleware。每个阶段收到 `&ToolCall` + `next()` 句柄;返回
  `ShortCircuit(text)` 不调 `next()` 即短路。超时、审批、审计、限流都是
  注册进管道的中间件,不再是循环代码。
- **TurnRunner**(`runner.rs`):驱动一个 turn 经过多个 step——
  `turn/start → agent/pre-step → step/start → agent/request → llm.chat →
  tool/call → pipeline → step/end → (还需请求则下一步) → turn-stopping →
  turn/end`。工具经 `ToolExecutor` 闭包执行(由 harness 提供具体实现),
  驱动本身不含任何策略代码。

接入:组合根注册 `ctx.loop`(`TurnRunner` 经服务定位解析);`LoopOptions`
(`max_tool_iterations`/`tool_timeout`)从配置而来。

## 备选方案

- **不抽取,维持单函数循环**:驱动不可替换,扩展点缺失。
- **只抽管道不抽驱动**:工具中间件可插拔,但 turn/step 生命周期仍硬编码。
- **驱动内嵌策略**:把超时/审批写进 runner——回到"策略是循环代码"。

## 后果

- 循环成为可替换插件:挂载不同 `TurnRunner`(如无工具模式、后台模式)即可
  改变驱动行为,消费方(agent、UI、hook)只依赖生命周期事件。
- 工具策略(超时/审批/审计/限流)收敛为管道中间件;新策略 = 一个
  `push_pre`/`push_post` 注册,不改循环。
- 三处"重复"循环评估:`run_subagent` 与 `generate_wait_reply` 是无工具
  单请求(本就没有可收敛的请求-工具迭代循环),保持自身实现以避免
  reasoning 语义损失;`run_background_branch` 是完整工具循环,收敛到
  TurnRunner 需要工具白名单与独立事件桥,列为后续工作。
- `create_provider` 的 provider 拆分、平台 seam 在 Phase 4 后续轮次完成
  (见 ADR-0006/0007)。
- 行为零变化:462 项测试全绿;`Agent` 现有循环未被替换(新驱动并行存在,
  组合根已注册),生命周期事件为纯增量。
