---
id: architecture
title: "架构总览"
group: 总览
x: 320
y: 500
---

# 架构总览

> **定位**：本文是 EchoAgent 的**架构脊柱**——设计原则、crate 布局与依赖方向、能力接缝、运行时一览、扩展点地图。读者：需要理解系统骨架、准备修改 `source/` 的开发者（治理门禁见 [开发指南](../dev/dev-guide.md)）。
> 阅读入口：[Core 后端](../core/core.md)；内核机制见 [框架（内核）](../core/frame.md) 与 [插件化设计](../plugins/core-plugins.md)；运行时子系统见 [多 Agent 与会话](../core/core-agents.md)、[Agent 循环](../core/core-agent-loop.md) 等（各见正文链接）。

## 设计原则（dsh 模式）

本仓库按 DeepSeek Harness 的设计模式组织，五条核心原则：

1. **一切皆插件、无特权核心**：agent 循环、模型插件实例、工具、技能、平台插件实例都可挂载/替换；注册是可逆副作用（返回 disposer）。
2. **服务定位 + 依赖注入**：服务通过稳定键从 `Ctx` 解析（`ctx.llm` / `ctx.loop`），加载顺序由服务可用性驱动，扩展插件只依赖定义层。
3. **类型化事件 = 扩展点**：`EventBus` 的 Observe/Waterfall/Parallel/Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）。
4. **能力接缝**：Service Definition / Service Provider / Consumer 三角独立演进、独立成 crate；依赖方向单向无环。
5. **事件溯源会话日志**：日志是唯一事实来源，模型上下文由日志投影——**"模型可见 ⇒ 已记录"**（模型上下文里的每条内容都能在日志中找到出处）；反向不成立：推理（thinking）正文同样入日志以保住显示时序，但投影时跳过、永不进入模型请求。compaction 是显式事件。

## Crate 布局与依赖方向

```prodoc-flow
graph BT
  Defs[echo-defs<br>Service Definition<br>（PluginInstance trait / InstanceManagement 在此）]
  Ctx[echo-context<br>Ctx/EventBus/Disposer]
  Session[echo-session<br>事件溯源]
  Loop[echo-loop<br>TurnRunner]
  LLM[echo-llm-*<br>OpenAI/Anthropic/Ollama]
  Proto[echo-protocol<br>线契约]
  Sanitize[echo-sanitize<br>脱敏]
  Config[echo-config<br>TOML 原子读改写]
  Fed[echo-federation<br>Core↔Core 联邦]
  Plugin[echo-plugin<br>生命周期钩子]
  PluginApi[echo-plugin-api<br>协议与贡献类型]
  PluginLoader[echo-plugin-loader]
  PluginSdk[echo-plugin-sdk<br>插件 SDK]
  PluginHost[echo-plugin-host<br>运输/监督（领域中立）]
  PluginAdapter[echo-plugin-adapter<br>贡献→领域 trait]
  Shell[echo-shell<br>Shell 会话机制]
  Agent[echo-agent<br>agent 框架]
  Bin[echo-agent-core<br>组合根]
  LLM --> Defs
  Session --> Defs
  Loop --> Defs
  Loop --> Ctx
  Proto --> Defs
  Proto --> Ctx
  Sanitize --> Defs
  Sanitize --> Ctx
  Shell --> Ctx
  Shell --> Proto
  Fed --> Defs
  Plugin --> Defs
  Plugin --> Ctx
  PluginSdk --> PluginApi
  PluginHost --> PluginApi
  PluginHost --> PluginLoader
  PluginAdapter --> PluginHost
  PluginAdapter --> PluginApi
  PluginAdapter --> Defs
  Agent --> Session
  Agent --> LLM
  Agent --> Loop
  Agent --> Proto
  Agent --> Plugin
  Agent --> Fed
  Agent --> Ctx
  Agent --> Defs
  Bin --> Agent
  Bin --> Loop
  Bin --> PluginHost
  Bin --> PluginAdapter
  Bin --> Fed
```

> 依赖边以 `cargo metadata --format-version 1 --no-deps`（仅 normal 依赖）实测为准。`echo-context` **不依赖** `echo-defs`（零 echo-* 依赖）；组合根另直接依赖全部中间层 crate（`echo-agent`/`echo-config`/`echo-context`/`echo-defs`/`echo-federation`/`echo-loop`/`echo-plugin`/`echo-plugin-adapter`/`echo-plugin-host`/`echo-plugin-loader`/`echo-plugin-loop`/`echo-plugin-management`/`echo-protocol`/`echo-sanitize`/`echo-session`），图中只画主干边（Phase 3 外迁的七个内置插件各自只依赖定义层/机制层，边略）。**依赖白名单的单一真值是 `scripts/deps-policy.txt`**——`scripts/check-deps.sh`（CI `deps-lint`）与 `source/core/tests/plugin_dependency_direction.rs`（cargo test）共读同一份，新增 crate 未登记即失败。

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | 全部词汇与 trait：LLM/工具/技能/平台消息/**插件实例（`PluginInstance` trait、`InstanceManagement`）**/媒体/会话事件/联邦节点；零实现、零 harness 依赖 |
| `echo-context` | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry` |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集（含模型不可见的推理事件）、`EventLog` append-only 持久化、`derive_messages` 投影、**按会话切片投影**（append 的重投影代价只与本次会话相关）、compaction、`SessionHeader`、v1-v4 兼容迁移 |
| `echo-loop` | Agent 循环驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道；循环模式（单会话串行 / 并行多会话）见 [Agent 循环](../core/core-agent-loop.md) |
| `echo-llm-*` | LLM provider | OpenAI/Anthropic/Ollama 实现。`openai`/`anthropic` 只依赖 `echo-defs`；`ollama` 是薄包装，**复用 `echo-llm-openai`**（组内一条边） |
| `echo-protocol` | 线契约 | `BackendCommand`/`BackendEvent`/bridge（事件帧带扇出序号，供消费方校验事件流连续性）。Panel 的 Rust 中继对它**零依赖**（不解析负载、按原文转发），TS 前端以 `web/src/protocol.ts` 手写镜像 |
| `echo-plugin-provider-llm` | 内置插件：LLM provider（Phase 3 ①） | provider 工厂（OpenAI 兼容 / Anthropic Messages / Ollama，含 DeepSeek `/anthropic` 自动路由与 Kimi base_url 归一）+ 计量装饰器（`UsageRecorder` 出口）+ 脱敏装饰器；只依赖 `echo-defs` 与三个 provider crate |
| `echo-plugin-skills` | 内置插件：技能目录（Phase 3 ③） | `SkillRegistry`（递归发现 + 多目录分层合并 + 热重载状态）、SKILL.md frontmatter 解析、Git 来源安装与 `.sources.json`；插件 id 归一走 `echo_defs::plugin_id` |
| `echo-plugin-subagent` | 内置插件：Subagent 委派（Phase 3 ⑥） | `spawn_subagent` 工具（异步受理 + 立即回执）、`SubagentStore`（取消句柄 + TTL 清扫）、`run_subagent_turn`（隔离上下文的一次性 turn，剥离 spawn 能力防递归）；远程委派经注入的 `RemoteDelegationSink` 上报，插件不依赖联邦实现 |
| `echo-plugin-loop` | 内置插件：循环模式（Phase 3 ⑦） | `loop.single` / `loop.parallel` 两个纯策略插件的 id / 描述 / manifest 唯一事实来源，`Service{key:"loop"}` 贡献声明；mount 无运行期资源 |
| `echo-plugin-workspace` | 内置插件：工作区会话（Phase 3 ⑤） | `WorkspaceStore`（每 persona 一份 JSON，原子写）、`workspace` 工具（list/status/use/create/delete）、只读 git 与文件采集、浏览沙箱解析；跨机查询经注入的 `RemoteWorkspaceQuery`，插件不依赖联邦实现 |
| `echo-plugin-management` | 内置插件：管理面 WS（Phase 3 ⑧） | Panel 连接中继（`EventBroker` 单接收者 → 每连接一订阅、按序分配 `seq`）、token 鉴权与常量时间比较、端口合并规划 `plan_federation_listen` + fail-closed `validate_merged`、心跳保活；**不需要 Agent 句柄**，只认线契约 |
| `echo-plugin-tools` | 内置插件：内置工具集（Phase 3 之后的延续工作） | `echo-agent.tools.builtin` 的实现：计算/检索（`calculator`/`web_search`）、清单（`checklist`）、插件实例管理（`instance_*`）、编码（`read_file`/`write_file`/`edit_file`/`list_files`/`search_code`/`run_command`）、Shell（`shell_*`）。三个外部能力**经注入**：`RemoteInvoker`（`node://` 远程调用）、`echo_shell`（会话管理器）、工作区根——未注入时显式降级（「联邦未接线」/「manager unavailable」），不静默换实现 |
| `echo-shell` | Shell 会话机制 | 每会话一个 `bash --noprofile --norc`（独立进程组，销毁整组终止）、写命令 + 随机哨兵双写分帧（stdout/stderr 两路专职读取，避免假超时）、会话串行/跨会话并行、`MAX_SHELL_SESSIONS` 上限、事件经注入 emit 广播；宿主侧接线（`shell_emit_for_self` 等要拿 `Agent` 的部分）留在 `echo-agent::shell` |
| `echo-tool` | 工具子系统机制 | `ToolRegistry`（可逆注册 + 定义缓存 + 出口脱敏）；`Tool` 词汇在 `echo-defs`，具体工具实现不在此 |
| `echo-plugin` / `-api` / `-loader` / `-sdk` / `-host` | 插件契约、协议与宿主 | 生命周期钩子（`Plugin`/`PluginManifest`/`PluginRegistry`）；宿主↔插件消息（握手/调用/取消/事件/排空）、贡献类型与版本协商；插件装载与 SDK；Transport（inproc/stdio）、监督与崩溃重启、子进程限额与入站频率限额、可逆停止、conformance 测试（见 [完全解耦推进计划](../design/decoupling-plan.md)）。**`echo-plugin-host` 只依赖 `echo-plugin-api` + `echo-plugin-loader`——不认识 `echo-defs`、不认识任何领域词汇**，因此可作领域中立的插件机制复用 |
| `echo-plugin-adapter` | 远程贡献 → 领域 trait 的适配层 | `RemoteTool`（`ToolContribution` → `echo_defs::tool::Tool`）、`RemoteInstance`/`RemoteInstanceManagement`（`InstanceContribution` → `PluginInstance`/`InstanceManagement`，能力位 fail-closed，动作经 `Invoke("<name>:<op>")`）、`CoreApiService`（插件 → 宿主回呼）；依赖 host + api + defs，**不依赖 `echo-agent` 与组合根**。单独成 crate 的理由：这一层认识领域 trait，而 host 必须保持领域中立 |
| `echo-agent` | agent 框架 | 循环（内置实现；loop.* 插件启用时 echo-loop 驱动接管普通输入——生效口径见 [Agent 循环](../core/core-agent-loop.md)）、provider 工厂与装配、命令分发、timeline 投影、trunk。**具体实现多已外迁**：内置工具在 `echo-plugin-tools`（`packages/tools_builtin` 只留 `register_all` 兼容面与 `api_profiles`）、Shell 机制在 `echo-shell`（`shell.rs` 只留宿主侧接线）、技能在 `echo-plugin-skills`、子任务在 `echo-plugin-subagent`、工作区在 `echo-plugin-workspace` |
| `echo-defs`（`plugin_instance`） | 插件实例的**定义层** | `PluginInstance` trait（`tool_specs() -> Vec<ToolSpec>`）/ `InstanceManagement` / `ToolSpec` / `InstanceRegistry` / 状态与错误类型。**平台中立**：这里没有平台名、没有部署形状。QQ 实现是**独立进程插件**（独立仓库，stdio 协议；宿主经 `RemoteInstance` 接入——见 [实例插件化](../design/instance-plugin.md)） |
| `echo-config` | TOML 原子读改写 | `ConfigStore` / `ensure_table`（串行化 + tmp+rename）。**不知道任何 section 的含义**——调用方各自拥有自己的段 |
| `echo-federation` | 联邦链路（Core↔Core） | `FedFrame` 线协议、Hello/Welcome 握手、per-peer 认证、心跳/重连/回环防护、邀请串；详见 [联邦](../federation/federation.md) |
| `echo-agent-core`（bin） | 组合根 | 配置加载、Ctx 装配、`ctx.llm`/`ctx.loop` 注册、联邦路由泵、启动 |

依赖方向收敛为单向下游：**两个互不依赖的底座**——`echo-defs`（零 harness 依赖）与 `echo-context`（零 echo-* 依赖，**不依赖 `echo-defs`**）；`echo-protocol` / `echo-config` / `echo-session` / `echo-tool` / `echo-shell` / `echo-llm-*` / `echo-federation` / `echo-plugin*`（含七个 Phase 3 外迁插件：provider-llm / skills / subagent / workspace / management / loop / tools）/ `echo-sanitize`（中间层，只依赖底座）◄ `echo-agent` ◄ `echo-agent-core`（bin）。**PluginInstance trait 定义在 `echo-defs`**（`echo_defs::plugin_instance::PluginInstance`；`InstanceError`/`InstanceInfo`/`InstanceConnectionState`/`InstanceManagement` 同）；**协议侧贡献类型 `ToolContribution`/`InstanceContribution` 在 `echo-plugin-api`**，两者由 `echo-plugin-adapter` 桥接。`echo-config` 只做 TOML 读改写、不认识任何配置段的语义（旧 `echo-adapter` crate 已于 2026-10 删除，其注册表并入 `echo-defs::plugin_instance`）。**入站过滤与平台细节都在实例插件进程内**，宿主侧不持有过滤管道、不认识平台名。**依赖白名单的单一真值是 `scripts/deps-policy.txt`**（`scripts/check-deps.sh` 与 `source/core/tests/plugin_dependency_direction.rs` 双门禁共读，新增 crate 未登记即 fail-closed）；扩展插件只依赖定义层。旧 crate（`echo-agent`/`echo-protocol`）re-export `echo_defs` 类型，保持 `echo_agent::…` 等路径兼容。

### 定义层（echo-defs）

定义层持有全部词汇类型与 trait，**零实现、零 harness 依赖**（依赖仅 serde/serde_json/async-trait/tokio/thiserror/base64/tracing 基础 crate，不含 reqwest 与其它 echo-* crate），可独立测试。模块覆盖：消息/LLM（`ChatMessage`/`ChatRequest`/`LlmProvider` 等）、工具、技能、聊天（`ChannelType`/`IncomingMessage`/`MessageTarget` 等跨边界消息词汇）、插件实例（`instance` 模块：`PluginInstance` trait、`InstanceManagement`、`InstanceInfo`/`InstanceError`）、门控与思考模式、token 纯函数、媒体卫生（文本内嵌 base64 → 占位符、图片只走独立 image 块）与入站图片落盘缓存（`/media/<id>` 引用）、会话事件契约、联邦节点词汇（`NodeId`、`node://` 跨机命名空间）。

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
| 工具 | `echo_defs::Tool` | `echo-tool::ToolRegistry` + 内置工具插件 `echo-plugin-tools` + 外部插件贡献经 `echo-plugin-adapter::RemoteTool` | agent 循环 |
| 技能 | `echo_defs::SkillProvider` | `echo-plugin-skills` 的 `SkillRegistry`（本地文件 + 多目录分层合并；`echo-agent` 侧只留 re-export 兼容面） | prompt 组装 |
| 插件实例模型工具 | `echo_defs::plugin_instance::ToolSpec`（`PluginInstance::tool_specs`）；wire 侧对应 `echo_plugin_api::ToolContribution` | 实例插件贡献（`InstanceContribution.tools`） | 组合根通用桥（合并 / `account` 寻址 / 出站闸门 / `invoke_custom` 透传） |
| 平台生命周期 | `echo_defs::plugin_instance::PluginInstance` | 实例插件（**进程外**，宿主经 `echo-plugin-adapter::RemoteInstance` 接入——当前唯一样本是 `echo-agent.instance.qq`，实现在独立仓库） | agent + 实例工具 |
| 默认驱动 | `echo_loop::TurnRunner` | 内置（经 `ctx.loop` 注册） | 组合根 |

## 运行时一览

内核之上的运行时子系统综述——每节只留要点，细节见对应子系统文档。

### 事件模型

事件分四类：**生命周期事件**（echo-loop 经 EventBus 分发——`TurnStart`/`AgentPreStep`/`StepStart`/`AgentRequest`/`ToolCallRequested`/`ToolResult`/`StepEnd`/`TurnStopping`/`TurnEnd`）；**会话事件**（echo-session append-only 日志——`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Reasoning`/`ModelStep`/`Compaction`；模型上下文由 `derive_messages` 投影，跳过 `Reasoning` 与 `ModelStep`（两者只服务显示/观测），加载时 `repair_tool_pairing` 修复悬空调用）；**协议事件**（echo-protocol 的 `BackendEvent`，经 management WS 供 Panel 消费，见 [协议与数据流](../protocol/protocol.md)）；**能力事件**（timeline 投影作为 EventBus observe 监听器——`Agent` 不直接写 timeline，显示历史是事件的投影消费者，可独立演进/测试）。

### Turn/Step 循环

`TurnRunner` 驱动 `turn/start → agent/pre-step → step/start → agent/request → llm.chat → tool/call → pipeline(pre/execute/post) → step/end →（循环）→ turn-stopping → turn/end`。调度口径：这是**唯一**循环实现（单轨化），全部输入（含 QQ hook/定时器/QQ 会话，差异只在提示词边界块）经该状态机执行，runner 在人格启动与运行期新建时无条件注入；loop.* 插件只决定循环模式；工具策略（超时/审批/审计/限流）的设计位置是 `ToolPipeline` 中间件（当前生产路径未注册任何中间件，超时由 harness `tool_guard_timeout` 守卫）。细节与边界见 [Agent 循环](../core/core-agent-loop.md)。

### 会话与持久化

事件溯源：`EventLog` 是权威（append-only，整档 JSON v6，tmp+rename 原子写）；`SessionEvent` 七类（`UserMessage`/`AssistantMessage`/`ToolCall`/`ToolResult`/`Reasoning`/`ModelStep`/`Compaction`），压缩落地为显式 `Compaction` 事件（被替换条数 + 摘要 + 可选归档快照），日志保持可重放。投影：`derive_messages` 是唯一的模型上下文出口——多会话按 `session` 归属逐会话投影，裁剪只在投影期、绝不破坏日志；追加按会话切片重投影，代价只与本次会话长度相关，不随会话总数增长。推理事件只服务显示：投影跳过它，显示水位线也把它排除在过滤之外（否则思考条目会被当成"已覆盖"而消失）。`ModelStep` 记一次模型往返（step_index / 模型 / token / 请求→响应耗时 / 本步工具调用 id），是"并行度"审计的唯一事实来源——此前只能靠相邻 `ToolCall` 时间戳猜同轮打包，会低估并行、也看不到被取消而未执行的调用；它同样不进投影、不进显示时间线、不进压缩转录。显示时间线带两个序号：`seq`（增量同步用，就地更新会推进它，故非单调）与 `ord`（push 时分配、此后永不改变，作**分页锚点**——分页若复用 `seq`，运行中工具条目就地更新会造成乱序并漏条目）。归档与保留：手动 `ArchiveHistory`（支持 `session_id` 会话级；数据为内存快照）与压缩前自动快照写入 `archives/`（v6 完整快照，保留最新 20 份）；v1-v4 旧格式加载期自动迁移，每个 persona 独立会话文件。细节见 [会话记忆](../core/core-memory.md)（日志/投影/压缩）与 [配置持久化](../core/core-config-persistence.md)（文件布局）。

### 媒体库

入站图片（QQ 消息图、面板上传图）**落盘为媒体文件，链路上只传引用**：`~/.local/share/echo-agent-core/media/`（`$ECHO_MEDIA_DIR` 可覆盖），引用为 `/media/<id>`（Panel 后端同源提供，浏览器懒加载 + 强缓存）。落盘侧：QQ 插件实例在**触发门控与过滤通过后**才下载远端图（被丢弃的消息不白存图），SendMessage（面板上传 data URI）入站落盘；遗留内嵌图由加载期迁移一次性改写（幂等）。模型侧：投影出口把引用还原为 data URI——发往 LLM 的请求与内嵌时代无差别。细节见 [多模态输入](../core/core-multimodal.md)。

### 命令分发

实例命令域独立：`apply_instance_command`（`packages/instance_ops/commands.rs`）承载 `InvokeInstanceOp` 及其状态查询等通用插件实例命令，主 `apply_command` 的对应分支为一行委托，核心分支原地保留——平台专属动作（门控/名单/登录/owner）不再有具体变体，一律经 `InvokeInstanceOp{instance, method, payload}` 透传给插件。`CommandRegistry` + `CommandHandler` trait（dyn-compatible，async）是开放命令扩展点的基础设施：封闭枚举 `BackendCommand` 下编译器 match 完备性仍由主分发器承担，注册表留待协议开放后承载外部命令插件。

### 结构化输入标记

结构化输入标记的单一事实来源是 echo-agent 的 `input_marker.rs`：构造（`wrap_hook`/`wrap_timer`）、判定、解析（`structured_message_sequence`，限制在标记开头、防普通文本误读）集中一处，改格式只改一处。**判定不依赖平台名**——`platform_hook_of` 按位置匹配任意 `<X_message_hook>`（`X` 限字母/数字/`_`/`-`）并返回平台名；`FIXED_INPUT_MARKERS` 只列宿主内部的两类（`<backend_message_hook>` / `<timer_event>`，并优先排除，避免被误判成平台 `backend`）。新增平台因此不需要改这个模块。演进方向：来源判定从 content 字符串改为 `MessageReceived` 的结构化 origin 字段（跨仓库 wire 变更）。

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`，注册 `ctx.llm` |
| 新增工具 | 内置：实现 `echo_defs::Tool` 并注册进 `ToolRegistry`（现役实现在 `echo-plugin-tools`）；外部：写进程插件贡献 `ToolContribution`，经 `echo-plugin-adapter` 适配（MCP 走桥接插件，见 [MCP 导入通道评估](../design/mcp-import.md)） |
| 新增平台 | 写一个**实例插件**（`InstancePlugin` + `InstanceContribution`：能力位 / UI 描述符 / 会话标签 / **模型工具声明**），在 `[instances.<platform>]` 配好 `enabled` 与 `plugin.command`。宿主侧无需改代码——配置、门控、工具桥、提示词边界都是平台中立的（见 [插件开发指南](../plugins/plugin-authoring.md)「实例插件」） |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略 | 超时已由 harness `tool_guard_timeout` 守卫（`agent/tool_exec.rs`）；`ToolPipeline` 中间件机制已备、生产路径未接线 |
| 新增命令 | 命令分发按域拆分（`apply_instance_command` 模式）+ CommandRegistry（开放后）；平台专属动作优先走 `InvokeInstanceOp` 方法名而非新增变体 |
| 会话状态扩展 | 扩展 `SessionEvent` 枚举（echo-session），从日志渲染 |

## 关键文档

- [文档总览](../index.md)：文档群入口与阅读路径；[开发指南](../dev/dev-guide.md)：仓库结构、关键抽象、构建与测试、治理门禁
- [Core 后端](../core/core.md) → [框架（内核）](../core/frame.md) / [插件化设计](../plugins/core-plugins.md)：框架与插件两支主线
- 运行时子系统：[多 Agent 与会话](../core/core-agents.md)、[Agent 循环](../core/core-agent-loop.md)、[会话记忆](../core/core-memory.md)、[多模态输入](../core/core-multimodal.md)、[配置持久化](../core/core-config-persistence.md)
- 数据流与部署：[协议与数据流](../protocol/protocol.md)、[部署与自更新](../ops/ops-deploy.md)
