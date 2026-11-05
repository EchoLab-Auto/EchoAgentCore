---
id: architecture
title: "架构总览"
group: 总览
x: 160
y: 360
---

# 架构总览

> **定位**：本文是 EchoAgent 的**架构脊柱**——设计原则、crate 布局与依赖方向、能力接缝、运行时一览、扩展点地图。读者：需要理解系统骨架、准备修改 `source/` 的开发者（治理门禁见 [开发指南](../dev/guide.md)）。
> 阅读入口：[Core 后端](../core/core.md)；内核机制见 [框架（内核）](../core/frame.md) 与 [模块化设计](../modules/modules.md)；运行时子系统见 [多 Agent 与会话](../core/agents.md)、[Agent 循环](../core/agent-loop.md) 等（各见正文链接）。

## 设计原则（dsh 模式）

本仓库按 DeepSeek Harness 的设计模式组织，五条核心原则：

1. **一切皆模块、无特权核心**：agent 循环、模型模块实例、工具、技能、平台模块实例都可挂载/替换；注册是可逆副作用（返回 disposer）。
2. **服务定位 + 依赖注入**：服务以稳定键登记在 `Ctx`（`ctx.llm` / `ctx.loop` / `ctx.sanitizer`）供**可发现性与诊断**；实际消费走显式参数（`AgentBuilder` 收 provider、`set_loop_runner` 收 runner）。**挂载顺序由 `modules.toml` 的挂载计划决定，不由服务可用性驱动**；扩展模块只依赖定义层。
3. **类型化事件 = 扩展点**：`EventBus` 的 Observe/Waterfall/Parallel/Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）。
4. **能力接缝**：Service Definition / Service Provider / Consumer 三角独立演进、独立成 crate；依赖方向单向无环。
5. **事件溯源会话日志**：日志是唯一事实来源，模型上下文由日志投影——**"模型可见 ⇒ 已记录"**（模型上下文里的每条内容都能在日志中找到出处）；反向不成立：推理（thinking）正文同样入日志以保住显示时序，但投影时跳过、永不进入模型请求。compaction 是显式事件。

## Crate 布局与依赖方向

```prodoc-flow
graph BT
  Defs[echo-defs<br>Service Definition<br>（ModuleInstance trait / InstanceManagement 在此）]
  Ctx[echo-context<br>Ctx/EventBus/Disposer]
  Session[echo-session<br>事件溯源]
  Loop[echo-loop<br>TurnRunner]
  Proto[echo-protocol<br>线契约]
  Config[echo-config<br>TOML 原子读改写]
  Fed[echo-federation<br>Core↔Core 联邦]
  Module[echo-module<br>生命周期钩子]
  ModuleApi[echo-module-api<br>协议与贡献类型]
  ModuleLoader[echo-module-loader]
  ModuleSdk[echo-module-sdk<br>模块 SDK]
  ModuleHost[echo-module-host<br>运输/监督（领域中立）]
  ModuleAdapter[echo-module-adapter<br>贡献→领域 trait]
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
  Module --> Defs
  Module --> Ctx
  ModuleSdk --> ModuleApi
  ModuleHost --> ModuleApi
  ModuleHost --> ModuleLoader
  ModuleAdapter --> ModuleHost
  ModuleAdapter --> ModuleApi
  ModuleAdapter --> Defs
  Agent --> Session
  Agent --> LLM
  Agent --> Loop
  Agent --> Proto
  Agent --> Module
  Agent --> Fed
  Agent --> Ctx
  Agent --> Defs
  Bin --> Agent
  Bin --> Loop
  Bin --> ModuleHost
  Bin --> ModuleAdapter
  Bin --> Fed
```

> 依赖边以 `cargo metadata --format-version 1 --no-deps`（仅 normal 依赖）实测为准。`echo-context` **不依赖** `echo-defs`（零 echo-* 依赖）；组合根另直接依赖全部中间层 crate 与全部内置模块 crate，图中只画主干边（内置模块各自只依赖定义层/机制层，边略）。**依赖白名单的单一真值是 `scripts/deps-policy.txt`**——`scripts/check-deps.sh`（CI `deps-lint`）与 `source/modules/core/tests/module_dependency_direction.rs`（cargo test）共读同一份，新增 crate 未登记即失败。

> **归属（框架外迁）**：下表里带「框架仓」标记的 crate **不在本仓**，
> 而是独立仓库 [echo-fabric-arch](https://github.com/EchoLab-Auto/echo-fabric-arch)
> （`v0.11.1`），由本仓的 `Cargo.toml` 以 `git + tag` 引用。它们与任何应用 crate
> 无反向依赖，可被其它 Rust 应用单独使用。本仓 `source/modules/`
> 下只剩**框架的使用者**（内置模块）与领域适配层。

| Crate | 角色 | 职责 |
|---|---|---|
| `echo-defs` | Service Definition 层 | 全部词汇与 trait：LLM / 工具 / 技能 / 平台消息 / **模块实例** / 媒体 / 会话事件 / 联邦节点；零实现、零 harness 依赖。模块实例模块（`module_instance`）持有 `ModuleInstance` trait（`tool_specs() -> Vec<ToolSpec>`）/ `InstanceManagement` / `ToolSpec` / `InstanceRegistry` / 状态与错误类型，**平台中立**：没有平台名、没有部署形状——QQ 实现是独立进程模块（独立仓库，stdio 协议，宿主经 `RemoteInstance` 接入） |
| `echo-context` **（框架仓）** | 机制层 | `Ctx` 服务定位、`EventBus` 类型化事件、`Disposer` 可逆注册、`ScopedRegistry`。**零全局态**：不持有进程级单例表——需要共享状态的调用方经 `Ctx` 显式注入 |
| `echo-session` | 事件溯源会话 | `SessionEvent` 事件集（含模型不可见的推理事件）、`EventLog` append-only 持久化、`derive_messages` 投影、**按会话切片投影**（append 的重投影代价只与本次会话相关）、compaction、`SessionHeader`、v1-v4 兼容迁移。**词汇层**：只导出类型与纯函数，实例与生命周期在 `echo-module-session` |
| `echo-loop` | Agent 循环驱动 | `TurnRunner` turn/step 状态机、`ToolPipeline` 工具执行管道；会话内 FIFO 串行准入（每会话一条时间线）见 [Agent 循环](../core/agent-loop.md) |
| `echo-protocol` | 线契约 | `BackendCommand`/`BackendEvent`/bridge（事件帧带扇出序号，供消费方校验事件流连续性）。另含显示文本工具 `timeline_text`（定时信封解析 / 单行折叠 / 工具概要截断，供实时投影与派生两条路径共用）。Panel 的 Rust 中继对它**零依赖**（不解析负载、按原文转发），TS 前端以 `web/src/protocol.ts` 手写镜像 |
| `echo-module-provider-llm` | 内置模块：LLM provider（Phase 3 ①） | provider 工厂（OpenAI 兼容 / Anthropic Messages / Ollama，含 DeepSeek `/anthropic` 自动路由、Kimi base_url 归一、OpenCode Go/Zen 会话头注入）+ 计量装饰器（`UsageRecorder` 出口）+ 脱敏装饰器；**API 设置面策略**：池规则（`admin`）、13 条命令域（`commands`）、探针配置解析 / `core.toml` agent 表补丁 / `ApiConfigUpdated` 事件装配 / 「测试 API」全流程（`api_admin`）；只依赖 `echo-defs` 与三个 provider crate |
| `echo-module-skills` | 内置模块：技能目录（Phase 3 ③） | `SkillRegistry`（递归发现 + 多目录分层合并 + 热重载状态）、SKILL.md frontmatter 解析、Git 来源安装与 `.sources.json`；模块 id 归一走 `echo_defs::module_id` |
| `echo-module-subagent` | 内置模块：Subagent 委派（Phase 3 ⑥） | `spawn_subagent` 工具（异步受理 + 立即回执）、`SubagentStore`（取消句柄 + TTL 清扫）、`run_subagent_turn`（隔离上下文的一次性 turn，剥离 spawn 能力防递归）；远程委派经注入的 `RemoteDelegationSink` 上报，模块不依赖联邦实现 |
| `echo-module-loop` | 内置模块：循环（Phase 3 ⑦） | 唯一循环模块 `loop.single` 的 id / 描述 / manifest 唯一事实来源，`Service{key:"loop"}` 贡献声明；mount 时构造唯一 `TurnRunner` 并 `provide` 该服务（卸载撤销），策略只有"同一会话 turn 串行排队" |
| `echo-module-workspace` | **独立仓**模块：工作区会话（2026-11 迁出 Core，git 依赖引入） | `WorkspaceStore`（每 persona 一份 JSON，原子写）、`workspace` 工具、只读 git 与文件采集、浏览沙箱解析；宿主只留**只读镜像**（`WorkspaceMirror`）+ 工具代理，模块经 `workspace.changed` 等三条模块事件驱动宿主的会话通道；跨机目录降级为占位（模块不依赖联邦实现） |
| `echo-module-session` | 内置模块：会话子系统（解耦计划 S6） | `TrunkStore` 的实现归属：append-only 事件日志、按会话上下文投影、持久化（tmp+rename 原子写、`save_lock` 串行）、归档轮转（`archives/` 保留 20 份）、压缩编排（预览→归档→摘要→落地，全程 `compact_lock`）、按需派生显示历史、**写命令域**（`ClearHistory` / `ArchiveHistory` / `CompactHistory`，2026-11 S6 补口自宿主迁入）。mount 只 provide 工厂 `echo-agent.session.factory`（每人格一份 trunk，不是单例）；**不依赖任何模块**，只认 `echo-defs`/`echo-protocol`/`echo-session` 词汇 + 模块机制 |
| `echo-module-transcript` | 内置模块：显示层（解耦计划 S8） | 时间线实时投影（`timeline`：哪些事件进面板、字段怎么映射、工具完成态原地合并）+ 会话视图命令域（`session_view`：5 条 `Request*`）。2026-11 自 `echo-module-management` 外置：连接面（WS 中继）与显示面（"面板看到什么"）分轴；与 `echo-module-session` 的历史派生共用 `echo-protocol::timeline_text`。只依赖词汇层与机制层 |
| `echo-module-management` | 内置模块：管理面 WS（Phase 3 ⑧） | Panel 连接中继（`EventBroker` 单接收者 → 每连接一订阅、按序分配 `seq`）、token 鉴权与常量时间比较、端口合并规划 `plan_federation_listen` + fail-closed `validate_merged`、心跳保活；**不需要 Agent 句柄**，只认线契约。显示层已外置到 `echo-module-transcript` |
| `echo-module-tools` | 内置模块：内置工具集（Phase 3 之后的延续工作） | `echo-agent.tools.builtin` 的实现：计算/检索（`calculator`/`web_search`）、清单（`checklist`）、模块实例管理（`instance_*`）、编码（`read_file`/`write_file`/`edit_file`/`list_files`/`search_code`/`bash`）。两个外部能力**经注入**：`RemoteInvoker`（`node://` 远程调用）、工作区根——未注入时显式降级（「联邦未接线」），不静默换实现 |
| `echo-module-shell` | 内置模块：Shell 会话 | `echo-agent.shell` 的实现：`bash-start` / `bash-exec` / `bash-stop` 三个工具 + 四条 `*Shell*` 面板命令。机制依赖 `echo-shell`（进程组生命周期 + 哨兵分帧），与工具集**互不依赖**；拆出的理由是独立的启停需求与独立的机制依赖——想关 shell 不该连计算器一起关 |
| `echo-module-federation` | 内置模块：联邦执行层（`echo-agent.federation`） | 调用调度路由表（`call_id → oneshot`）、`RemoteTool` 代理工具（`<peer>__<tool>`）、白名单裁决与执行端、工作区跨机查询、联邦命令域。只依赖 `echo-federation` / `echo-tool` / `echo-defs` 等机制层，**可独立禁用**：关掉后内核照常启动，远程工具显式降级为「联邦未接线」；链路泵（握手 / 心跳 / 重连 / 回环防护）仍在组合根 |
| `echo-module-sanitize` | 内置模块：脱敏（`echo-agent.security.sanitize`，`kind = security` → **进程级**） | 脱敏的**实现 + 装配**：宿主收集秘密表（`[agent]` / profiles / env / 实例凭据 / `extra_*`），模块用 `build(&RedactorSpec)` 构造 `RegistryRedactor`——组合根不再认识任何脱敏实现。唯一的特殊性：构造必须在**挂载之前**（脱敏是 LLM 出口的装饰器，`llm` 服务要以已脱敏形态进 `Ctx`），所以 `mount` 是纯身份声明。契约见 [脱敏模块](../modules/sanitize.md) |
| `echo-module-self-update` | 内置模块：自更新（`echo-agent.selfupdate`） | `RequestSelfUpdate` / `RequestSelfUpdateStatus` 两条命令的归属模块，连同 updater 单元名、状态文件布局与重入判定；零参数（宿主能力经服务表解析），只经事件出口说话。做成内置模块而非「宿主自我管理面」的理由：命令认领不该由一张保留名单守卫，而「第三方不得劫持自更新」用编译进本二进制 + 启动即挂载满足更准确 |
| `echo-shell` | Shell 会话机制 | 每会话一个 `bash --noprofile --norc`（独立进程组，销毁整组终止）、写命令 + 随机哨兵双写分帧（stdout/stderr 两路专职读取，避免假超时）、会话串行/跨会话并行、`MAX_SHELL_SESSIONS` 上限、事件经注入 emit 广播；宿主侧接线（`shell_emit_for_self` 等要拿 `Agent` 的部分）留在 `echo-agent::shell` |
| `echo-tool` | 工具子系统机制 | `ToolRegistry`（可逆注册 + 定义缓存 + 出口脱敏）；`Tool` 词汇在 `echo-defs`，具体工具实现不在此 |
| `echo-module` / `-api` / `-loader` / `-sdk` / `-host` **（框架仓）** | 模块契约、协议与宿主 | 生命周期钩子（`Module`/`ModuleManifest`/`ModuleRegistry`）；宿主↔模块消息（握手/调用/取消/事件/排空）、贡献类型与版本协商；模块装载与 SDK；Transport（inproc/stdio）、监督与崩溃重启、子进程限额与入站频率限额、可逆停止、conformance 测试。**`echo-module-host` 只依赖 `echo-module-api` + `echo-module-loader`——不认识 `echo-defs`、不认识任何领域词汇**，因此可作领域中立的模块机制复用 |
| `echo-module-adapter` | 远程贡献 → 领域 trait 的适配层 | `RemoteTool`（`ToolContribution` → `echo_defs::tool::Tool`）、`RemoteInstance`/`RemoteInstanceManagement`（`InstanceContribution` → `ModuleInstance`/`InstanceManagement`，能力位 fail-closed，动作经 `Invoke("<name>:<op>")`）、`CoreApiService`（模块 → 宿主回呼）；依赖 host + api + defs，**不依赖 `echo-agent` 与组合根**。单独成 crate 的理由：这一层认识领域 trait，而 host 必须保持领域中立 |
| `echo-agent` | agent 框架 | 命令分发、timeline 投影、trunk、provider 工厂与装配、提示词边界与装配、多 agent 管理。**循环只有 echo-loop 一条轨道**（内置循环已删除；`echo-agent.loop.single` 是进程级模块，只声明"同一会话 turn 串行排队"这一条策略——见 [Agent 循环](../core/agent-loop.md)）。**具体实现多已外迁**：内置工具在 `echo-module-tools`、Shell 机制在 `echo-shell`（`shell.rs` 只留宿主侧接线）、技能在 `echo-module-skills`、子任务在 `echo-module-subagent`、工作区在 `echo-module-workspace`、联邦执行层在 `echo-module-federation`；`packages/` 只剩三个内联包（`tools_builtin` 端口接线 / `instance_ops` 入站桥 / `federation` 兼容面），宿主侧**不认识任何一个内置工具的名字** |
| `echo-config` | TOML 原子读改写 | `ConfigStore` / `ensure_table`（串行化 + tmp+rename）。**不知道任何 section 的含义**——调用方各自拥有自己的段 |
| `echo-federation` | 联邦链路（Core↔Core） | `FedFrame` 线协议、Hello/Welcome 握手、per-peer 认证、心跳/重连/回环防护、邀请串；详见 [联邦](../federation/federation.md) |
| `echo-agent-core`（bin） | 组合根 | 配置加载、Ctx 装配、`ctx.llm` 注册、模块宿主装配与挂载、挂载后解析 `loop` 服务、联邦链路泵、启动 |

> 另有 `echo-test-utils`（跨 crate 复用的测试夹具）这个 dev crate，不进运行时依赖链。

依赖方向收敛为单向下游：**两个互不依赖的底座**——`echo-defs`（零 harness 依赖）与 `echo-context`（零 echo-* 依赖，**不依赖 `echo-defs`**）；其上为中间层（`echo-protocol` / `echo-config` / `echo-session` / `echo-tool` / `echo-shell` / `echo-federation`）与模块层（`echo-module*` 与全部内置模块 crate）◄ `echo-agent` ◄ `echo-agent-core`（bin）。三条容易记错的归属：

- **`ModuleInstance` trait 在 `echo-defs`**（`echo_defs::module_instance`；`InstanceError` / `InstanceInfo` / `InstanceConnectionState` / `InstanceManagement` 同）；**协议侧贡献类型 `ToolContribution` / `InstanceContribution` 在 `echo-module-api`**，两者由 `echo-module-adapter` 桥接
- **`echo-config` 只做 TOML 原子读改写**，不认识任何配置段的语义（旧 `echo-adapter` crate 已删除，其注册表并入 `echo-defs::module_instance`）
- **入站过滤与平台细节都在实例模块进程内**：宿主侧不持有过滤管道、不认识平台名

**依赖白名单的单一真值是 `scripts/deps-policy.txt`**（`scripts/check-deps.sh` 与 `source/modules/core/tests/module_dependency_direction.rs` 双门禁共读，新增 crate 未登记即 fail-closed）；扩展模块只依赖定义层。旧 crate（`echo-agent` / `echo-protocol`）re-export `echo_defs` 类型，保持 `echo_agent::…` 等路径兼容。

### 定义层（echo-defs）

定义层持有全部词汇类型与 trait，**零实现、零 harness 依赖**（依赖仅 serde/serde_json/async-trait/tokio/thiserror/base64/tracing 基础 crate，不含 reqwest 与其它 echo-* crate），可独立测试。模块覆盖：消息/LLM（`ChatMessage`/`ChatRequest`/`LlmProvider` 等）、工具、技能、聊天（`ChannelType`/`IncomingMessage`/`MessageTarget` 等跨边界消息词汇）、模块实例（`instance` 模块：`ModuleInstance` trait、`InstanceManagement`、`InstanceInfo`/`InstanceError`）、门控与思考模式、token 纯函数、媒体卫生（文本内嵌 base64 → 占位符、图片只走独立 image 块）与入站图片落盘缓存（`/media/<id>` 引用）、会话事件契约、联邦节点词汇（`NodeId`、`node://` 跨机命名空间）。

### 机制底座（echo-context）

最底层机制 crate（仅依赖 tokio/dashmap，不依赖任何 echo-* crate）：

- **`Ctx` 服务定位**：按字符串 key 注册/解析；注册 `Arc<dyn Trait>`（trait 对象存入 `Box<dyn Any>`，resolve 时 downcast 回同一类型）或具体 `Clone + 'static` 值——字符串键便于配置与诊断，且允许同一 trait 多实例按名共存
- **`Disposer` 可逆注册**：`Ctx::register` / `EventBus::subscribe` / 注册表方法都返回 `Disposer`；注册必须持有 disposer 才生效（组合根持有 `ctx` 注册），装配方需明确管理生命周期
- **`EventBus` 类型化事件**：`Event` trait（Any + Clone + Debug + Send + Sync），按事件 `TypeId` 分组监听器；四种分发——`Observe`（扇出）/ `Waterfall`（around-middleware，不调 `next()` 即短路）/ `Parallel`（每监听器一份事件副本并发）/ `Serial`（按序；**当前实现与 `Observe` 等价**，「首个拒绝即停止」为预留语义）。`emit_sync` 覆盖同步热路径（Observe/Waterfall），`emit` 支持全部模式；`echo-protocol` 为 `BackendEvent` 实现 `Event`（依赖方向仍为 context ◄ protocol）
- **`ScopedRegistry` 作用域注册**：全局条目 + per-scope 条目，lookup 先 scoped 后 global（shadowing），scope 卸载时条目随 disposer 撤销

### LLM provider 实现（`echo-module-provider-llm::providers`）

三个协议的客户端（OpenAI 兼容 / Anthropic Messages / Ollama）是 `echo-module-provider-llm` 的**私有模块**，不再是独立 crate：

```text
echo-module-provider-llm/src/providers/
├── openai.rs       1181 行  OpenAI 兼容（含 DeepSeek / Moonshot / vLLM）
├── anthropic.rs    1458 行  Anthropic Messages
└── ollama.rs         82 行  Ollama（薄包装，复用 openai 指向 {base}/v1）
```

**为什么合并（2026-11）**：它们此前是 `source/llm/echo-llm-*` 三个 crate，被 `deps-policy.txt` 归入"中间层"。但中间层的定义是**被多方复用的底座**，而三者的消费者各只有一个（`echo-module-provider-llm`），仓外零引用。也就是说那三个 crate 边界换来的不是"可替换点"，只是目录层级——协议差异靠的是 `LlmProvider` trait（在 `echo-defs`），不是 crate 之间的抽象。

判据（下一个"该不该独立成 crate"的决定可用）：**消费者 = 1 → 是那个消费者的实现细节，合并；消费者 > 1 → 是共用底座，独立**（对照：`echo-tool` 被 7 个 crate 用，因此保留；反例是 `echo-sanitize`——它的生产消费者只有 `echo-module-sanitize` 一处，2026-11 已并入该模块）。

链路不变：`echo-agent::llm::create_provider`（配置投影 + 指标出口的宿主 glue）→ `echo-module-provider-llm::create_provider`（按 provider 名与 `base_url` 后缀路由到某个客户端的工厂）。新增 provider = 在 `providers/` 加一个文件 + 在工厂的 match 里加一条分支；跨仓库的外部 provider 仍可实现 `echo_defs::LlmProvider` 自行注册（演进方向：以 `ctx.llm` 注册表查找替代工厂字符串 match，支持运行期热替换）。

## 能力接缝

一个可替换能力 = 三角色。当前接缝：

| Seam | Service Definition | Provider | Consumer |
|---|---|---|---|
| LLM | `echo_defs::LlmProvider` | `echo-module-provider-llm`（内置 `providers::{openai,anthropic,ollama}`；外部 provider 自行实现 trait） | `echo-agent` 工厂 + `echo-loop` runner |
| 工具 | `echo_defs::Tool` | `echo-tool::ToolRegistry` + 内置工具模块 `echo-module-tools` + 外部模块贡献经 `echo-module-adapter::RemoteTool` | agent 循环 |
| 技能 | `echo_defs::skill::{Skill, SkillMetadata}`（词汇） | `echo-module-skills` 的 `SkillRegistry`（本地文件 + 多目录分层合并；`echo-agent` 侧只留 re-export 兼容面） | prompt 组装 |
| 模块实例模型工具 | `echo_defs::module_instance::ToolSpec`（`ModuleInstance::tool_specs`）；wire 侧对应 `echo_module_api::ToolContribution` | 实例模块贡献（`InstanceContribution.tools`） | `echo-module-tools::instance_bridge`（合并 / `account` 寻址 / 出站闸门 `gate_outbound` / `invoke_custom` 透传） |
| 平台生命周期 | `echo_defs::module_instance::ModuleInstance` | 实例模块（**进程外**，宿主经 `echo-module-adapter::RemoteInstance` 接入——当前唯一样本是 `echo-agent.instance.qq`，实现在独立仓库） | agent + 实例工具 |
| 默认驱动 | `echo_loop::TurnRunner` | `echo-module-loop`（mount 期构造并 `provide` `loop` 服务键） | 组合根挂载后解析 `loop`，再经 `Agent::set_loop_runner` 注入全体人格；**解析失败即启动失败**（没有驱动的 agent 只收消息不干活，静默比崩溃难查） |

## 运行时一览

内核之上的运行时子系统综述——每节只留要点与边界，机制细节见对应子系统文档。

### 事件模型

四类事件各有归属：**生命周期事件**（`echo-loop` 经 `EventBus` 分发，`TurnStart` … `TurnEnd`；清单与拦截点见 [Agent 循环](../core/agent-loop.md)）；**会话事件**（`echo-session` 的 append-only 日志，见下节）；**协议事件**（`echo-protocol` 的 `BackendEvent`，经 management WS 供 Panel 消费，见 [协议与数据流](../protocol/protocol.md)）；**能力事件**（timeline 投影是 `EventBus` 的 observe 监听器——`Agent` 不直接写 timeline，显示历史是事件的投影消费者，可独立演进与测试）。

### Turn/Step 循环

`TurnRunner` 驱动 `turn/start → agent/pre-step → step/start → agent/request → llm.chat → tool/call → pipeline(pre/execute/post) → step/end →（循环）→ turn-stopping → turn/end`。这是**唯一**循环实现（单轨化）：全部输入（含平台 hook / 定时器）经该状态机执行，差异只在提示词边界块；runner 在人格启动与运行期新建时无条件注入，同一会话的 turn 由 FIFO 串行闸门排队（唯一模式，没有模式可切）。工具策略（超时 / 审批 / 审计 / 限流）的设计位置是 `ToolPipeline` 中间件（当前生产路径未注册任何中间件，超时由 harness `tool_guard_timeout` 守卫）。细节与边界见 [Agent 循环](../core/agent-loop.md)。

### 会话与持久化

`EventLog` 是权威（append-only，整档 JSON v6，tmp+rename 原子写）；`SessionEvent` 七类（`UserMessage` / `AssistantMessage` / `ToolCall` / `ToolResult` / `Reasoning` / `ModelStep` / `Compaction`），压缩落地为显式 `Compaction` 事件，日志保持可重放。三条边界值得记住：

- **投影是唯一的模型上下文出口**：`derive_messages` 跳过 `Reasoning` 与 `ModelStep`（两者只服务显示与观测）；裁剪只在投影期，绝不破坏日志；追加按会话切片重投影，代价不随会话总数增长
- **显示时间线带两个序号**：`seq`（增量同步用，就地更新会推进它，故非单调）与 `ord`（push 时分配、此后永不改变，作**分页锚点**——复用 `seq` 会让运行中工具条目的就地更新造成乱序并漏条目）
- **归档与保留**：手动 `ArchiveHistory`（支持 `session_id` 会话级）与压缩前自动快照写入 `archives/`（完整快照，保留最新 20 份）

细节见 [会话记忆](../core/memory.md)（日志 / 投影 / 压缩）与 [配置持久化](../core/config-persistence.md)（文件布局）。

### 媒体库

入站图片（平台消息图、面板上传图）**落盘为媒体文件，链路上只传引用**：`~/.local/share/echo-agent-core/media/`（`$ECHO_MEDIA_DIR` 可覆盖），引用为 `/media/<id>`（Panel 后端同源提供，浏览器懒加载 + 强缓存）。发往 LLM 前在投影出口把引用还原为 data URI——模型侧与内嵌时代无差别。细节见 [多模态输入](../core/multimodal.md)。

### 命令分发

`BackendCommand` 是封闭枚举；**命令扩展只有一条路径**：写一个模块、用 `Contribution::Command` 声明它认领的命令名（工具域在 `echo-module-tools`、Shell 域在 `echo-module-shell`、模块实例域在 `echo-module-tools::instance_commands`、联邦域在 `echo-module-federation`、自更新域在 `echo-module-self-update`）。此前另有一套 `CommandRegistry` + `CommandHandler`（按注册顺序问「谁认领」），在模块命令域落地后已删除。宿主保留面（自管理 / 会话写 / turn 控制）仍在主分发器的 match 里加分支；平台专属动作走 `InvokeInstanceOp{instance, method, payload}` 透传给模块，**不新增变体**。

### 结构化输入标记

标记的单一事实来源是 `echo_defs::input_marker`（`echo-agent` 侧只留 re-export）：构造（`wrap_hook` / **`format_instance_hook`**——一条入站消息 → 完整 payload 信封）、判定、解析（`structured_message_sequence`，限制在标记开头、防普通文本误读）集中一处，改格式只改一处。**构造与解析必须同住一个模块**：`format_instance_hook` 写的 `"message_sequence"` 正是 `structured_message_sequence` 读的那个字段，两侧分居不同 crate 时靠字符串字面量对齐——改字段名不会有任何编译错误，只会静默丢序号。**判定不依赖平台名**——`platform_hook_of` 按位置匹配任意 `<X_message_hook>` 并返回平台名，`FIXED_INPUT_MARKERS` 只列宿主内部的两类并优先排除；新增平台因此不需要改这个模块。演进方向：来源判定从 content 字符串改为 `MessageReceived` 的结构化 origin 字段。

平台无关的**控制命令词汇**同样在定义层：`echo_defs::control_command::cancel_scope`
（`"取消当前任务"` → 只停最新一个 / `"全部停止"` → 全停 / 其余 → `None` 交给模型）。
判据是**用户打了什么字**，与平台无关；若各入口各写一份，会出现"取消在 A 平台
生效、在 B 平台不生效"的跨平台不一致。宿主只负责**执行**（按令牌取消 turn），
不负责**识别**。匹配口径刻意严格：只认整条输入等于某个词条，不认包含——伪装
成命令的自由文本会让 agent 静默不回复，比漏判更难被用户理解。

## 扩展点地图

| 目标 | 机制 |
|---|---|
| 新增 LLM provider | 实现 `echo_defs::LlmProvider`，在 `echo-module-provider-llm` 的 `create_provider` 加一个分支（工厂产物登记为 `ctx.llm` 供诊断） |
| 新增工具 | 内置：实现 `echo_defs::Tool` 并注册进 `ToolRegistry`（现役实现在 `echo-module-tools`）；外部：写进程模块贡献 `ToolContribution`，经 `echo-module-adapter` 适配（MCP 走桥接模块） |
| 新增平台 | 写一个**实例模块**（`InstanceModule` + `InstanceContribution`：能力位 / UI 描述符 / 会话标签 / **模型工具声明**），在 `[instances.<platform>]` 配好 `enabled` 与 `module.command`。宿主侧无需改代码——配置、门控、工具桥、提示词边界都是平台中立的（见 [模块开发指南](../modules/module-authoring.md)「实例模块」） |
| 拦截请求/工具/turn | EventBus 上注册 lifecycle 事件监听器 |
| 工具策略 | 超时已由 harness `tool_guard_timeout` 守卫（定义在 `agent/tool_exec.rs`、在 `agent/turn.rs` 装配进工具执行）；`ToolPipeline` 中间件机制已备、生产路径未接线 |
| 新增命令 | 新域 → 写模块并声明 `Contribution::Command`；宿主保留面（自管理 / 会话写 / turn 控制）仍在 `apply_command` 的 match 里加分支；平台专属动作优先走 `InvokeInstanceOp` 方法名而非新增变体 |
| 会话状态扩展 | 扩展 `SessionEvent` 枚举（echo-session），从日志渲染 |

## 关键文档

- [文档总览](../index.md)：文档群入口与阅读路径；[开发指南](../dev/guide.md)：仓库结构、关键抽象、构建与测试、治理门禁
- [Core 后端](../core/core.md) → [框架（内核）](../core/frame.md) / [模块化设计](../modules/modules.md)：框架与模块两支主线
- 运行时子系统：[多 Agent 与会话](../core/agents.md)、[Agent 循环](../core/agent-loop.md)、[会话记忆](../core/memory.md)、[多模态输入](../core/multimodal.md)、[配置持久化](../core/config-persistence.md)
- 数据流与部署：[协议与数据流](../protocol/protocol.md)、[部署与自更新](../ops/deploy.md)
