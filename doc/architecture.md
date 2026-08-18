# EchoAgentCore Architecture

中文。重构后(Phase 0-6)的架构脊柱:组合、核心 crate、能力接缝、事件、
会话、扩展点。修改 `packages/` 前先读此文档;决策理由在
[doc/decisions](decisions/README.md)。

## 设计原则(dsh 模式)

本仓库按 DeepSeek Harness 的设计模式重构,五条核心原则:

1. **一切皆插件、无特权核心**:agent 循环、模型适配器、工具、技能、平台
   适配器都可挂载/替换;注册是可逆副作用(返回 disposer)。
2. **服务定位 + 依赖注入**:服务通过稳定键从 `Ctx` 解析(`ctx.llm`/
   `ctx.loop`),加载顺序由服务可用性驱动,扩展插件只依赖定义层。
3. **类型化事件 = 扩展点**:`EventBus` 的 Observe/Waterfall/Parallel/Serial
   四种分发;waterfall 是 around-middleware(`next()` 委托、不调即短路)。
4. **能力接缝**:Service Definition / Service Provider / Consumer 三角独立
   演进、独立成 crate;依赖方向单向无环。
5. **事件溯源会话日志**:日志是唯一事实来源,模型上下文由日志投影,
   "模型可见 ⟺ 已记录";compaction 是显式事件。

## Crate 布局与依赖方向

```
echo-defs ◄── echo-protocol ◄── echo-adapter ◄── echo-agent ◄── echo-agent-core (bin)
    ▲              ▲                ▲   ▲                          ▲
    └── echo-context ◄──────────────┴───┴── echo-adapter-qq ◄──────┘
    ▲                                                     (→ echo-core, echo-server)
    ├── echo-session ◄── echo-agent
    ├── echo-loop ◄── echo-agent-core (composition root 经 ctx.loop)
    ├── echo-llm-openai/anthropic/ollama ◄── echo-agent (factory)
    └── echo-chat-capability ◄── echo-agent (DeliveryPolicy seam)
```

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | LLM/工具/技能/平台消息词汇与 trait,零实现、零 harness 依赖 |
| `echo-context` | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry` |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集、`EventLog` append-only 持久化、`derive_messages` 投影、compaction、`SessionHeader`、v1-v4 兼容迁移 |
| `echo-loop` | 默认 agent 驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道 |
| `echo-llm-*` | LLM provider | OpenAI/Anthropic/Ollama 实现,只依赖 echo-defs |
| `echo-chat-capability` | 平台能力定义 | `DeliveryPolicy`/`DeliveryTarget`(交付策略接缝) |
| `echo-protocol` | 线契约 | `BackendCommand`/`BackendEvent`/bridge,Panel 只依赖它 |
| `echo-agent` | agent 框架 | 循环(旧实现,逐步让位于 echo-loop)、工具注册表、技能、trunk、编排、命令分发 |
| `echo-adapter`/`echo-adapter-qq` | 平台适配 | `Adapter` trait、过滤管道、ConfigStore;QQ 实现 |
| `echo-core`/`echo-server` | OneBot 类型/反向 WS | 仅供 QQ 适配器 |
| `echo-agent-core`(bin) | 组合根 | 配置加载、Ctx 装配、`ctx.llm`/`ctx.loop` 注册、启动 |

## 能力接缝

一个可替换能力 = 三角色。当前接缝:

| Seam | Service Definition | Provider | Consumer |
|---|---|---|---|
| LLM | `echo_defs::LlmProvider` | `echo-llm-openai`/`anthropic`/`ollama` | `echo-agent` 工厂 + `echo-loop` runner |
| 工具 | `echo_defs::Tool` | `echo-agent::ToolRegistry` + builtin + orchestration | agent 循环 |
| 技能 | `echo_defs::SkillProvider` | `echo-agent::SkillRegistry`(本地文件) | prompt 组装 |
| 交付策略 | `echo_chat_capability::DeliveryPolicy` | `echo-agent::QqDeliveryPolicy` | agent 循环 |
| 平台生命周期 | `echo_defs::chat::ChatAdapter` | `echo-adapter-qq`(经 Adapter 收敛中) | agent + 工具 |
| 默认驱动 | `echo_loop::TurnRunner` | 内置(经 `ctx.loop` 注册) | 组合根 |

## 事件模型

- **生命周期事件**(echo-loop,经 EventBus):`TurnStart`/`AgentPreStep`
  (waterfall,可改写/拒绝)/`StepStart`/`AgentRequest`(waterfall)/
  `ToolCallRequested`/`ToolResult`/`StepEnd`/`TurnStopping`/`TurnEnd`。
- **会话事件**(echo-session,append-only 日志):`UserMessage`/
  `AssistantMessage`/`ToolCall`/`ToolResult`/`Compaction`——模型上下文由
  `derive_messages` 投影。
- **协议事件**(echo-protocol,经 management WS):`BackendEvent`,TUI 消费。
- **能力事件**:timeline 投影(`TimelineProjector`)作为 EventBus observe
  监听器,从事件流记录显示历史。

## Turn/Step 循环

`TurnRunner` 驱动:`turn/start → agent/pre-step → step/start →
agent/request → llm.chat → tool/call → pipeline(pre/execute/post) →
step/end → (循环)→ turn-stopping → turn/end`。工具策略(超时/审批/审计/
限流)是 `ToolPipeline` 中间件,不是循环代码。`Agent::process_message_inner`
为既有实现,逐步由 `ctx.loop` 取代。

## 会话与持久化

- 事件溯源:`echo-session` 的 `EventLog` 是权威;`TrunkStore` 持 `EventLog`,
  `trunk_history` 为投影缓存;持久化 v5 以 `events` 为权威。
- 兼容:v1-v4 旧格式(`echo-sessions.json`)经 `migrate_v4_document` 读入。
- `SessionHeader` 携带 fork/resume 元数据(parent/seed_length/origin/
  delegation_depth)。

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`,注册 `ctx.llm` |
| 新增工具 | 实现 `echo_defs::Tool`,注册进 ToolRegistry |
| 新增平台 | 实现 `ChatAdapter` + `DeliveryPolicy`,注册 `ctx.chat(platform)` |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略(超时/审批) | `ToolPipeline::push_pre/push_post` |
| 新增命令 | 命令分发按域拆分(`apply_qq_command` 模式)+ CommandRegistry(开放后) |
| 会话状态扩展 | 扩展 `SessionEventMap`(echo-session),从日志渲染 |

## 关键文档

- [doc/decisions](decisions/README.md):ADR-0001 至 0009(重构决策全记录)
- [doc/develop](develop/README.md):开发文档(协议/安装/门控/测试)
- EchoAgentPanel 仓库 `doc/decisions/`:Panel 侧 ADR(Panel-0001 至 0005)
