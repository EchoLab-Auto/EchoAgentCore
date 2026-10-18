---
id: agent-loop
title: "Agent 循环"
group: 框架
x: 1280
y: 820
---

# Agent 循环

> **定位**：本文描述 **Agent 循环**——可插拔驱动（echo-loop `TurnRunner`）的 turn/step 状态机、循环模式（单会话 / 并行多会话）、工具管道，以及 harness（`agent/turn.rs`）如何把一条输入交给唯一的循环驱动执行。读者：需要理解消息如何被循环执行的开发者。
> 相关：[多 Agent 与会话](./core-agents.md)（分支与会话准入）、[插件化设计](../plugins/core-plugins.md)（loop 插件治理）、[工具系统](../plugins/core-tools.md)（工具执行与超时）。

Agent 循环只有**一个驱动**：echo-loop `TurnRunner`（turn/step 状态机 + 工具
管道）。`echo-agent.loop.{single,parallel}` 插件（kind=Loop，互斥二选一）只表达
**循环模式**（会话内准入策略），不切换驱动。

> 单轨化（架构评审）：此前存在第二套"内置循环"（`process_message_inner`
> 内联实现），QQ hook / 定时器 / QQ 会话输入走它、普通输入走 echo-loop；两者
> 靠"逐项同口径"维持一致并多次漂移。内置循环已删除，边界输入的唯一差异
> （提示词边界块）由 harness 统一注入。

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
  通道**。并行模式下不使用闸门（到达即快照）。
- **排队期间可取消**：turn 在等待闸门**之前**就注册进 `active_inbound_turns`，
  取消命令能中断排队（该 turn 不执行、不留残回复）；QQ 入站注册在分支任务内
  完成，适配器入站永不因排队阻塞。
- **推导**：`TeamMember::loop_mode()`（单一来源）——白名单含 `loop.parallel`
  → parallel；其余（含白名单为空 = 默认）→ single。
  插件黑名单已移除，推导只看白名单。相关机制见
  [配置持久化](./core-config-persistence.md)。
- **落地**：两个插件是纯策略插件（mount 无运行期资源）；驱动本体与插件启用态
  无关，始终注入。模式只能经面板「循环模式」分段单选修改（写白名单）。

## Turn 循环
- 每条入站消息经[多 Agent 与会话](./core-agents.md)的临时分支机制进入
  `process_message_inner`；本层只负责循环本身
- 系统提示词按块构建（`agent/prompt.rs` 的 `build_prompt_blocks`，
  [插件化设计](../plugins/core-plugins.md) 的能力门控作用于各层）：
  基础提示词、**system 技能层**（system:true，见 [技能系统](../plugins/core-skills.md)）、
  **persona 级系统技能**（`TeamMember.system_skills` 引用）、技能清单、
  常驻/触发技能、**工作区会话**
  （workspace 插件启用且有激活会话时注入名称 + 目录清单）、输入边界规则
  （`agent/boundary.rs` 的 `BoundaryKind`：QQ hook / 定时器 / 后台输入三块文案）
- 提示词构建的锁纪律：`skills` 的 tokio Mutex guard
  只在 `build_prompt_blocks` 调用作用域内持有，构建完即 drop——guard 跨过
  LLM await 会让并行模式的第二个 turn 在 `skills.lock()` 上饿死
- 循环迭代（`max_tool_iterations`，随附配置缺省 1024；`AgentConfig::default` 代码缺省为 5）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 输出预算：`[agent].max_tokens`（0 = 无上限，后端回退 `DEFAULT_MAX_TOKENS = 128K` 实用上限）；响应被 `max_tokens` 掐断（finish_reason = length/max_tokens）时自动续跑（最多 4 次，提示"继续上次输出"喂回模型；半截工具调用丢弃后重发完整调用）
- 达上限未完成时报错收尾；工具的超时/失败不中断 loop（见 [工具系统](../plugins/core-tools.md)）

## echo-loop：可替换的默认驱动（TurnRunner）

`echo-loop` crate 把驱动抽象为 **turn/step 状态机**（对齐 dsh 的 turn/step 模型）：turn 消化一条输入直到不再欠债；step = 一次模型请求 + 它引发的工具执行。组合根经 `ctx.loop` 注册 `TurnRunner`（`LoopOptions`：`max_tokens` 与 `max_tool_iterations` 来自配置），挂载不同实现即可改变驱动行为；消费方（agent、UI、hook）只依赖生命周期事件。

- **生命周期事件**（全部经 `EventBus` 分发）：`TurnStart` / `AgentPreStep`（waterfall，可改写消息或拒绝 step）/ `StepStart` / `AgentRequest`（waterfall，可改写请求）/ `ToolCallRequested` / `ToolResult` / `StepEnd` / `TurnStopping`（serial）/ `TurnEnd`
- **工具执行管道**（`ToolPipeline`）：`pre-execute → execute → post-execute` 的 waterfall around-middleware；每个阶段收 `&ToolCall` + `next()` 句柄，不调 `next()` 即短路。审批、审计、限流等策略的设计接入方式为管道中间件（非循环代码，新策略 = 一次 `push_pre`/`push_post`；机制已备，当前生产路径未注册任何中间件）；**工具超时守卫**由 harness 侧统一实施（`Agent::tool_guard_timeout`：配置 `tool_timeout_secs` base + 工具自声明 `timeout_hint`）——超时只中止该工具、不中断 turn，结果以 notice 喂回模型，事件日志补记中断结果保持成对
- 工具经 harness 提供的 `ToolExecutor` 闭包执行，runner 本身不含任何策略代码

### 注入与分派

**注入**：组合根构建唯一的共享 `TurnRunner`（provider 已包计量 + 脱敏装饰；
`LoopOptions` 来自配置），并在两处无条件注入（`Agent::set_loop_runner`）：

- **启动期**：人格启动循环（`apply_capabilities` 之后）；
- **运行期新建人格**：`set_agent_factory` 的包装。

未注入 runner 的 `Agent`（单元测试 / 独立嵌入）在首个 turn 按自身 provider
与配置惰性构建一个默认 runner（`turn::turn_runner`）。

**分派**：所有输入（普通 / QQ hook / 定时器 / QQ 会话后台输入）都经
`process_message_inner`（`agent/turn.rs`）进入 runner：

- **输入边界**：`input_boundary` 按**内容标记**（而非会话平台）判定
  `BoundaryKind`（QQ hook / 定时器 / QQ 会话的后台输入），作为提示词边界块
  注入；普通输入不注入。投递纪律由提示词与 qq-transport 技能引导。QQ 并发
  回复的细节见 [多 Agent 与会话](./core-agents.md)。
- `RunExtras` 携带工具 schema（模型可见）、推理回调（`on_reasoning` → 发
  `AgentReasoning` 事件）、**每 turn 模型名**、**每 turn 迭代上限**
  （`max_iterations`，运行期调整下一 turn 生效）与 **ChatExecutor**（每 step
  解析当前 provider/预算——运行期 persona API 切换即时生效）、turn 标识
  （多分支并发时的事件归属）；
- 工具执行经异步 `ToolExecutor` 直连 `run_tool`（ToolCall/ToolResult 事件与
  事件日志），**超时守卫 + turn 取消竞速**（在途工具可立即中止）、工具产出
  图片透传、成功 `send_*` 触发 visible_reply 抑制；
- **UI 事件翻译**：订阅 runner 总线把 `AgentRequest`/`ModelResponse` 转发为
  `LlmRequest`/`LlmResponse`，并直接发 `AgentThinking`/`AgentCompleted`、
  失败时发 `Error`，写 `last_prompt_blocks`（/context 视图）。

工具路径（`run_tool`）与提示词构建（`agent/prompt.rs` 的 `build_prompt_blocks` /
`join_prompt_blocks`，`agent/boundary.rs` 的边界块）只有一份。
`generate_wait_reply` 是无工具单请求，保持自身实现。

### agent 模块拆分

`agent/mod.rs` 只保留 `Agent` 结构体、装配与运行期接线；按职责拆出的子模块
（架构评审后，`impl Agent` 分布在各文件）：

- `agent/turn.rs`：turn 驱动（上文「注入与分派」）+ `input_boundary`
- `agent/inbound.rs`：入站 turn 生命周期（分支登记 / 串行闸门 / 取消 / 合并、
  `process_message`、适配器入站 `process_inbound_branch` → `run_inbound_turn`）
- `agent/tool_dispatch.rs`：`run_tool` 统一分派、参数预检、中断结果补记
- `agent/streaming.rs`：流式增量转发（`AgentContentDelta` / `AgentReasoningDelta`）
- `agent/plugin_reload.rs`：数据插件热重载

- `agent/boundary.rs`：`PromptBlock`（提示词命名块，供面板 token 用量可视化）+
  `BoundaryKind`（三种输入边界的提示词文案）
- `agent/prompt.rs`：`build_prompt_blocks`（分层构建：base → system 技能 →
  persona 技能 → 技能清单 → 常驻/触发技能 → 工作区 → 边界）+ `join_prompt_blocks`
- `agent/tool_exec.rs`：`tool_arguments_error` / `invalid_tool_arguments`
  （schema 必需字段预检，错误文案说清"你发了什么、应该发什么"）+ `tool_timeout`
  （尊重工具自声明 `timeout_hint`，硬上限 600s）

`Agent::build_prompt_blocks`（无参版）保留为 `/context` 在无最近 turn 时的
代表性回退构建薄封装。

- 用户取消（`CancelRequestedWork`）时正在执行的工具被外层 select 中止，
  turn 以取消收尾
- **按 agent 路由**：`CancelRequestedWork` 携带 `team_id`（`#[serde(default)]` 向后兼容），
  命令泵按 team 路由到对应 persona——self-coding 的消息只有在 self-coding 的 Agent 中才能取消
- **取消即收尾**：TUI 路径取消同样 emit `AgentCompleted`（与 QQ 路径一致），前端据此把
  activity phase 置 completed；前端同时做乐观中断（见 [Panel 前端](../panel/panel.md)）
- 优雅排空（见 [部署与自更新](../ops/ops-deploy.md)）：收到 SIGTERM 后排空模式拒绝新消息、等待活跃 turn 完成（最多 120s）
- 重启后 turn 不恢复；悬空工具调用在投影时由 `repair_tool_pairing` 补合成结果（只作用于投影副本，不回写事件日志），显示时间线的悬空 running 条目标注为"已中断"
