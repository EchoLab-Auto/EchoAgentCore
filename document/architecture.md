---
group: 总览
x: -53
y: 805
---

# 架构总览

重构后（Phase 0-6）的架构脊柱：组合、核心 crate、能力接缝、事件、会话、扩展点。修改 `source/` 前先读此文档（治理门禁见 [开发指南](./dev-guide.md)）。

## 设计原则（dsh 模式）

本仓库按 DeepSeek Harness 的设计模式重构，五条核心原则：

1. **一切皆插件、无特权核心**：agent 循环、模型适配器、工具、技能、平台适配器都可挂载/替换；注册是可逆副作用（返回 disposer）。
2. **服务定位 + 依赖注入**：服务通过稳定键从 `Ctx` 解析（`ctx.llm`/`ctx.loop`），加载顺序由服务可用性驱动，扩展插件只依赖定义层。
3. **类型化事件 = 扩展点**：`EventBus` 的 Observe/Waterfall/Parallel/Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）。
4. **能力接缝**：Service Definition / Service Provider / Consumer 三角独立演进、独立成 crate；依赖方向单向无环。
5. **事件溯源会话日志**：日志是唯一事实来源，模型上下文由日志投影，"模型可见 ⟺ 已记录"；compaction 是显式事件。

## Crate 布局与依赖方向

```prodoc-flow
graph BT
  Defs[echo-defs<br>Service Definition]
  Ctx[echo-context<br>Ctx/EventBus/Disposer]
  Session[echo-session<br>事件溯源]
  Loop[echo-loop<br>TurnRunner]
  LLM[echo-llm-*<br>OpenAI/Anthropic/Ollama]
  Proto[echo-protocol<br>线契约]
  Adapter[echo-adapter/echo-adapter-qq<br>平台适配]
  Agent[echo-agent<br>agent 框架]
  Bin[echo-agent-core<br>组合根]
  LLM --> Defs
  Ctx --> Defs
  Session --> Defs
  Loop --> Defs
  Proto --> Defs
  Proto --> Ctx
  Adapter --> Proto
  Agent --> Adapter
  Agent --> Session
  Agent --> LLM
  Bin --> Agent
  Bin --> Loop
```

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | LLM/工具/技能/平台消息词汇与 trait，零实现、零 harness 依赖 |
| `echo-context` | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry` |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集、`EventLog` append-only 持久化、`derive_messages` 投影、compaction、`SessionHeader`、v1-v4 兼容迁移 |
| `echo-loop` | Agent 循环驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道；循环模式（单会话串行 / 并行多会话，见 Agent 循环文档） |
| `echo-llm-*` | LLM provider | OpenAI/Anthropic/Ollama 实现，只依赖 echo-defs |
| `echo-protocol` | 线契约 | `BackendCommand`/`BackendEvent`/bridge，Panel 只依赖它 |
| `echo-agent` | agent 框架 | 循环（旧实现，逐步让位于 echo-loop）、工具注册表、技能、trunk、编排、命令分发 |
| `echo-adapter`/`echo-adapter-qq` | 平台适配 | `Adapter` trait、过滤管道、ConfigStore；QQ 实现 |
| `echo-core`/`echo-server` | OneBot 类型/反向 WS | 仅供 QQ 适配器 |
| `echo-agent-core`（bin） | 组合根 | 配置加载、Ctx 装配、`ctx.llm`/`ctx.loop` 注册、启动 |

### echo-defs 模块清单与约束

定义层持有全部词汇类型与 trait，**零实现、零 harness 依赖**（仅 serde/async-trait/tokio 基础依赖），可独立测试：

| 模块 | 内容 |
|---|---|
| `message` / `llm` | `ChatMessage`/`ChatRequest`/`ChatResponse`/`ToolCall`/`ChatChunk`/`Usage` + `LlmProvider` trait + 传输无关的 `LlmError`（携带字符串，provider 自行 `map_err`，定义层不依赖 reqwest） |
| `tool` | `Tool` trait、`ToolError`、`ToolDefinition`（工具 schema 归工具域，`ChatRequest` 引用它） |
| `skill` | `Skill`/`SkillMetadata` + `SkillProvider` trait；文件发现/热重载留在 `echo-agent` 的具体 `SkillRegistry` |
| `chat` | 平台无关 `ChannelType`/`IncomingMessage`/`MessageTarget`/`SendResult`/`AdapterEvent` + `ChatAdapter` trait；门控词不在此层 |
| `mode` | `GateMode`/`ThinkingMode`/`ReasoningEffort`（自 `echo-protocol` 移入，`echo-protocol` re-export 保持 wire 路径；`echo-adapter` 不依赖 `echo-protocol`） |
| `token` | 纯 token 估算/截断函数 |
| `session` | `SessionEvent` + `SessionStore` trait（事件溯源会话契约） |

依赖方向收敛为 `echo-defs ◄ echo-protocol ◄ echo-adapter ◄ echo-agent`，扩展插件只依赖定义层；旧 crate（`echo-agent`/`echo-adapter`/`echo-protocol`）re-export `echo_defs` 类型，保持 `echo_agent::…` 等路径兼容。

### echo-context 机制

最底层机制 crate（仅依赖 tokio/dashmap，不依赖任何 echo-* crate）：

- **`Ctx` 服务定位**：按字符串 key 注册/解析；注册 `Arc<dyn Trait>`（trait 对象存入 `Box<dyn Any>`，resolve 时 downcast 回同一类型）或具体 `Clone + 'static` 值。字符串键便于配置与诊断，且允许同一 trait 多实例按名共存
- **`Disposer` 可逆注册**：`Ctx::register` / `EventBus::subscribe` / 注册表方法都返回 `Disposer`；注册必须持有 disposer 才生效（组合根持有 `ctx` 注册），装配方需明确管理生命周期
- **`EventBus` 类型化事件**：`Event` trait（Any + Clone + Debug + Send + Sync），按事件 `TypeId` 分组注册监听器；四种分发——`Observe`（扇出）/ `Waterfall`（around-middleware，`next()` 委托、不调即短路）/ `Parallel`（每监听器一份事件副本并发，故要求 `Event: Clone`）/ `Serial`（按序，首个拒绝停止）。`emit_sync` 覆盖同步热路径（Observe/Waterfall），`emit` 支持全部模式。`echo-protocol` 为 `BackendEvent` 实现 `Event`（声明成员资格），依赖方向仍为 context ◄ protocol
- **`ScopedRegistry` 作用域注册**：全局条目 + per-scope 条目，lookup 先 scoped 后 global（shadowing），scope 卸载时条目随 disposer 撤销
- 显示时间线投影（`TimelineProjector`）是 EventBus 的 observe 监听器：`Agent` 不直接写 timeline，timeline 是事件的投影消费者，可独立演进/测试

### LLM provider crates（echo-llm-*）

- 三个 provider 各自独立成 crate，只依赖 `echo-defs`（词汇/trait/策略枚举/token 纯函数）+ 传输依赖（reqwest/futures/tokio），可独立演进与测试；`echo-llm-ollama` 是薄包装，复用 `echo-llm-openai` 指向 `{base}/v1`
- `echo-agent` 只保留 `create_provider` 工厂与装配，不承载 provider 实现；新增 provider = 一个新 crate 实现 `echo_defs::LlmProvider` + 注册（后续以 `ctx.llm` 注册表查找替代工厂字符串 match，即可运行期热替换）

## 能力接缝

一个可替换能力 = 三角色。当前接缝：

| Seam | Service Definition | Provider | Consumer |
|---|---|---|---|
| LLM | `echo_defs::LlmProvider` | `echo-llm-openai`/`anthropic`/`ollama` | `echo-agent` 工厂 + `echo-loop` runner |
| 工具 | `echo_defs::Tool` | `echo-agent::ToolRegistry` + builtin | agent 循环 |
| 技能 | `echo_defs::SkillProvider` | `echo-agent::SkillRegistry`（本地文件） | prompt 组装 |
| 平台生命周期 | `echo_defs::chat::ChatAdapter` | `echo-adapter-qq`（经 Adapter 收敛中） | agent + 工具 |
| 默认驱动 | `echo_loop::TurnRunner` | 内置（经 `ctx.loop` 注册） | 组合根 |

## 媒体库（2026-09-24）

入站图片（QQ 消息图、面板上传图）**落盘为媒体文件，链路上只传引用**：
`~/.local/share/echo-agent-core/media/<内容哈希>.<ext>`（`$ECHO_MEDIA_DIR` 可覆盖），
引用为 `/media/<id>`（Panel web 后端同源提供，浏览器懒加载 + 强缓存）。

- 落盘侧：adapter-qq 下载远端图后落盘；SendMessage（面板上传 data URI）入站落盘；
  遗留内嵌图由加载期迁移（`spill_event_media` + 时间线遍历）一次性改写（幂等）
- 模型侧：投影出口（`TrunkStore::reproject_one`）把引用还原为 data URI——
  发往 LLM 的请求与改造前无差别
- 背景与实测：单个 GIF 表情包数 MB，内嵌在 hook JSON / 事件日志 / 时间线三处，
  alix 的时间线快照 8MB → 面板启动 802ms、会话文件 24.6MB；改造后 57KB /
  299ms / 225KB（图片全保留）

## 命令分发

- QQ 命令域独立：`apply_qq_command`（`agent/qq_commands.rs`）承载名单/门控/群列表/好友列表等 QQ 变体，主 `apply_command` 的对应分支为一行委托，核心分支原地保留
- `CommandRegistry` + `CommandHandler` trait（dyn-compatible，async）是开放命令扩展点的基础设施：封闭枚举 `BackendCommand` 下编译器 match 完备性仍由主分发器承担，注册表留待协议开放后承载外部命令插件

## 结构化输入标记

`<qq_message_hook>`/`<backend_message_hook>`/`<timer_event>` 三个结构化输入标记的单一事实来源是 echo-agent 的 `input_marker.rs`：构造（`wrap_hook`/`wrap_timer`）、判定（`STRUCTURED_INPUT_MARKERS` 常量表 + `is_structured_input`）、解析（`structured_message_sequence`——限制在标记开头，防普通文本误读）集中于此，改格式只改一处。后续演进方向：来源判定从 content 字符串改为 `MessageReceived` 的结构化 origin 字段（跨仓库 wire 变更）。

## 事件模型

- **生命周期事件**（echo-loop，经 EventBus）：`TurnStart`/`AgentPreStep`（waterfall，可改写/拒绝）/`StepStart`/`AgentRequest`（waterfall）/`ToolCallRequested`/`ToolResult`/`StepEnd`/`TurnStopping`/`TurnEnd`。
- **会话事件**（echo-session，append-only 日志）：`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Compaction`——模型上下文由 `derive_messages` 投影，加载时 `repair_tool_pairing` 修复悬空调用。
- **协议事件**（echo-protocol，经 management WS）：`BackendEvent`，Panel 消费（详见 [协议与数据流](./protocol.md)）。
- **能力事件**：timeline 投影（`TimelineProjector`）作为 EventBus observe 监听器，从事件流记录显示历史。

## Turn/Step 循环

`TurnRunner` 驱动：`turn/start → agent/pre-step → step/start → agent/request → llm.chat → tool/call → pipeline(pre/execute/post) → step/end →（循环）→ turn-stopping → turn/end`。工具策略（超时/审批/审计/限流）是 `ToolPipeline` 中间件，不是循环代码。`Agent::process_message_inner` 为既有实现，逐步由 `ctx.loop` 取代。

## 会话与持久化

- 事件溯源：`echo-session` 的 `EventLog` 是权威（append-only，整档 JSON 持久化 version=5，tmp+rename 原子写）；`TrunkStore` 持 `EventLog`，`trunk_history` 降级为内存投影缓存（`append_event` 在锁内 append 并重新投影）；持久化 v5 以 `events` 为权威
- `SessionEvent` 五类：`UserMessage`（含 `message_sequence` 供并发分支按请求序合并）、`AssistantMessage`（完整保留 `reasoning_content` 与 `tool_calls`）、`ToolCall`、`ToolResult`（带 `tool_call_id` 回链）、`Compaction`（摘要替换前缀的显式压缩事件，日志保持 append-only 可重放）
- `derive_messages` / `project_messages` 是唯一的模型上下文投影：裁剪只在投影期（`trim_to_budget`），绝不破坏日志；`insert_after_sequence` 把并发分支回复插入对应请求事件之后，投影顺序 = 请求到达顺序
- 兼容：v1-v4 旧格式（`echo-sessions.json`）经 `migrate_v4_document` 读入并迁移进事件日志；旧格式的工具结构缺失如实保留（迁移后的工具消息降级为纯文本，新写日志保留完整结构）
- 每个 persona 独立会话文件（`echo-sessions-{id}.json`）；显示时间线带条目级 `seq` 支持增量同步
- `SessionHeader` 携带 fork/resume 元数据（parent/seed_length/origin/delegation_depth），随日志持久化

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`，注册 `ctx.llm` |
| 新增工具 | 实现 `echo_defs::Tool`，注册进 ToolRegistry |
| 新增平台 | 实现 `ChatAdapter`，注册 `ctx.chat(platform)` |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略（超时/审批） | `ToolPipeline::push_pre/push_post` |
| 新增命令 | 命令分发按域拆分（`apply_qq_command` 模式）+ CommandRegistry（开放后） |
| 会话状态扩展 | 扩展 `SessionEventMap`（echo-session），从日志渲染 |

## 关键文档

- [开发指南](./dev-guide.md)：仓库结构、关键抽象、构建与测试、治理门禁
