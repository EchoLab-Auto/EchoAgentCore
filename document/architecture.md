---
group: 总览
x: 48
y: 216
---

# 架构总览

重构后（Phase 0-6）的架构脊柱：组合、核心 crate、能力接缝、事件、会话、扩展点。修改 `source/` 前先读此文档；决策理由在 [架构决策](./adr-index.md)。

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
  Chat[echo-chat-capability<br>DeliveryPolicy]
  Proto[echo-protocol<br>线契约]
  Adapter[echo-adapter/echo-adapter-qq<br>平台适配]
  Agent[echo-agent<br>agent 框架]
  Bin[echo-agent-core<br>组合根]
  LLM --> Defs
  Chat --> Defs
  Ctx --> Defs
  Session --> Defs
  Loop --> Defs
  Proto --> Defs
  Proto --> Ctx
  Adapter --> Proto
  Agent --> Adapter
  Agent --> Session
  Agent --> LLM
  Agent --> Chat
  Bin --> Agent
  Bin --> Loop
```

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | LLM/工具/技能/平台消息词汇与 trait，零实现、零 harness 依赖 |
| `echo-context` | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry` |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集、`EventLog` append-only 持久化、`derive_messages` 投影、compaction、`SessionHeader`、v1-v4 兼容迁移 |
| `echo-loop` | 默认 agent 驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道 |
| `echo-llm-*` | LLM provider | OpenAI/Anthropic/Ollama 实现，只依赖 echo-defs |
| `echo-chat-capability` | 平台能力定义 | `DeliveryPolicy`/`DeliveryTarget`（交付策略接缝） |
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

## 能力接缝

一个可替换能力 = 三角色。当前接缝：

| Seam | Service Definition | Provider | Consumer |
|---|---|---|---|
| LLM | `echo_defs::LlmProvider` | `echo-llm-openai`/`anthropic`/`ollama` | `echo-agent` 工厂 + `echo-loop` runner |
| 工具 | `echo_defs::Tool` | `echo-agent::ToolRegistry` + builtin + orchestration | agent 循环 |
| 技能 | `echo_defs::SkillProvider` | `echo-agent::SkillRegistry`（本地文件） | prompt 组装 |
| 交付策略 | `echo_chat_capability::DeliveryPolicy` | `echo-agent::QqDeliveryPolicy` | agent 循环 |
| 平台生命周期 | `echo_defs::chat::ChatAdapter` | `echo-adapter-qq`（经 Adapter 收敛中） | agent + 工具 |
| 默认驱动 | `echo_loop::TurnRunner` | 内置（经 `ctx.loop` 注册） | 组合根 |

## 事件模型

- **生命周期事件**（echo-loop，经 EventBus）：`TurnStart`/`AgentPreStep`（waterfall，可改写/拒绝）/`StepStart`/`AgentRequest`（waterfall）/`ToolCallRequested`/`ToolResult`/`StepEnd`/`TurnStopping`/`TurnEnd`。
- **会话事件**（echo-session，append-only 日志）：`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Compaction`——模型上下文由 `derive_messages` 投影，加载时 `repair_tool_pairing` 修复悬空调用。
- **协议事件**（echo-protocol，经 management WS）：`BackendEvent`，Panel 消费（详见 [协议与数据流](./protocol.md)）。
- **能力事件**：timeline 投影（`TimelineProjector`）作为 EventBus observe 监听器，从事件流记录显示历史。

## Turn/Step 循环

`TurnRunner` 驱动：`turn/start → agent/pre-step → step/start → agent/request → llm.chat → tool/call → pipeline(pre/execute/post) → step/end →（循环）→ turn-stopping → turn/end`。工具策略（超时/审批/审计/限流）是 `ToolPipeline` 中间件，不是循环代码。`Agent::process_message_inner` 为既有实现，逐步由 `ctx.loop` 取代。

## 会话与持久化

- 事件溯源：`echo-session` 的 `EventLog` 是权威；`TrunkStore` 持 `EventLog`，`trunk_history` 为投影缓存；持久化 v5 以 `events` 为权威。
- 兼容：v1-v4 旧格式（`echo-sessions.json`）经 `migrate_v4_document` 读入。
- 每个 persona 独立会话文件（`echo-sessions-{id}.json`）；显示时间线带条目级 `seq` 支持增量同步。
- `SessionHeader` 携带 fork/resume 元数据（parent/seed_length/origin/delegation_depth）。

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`，注册 `ctx.llm` |
| 新增工具 | 实现 `echo_defs::Tool`，注册进 ToolRegistry |
| 新增平台 | 实现 `ChatAdapter` + `DeliveryPolicy`，注册 `ctx.chat(platform)` |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略（超时/审批） | `ToolPipeline::push_pre/push_post` |
| 新增命令 | 命令分发按域拆分（`apply_qq_command` 模式）+ CommandRegistry（开放后） |
| 会话状态扩展 | 扩展 `SessionEventMap`（echo-session），从日志渲染 |

## 关键文档

- [架构决策](./adr-index.md)：ADR 0003-0018 全记录（0001/0002/0011 已并入主文档；含 Panel 侧 0016/0017）
- [开发指南](./dev-guide.md)：仓库结构、关键抽象、构建与测试
