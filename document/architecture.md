---
id: architecture
title: "架构总览"
group: 总览
x: 320
y: 0
---

# 架构总览

> **定位**：本文是 EchoAgent 的**架构脊柱**——设计原则、crate 布局与依赖方向、能力接缝、运行时一览、扩展点地图。读者：需要理解系统骨架、准备修改 `source/` 的开发者（治理门禁见 [开发指南](./dev-guide.md)）。
> 阅读入口：[Core 后端](./core.md)；内核机制见 [框架（内核）](./frame.md) 与 [插件化设计](./core-plugins.md)；运行时子系统见 [多 Agent 与会话](./core-agents.md)、[Agent 循环](./core-agent-loop.md) 等（各见正文链接）。

## 设计原则（dsh 模式）

本仓库按 DeepSeek Harness 的设计模式组织，五条核心原则：

1. **一切皆插件、无特权核心**：agent 循环、模型适配器、工具、技能、平台适配器都可挂载/替换；注册是可逆副作用（返回 disposer）。
2. **服务定位 + 依赖注入**：服务通过稳定键从 `Ctx` 解析（`ctx.llm` / `ctx.loop`），加载顺序由服务可用性驱动，扩展插件只依赖定义层。
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
  Adapter --> Defs
  Agent --> Adapter
  Agent --> Session
  Agent --> LLM
  Bin --> Agent
  Bin --> Loop
```

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | 全部词汇与 trait：LLM/工具/技能/平台消息/媒体/会话事件/联邦节点；零实现、零 harness 依赖 |
| `echo-context` | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry` |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集、`EventLog` append-only 持久化、`derive_messages` 投影、compaction、`SessionHeader`、v1-v4 兼容迁移 |
| `echo-loop` | Agent 循环驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道；循环模式（单会话串行 / 并行多会话）见 [Agent 循环](./core-agent-loop.md) |
| `echo-llm-*` | LLM provider | OpenAI/Anthropic/Ollama 实现，只依赖 `echo-defs` |
| `echo-protocol` | 线契约 | `BackendCommand`/`BackendEvent`/bridge，Panel 只依赖它 |
| `echo-plugin` / `-api` / `-host` | 插件契约、协议与宿主 | 生命周期钩子（`Plugin`/`PluginManifest`/`PluginRegistry`）；宿主↔插件消息（握手/调用/取消/事件/排空）、贡献类型与版本协商；Transport（inproc/stdio）、监督与崩溃重启、conformance 测试（见 [完全解耦推进计划](./decoupling-plan.md)） |
| `echo-agent` | agent 框架 | 循环（内置实现；loop.* 插件启用时 echo-loop 驱动接管普通输入——生效口径见 [Agent 循环](./core-agent-loop.md)）、工具注册表与各包（`packages/`）、技能、trunk、异步子任务（`spawn_subagent`）、命令分发 |
| `echo-adapter` / `echo-adapter-qq` | 平台适配 | `Adapter` trait、过滤管道、ConfigStore、QQ 实现（OneBot 类型/反向 WS 在 `echo-core`/`echo-server`，仅供 QQ 适配器） |
| `echo-federation` | 联邦链路（Core↔Core） | `FedFrame` 线协议、Hello/Welcome 握手、per-peer 认证、心跳/重连/回环防护、邀请串；详见 [联邦](./federation.md) |
| `echo-agent-core`（bin） | 组合根 | 配置加载、Ctx 装配、`ctx.llm`/`ctx.loop` 注册、联邦路由泵、启动 |

依赖方向收敛为单向下游：`echo-defs` / `echo-context`（底座，零 echo-* 依赖）◄ `echo-protocol` / `echo-adapter` / `echo-session` / `echo-llm-*`（中间层，只依赖底座）◄ `echo-agent` ◄ `echo-agent-core`（bin）——`echo-adapter` 只依赖定义层、不依赖 `echo-protocol`；扩展插件只依赖定义层。旧 crate（`echo-agent`/`echo-adapter`/`echo-protocol`）re-export `echo_defs` 类型，保持 `echo_agent::…` 等路径兼容。

### 定义层（echo-defs）

定义层持有全部词汇类型与 trait，**零实现、零 harness 依赖**（依赖仅 serde/serde_json/async-trait/tokio/thiserror/base64/tracing 基础 crate，不含 reqwest 与其它 echo-* crate），可独立测试。模块覆盖：消息/LLM（`ChatMessage`/`ChatRequest`/`LlmProvider` 等）、工具、技能、聊天（平台消息词汇与 `ChatAdapter`）、门控与思考模式、token 纯函数、媒体卫生（文本内嵌 base64 → 占位符、图片只走独立 image 块）与入站图片落盘缓存（`/media/<id>` 引用）、会话事件契约、联邦节点词汇（`NodeId`、`node://` 跨机命名空间）。

### 机制底座（echo-context）

最底层机制 crate（仅依赖 tokio/dashmap，不依赖任何 echo-* crate）：

- **`Ctx` 服务定位**：按字符串 key 注册/解析；注册 `Arc<dyn Trait>`（trait 对象存入 `Box<dyn Any>`，resolve 时 downcast 回同一类型）或具体 `Clone + 'static` 值——字符串键便于配置与诊断，且允许同一 trait 多实例按名共存
- **`Disposer` 可逆注册**：`Ctx::register` / `EventBus::subscribe` / 注册表方法都返回 `Disposer`；注册必须持有 disposer 才生效（组合根持有 `ctx` 注册），装配方需明确管理生命周期
- **`EventBus` 类型化事件**：`Event` trait（Any + Clone + Debug + Send + Sync），按事件 `TypeId` 分组监听器；四种分发——`Observe`（扇出）/ `Waterfall`（around-middleware，不调 `next()` 即短路）/ `Parallel`（每监听器一份事件副本并发）/ `Serial`（按序；**当前实现与 `Observe` 等价**，「首个拒绝即停止」为预留语义）。`emit_sync` 覆盖同步热路径（Observe/Waterfall），`emit` 支持全部模式；`echo-protocol` 为 `BackendEvent` 实现 `Event`（依赖方向仍为 context ◄ protocol）
- **`ScopedRegistry` 作用域注册**：全局条目 + per-scope 条目，lookup 先 scoped 后 global（shadowing），scope 卸载时条目随 disposer 撤销

### LLM provider crates

三个 provider 各自独立成 crate，只依赖 `echo-defs`（词汇/trait/策略枚举/token 纯函数）+ 传输依赖（reqwest/futures/tokio），可独立演进与测试；`echo-llm-ollama` 是薄包装，复用 `echo-llm-openai` 指向 `{base}/v1`。`echo-agent` 只保留 `create_provider` 工厂与装配，不承载 provider 实现；新增 provider = 一个新 crate 实现 `echo_defs::LlmProvider` + 注册（演进方向：以 `ctx.llm` 注册表查找替代工厂字符串 match，支持运行期热替换）。

## 能力接缝

一个可替换能力 = 三角色。当前接缝：

| Seam | Service Definition | Provider | Consumer |
|---|---|---|---|
| LLM | `echo_defs::LlmProvider` | `echo-llm-openai`/`anthropic`/`ollama` | `echo-agent` 工厂 + `echo-loop` runner |
| 工具 | `echo_defs::Tool` | `echo-agent::ToolRegistry` + builtin | agent 循环 |
| 技能 | `echo_defs::SkillProvider` | `echo-agent::SkillRegistry`（本地文件） | prompt 组装 |
| 平台生命周期 | `echo_defs::chat::ChatAdapter` | `echo-adapter-qq`（经 Adapter 收敛中） | agent + 工具 |
| 默认驱动 | `echo_loop::TurnRunner` | 内置（经 `ctx.loop` 注册） | 组合根 |

## 运行时一览

内核之上的运行时子系统综述——每节只留要点，细节见对应子系统文档。

### 事件模型

事件分四类：**生命周期事件**（echo-loop 经 EventBus 分发——`TurnStart`/`AgentPreStep`/`StepStart`/`AgentRequest`/`ToolCallRequested`/`ToolResult`/`StepEnd`/`TurnStopping`/`TurnEnd`）；**会话事件**（echo-session append-only 日志——`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Compaction`，模型上下文由 `derive_messages` 投影，加载时 `repair_tool_pairing` 修复悬空调用）；**协议事件**（echo-protocol 的 `BackendEvent`，经 management WS 供 Panel 消费，见 [协议与数据流](./protocol.md)）；**能力事件**（timeline 投影作为 EventBus observe 监听器——`Agent` 不直接写 timeline，显示历史是事件的投影消费者，可独立演进/测试）。

### Turn/Step 循环

`TurnRunner` 驱动 `turn/start → agent/pre-step → step/start → agent/request → llm.chat → tool/call → pipeline(pre/execute/post) → step/end →（循环）→ turn-stopping → turn/end`。调度口径：loop.* 插件启用时普通输入经该状态机执行（启动期回填注入 + 运行期工厂覆盖 + 插件停/再启联动），QQ hook/定时器/QQ 会话始终走内置循环；工具策略（超时/审批/审计/限流）的设计位置是 `ToolPipeline` 中间件（当前生产路径未注册任何中间件，超时由 harness `tool_guard_timeout` 守卫）。细节与边界见 [Agent 循环](./core-agent-loop.md)。

### 会话与持久化

事件溯源：`EventLog` 是权威（append-only，整档 JSON v6，tmp+rename 原子写）；`SessionEvent` 五类（`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Compaction`），压缩落地为显式 `Compaction` 事件（被替换条数 + 摘要 + 可选归档快照），日志保持可重放。投影：`derive_messages` 是唯一的模型上下文出口——多会话按 `session` 归属逐会话投影，裁剪只在投影期、绝不破坏日志；显示时间线带条目级 `seq` 支持增量同步。归档与保留：手动 `ArchiveHistory` 与压缩前自动快照写入 `archives/`（v6 完整快照，保留最新 20 份）；v1-v4 旧格式加载期自动迁移，每个 persona 独立会话文件。细节见 [会话记忆](./core-memory.md)（日志/投影/压缩）与 [配置持久化](./core-config-persistence.md)（文件布局）。

### 媒体库

入站图片（QQ 消息图、面板上传图）**落盘为媒体文件，链路上只传引用**：`~/.local/share/echo-agent-core/media/`（`$ECHO_MEDIA_DIR` 可覆盖），引用为 `/media/<id>`（Panel 后端同源提供，浏览器懒加载 + 强缓存）。落盘侧：QQ 适配器在**触发门控与过滤通过后**才下载远端图（被丢弃的消息不白存图），SendMessage（面板上传 data URI）入站落盘；遗留内嵌图由加载期迁移一次性改写（幂等）。模型侧：投影出口把引用还原为 data URI——发往 LLM 的请求与内嵌时代无差别。细节见 [多模态输入](./core-multimodal.md)。

### 命令分发

QQ 命令域独立：`apply_qq_command`（`packages/adapter_qq/commands.rs`）承载名单/门控/群列表/好友列表等 QQ 变体，主 `apply_command` 的对应分支为一行委托，核心分支原地保留。`CommandRegistry` + `CommandHandler` trait（dyn-compatible，async）是开放命令扩展点的基础设施：封闭枚举 `BackendCommand` 下编译器 match 完备性仍由主分发器承担，注册表留待协议开放后承载外部命令插件。

### 结构化输入标记

`<qq_message_hook>`/`<backend_message_hook>`/`<timer_event>` 三个结构化输入标记的单一事实来源是 echo-agent 的 `input_marker.rs`：构造（`wrap_hook`/`wrap_timer`）、判定（`STRUCTURED_INPUT_MARKERS` 常量表）、解析（`structured_message_sequence`，限制在标记开头、防普通文本误读）集中一处，改格式只改一处。演进方向：来源判定从 content 字符串改为 `MessageReceived` 的结构化 origin 字段（跨仓库 wire 变更）。

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`，注册 `ctx.llm` |
| 新增工具 | 实现 `echo_defs::Tool`，注册进 ToolRegistry |
| 新增平台 | 实现 `ChatAdapter`，经 `AdapterRegistry` 接入（`ctx.chat` 服务键为规划） |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略 | 超时已由 harness `tool_guard_timeout` 守卫（`agent/tool_exec.rs`）；`ToolPipeline` 中间件机制已备、生产路径未接线 |
| 新增命令 | 命令分发按域拆分（`apply_qq_command` 模式）+ CommandRegistry（开放后） |
| 会话状态扩展 | 扩展 `SessionEvent` 枚举（echo-session），从日志渲染 |

## 关键文档

- [文档总览](./index.md)：文档群入口与阅读路径；[开发指南](./dev-guide.md)：仓库结构、关键抽象、构建与测试、治理门禁
- [Core 后端](./core.md) → [框架（内核）](./frame.md) / [插件化设计](./core-plugins.md)：框架与插件两支主线
- 运行时子系统：[多 Agent 与会话](./core-agents.md)、[Agent 循环](./core-agent-loop.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)
- 数据流与部署：[协议与数据流](./protocol.md)、[部署与自更新](./ops-deploy.md)
