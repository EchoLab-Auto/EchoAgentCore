---
id: frame
title: "框架（内核）"
group: Core
x: 48
y: 216
link: ["agents | 多 Agent 与会话", "memory | 会话记忆", "multimodal | 多模态输入", "config-persistence | 配置持久化", "logging | 日志与可观测性", "security-redaction | 敏感信息隔离"]
---

# 框架（内核）

> **定位**：本文描述 EchoAgent 的**内核**——模块化软件体系里"让一切模块都能挂上去"的最小机制集：组合与装配、服务定位、事件总线、模块宿主、进程结构。读者：需要理解系统骨架的开发者。
> 设计原则的完整表述在 [架构总览](../overview/architecture.md)「设计原则」，本文只写内核如何落地它们。姊妹篇：[模块化设计](../modules/modules.md)（模块体系与装载清单）；运行时子系统见 [多 Agent 与会话](./agents.md)、[会话记忆](./memory.md)、[多模态输入](./multimodal.md)、[配置持久化](./config-persistence.md)。

## 内核与模块：一切皆模块

内核只含机制（服务注册表 / 事件总线 / 模块宿主 / 线协议 / 组合根）；能力以**模块**装载——循环、provider、工具、技能、模块实例、工作区、subagent、联邦、自更新——配置可禁用到"只剩内核仍能启动"。无特权核心与能力接缝两条原则见 [架构总览](../overview/architecture.md)「设计原则」；内核自己直接落地的两条是：

- **服务登记 + 显式注入**：服务以稳定键登记在 `Ctx`（`ctx.llm` / `ctx.loop` / `ctx.sanitizer`）供**可发现性与诊断**；实际消费走显式参数——provider 进 `AgentBuilder`、`TurnRunner` 由 `set_loop_runner` 下发。**挂载顺序由 `modules.toml` 的挂载计划决定，不由服务可用性驱动**
- **类型化事件 = 扩展点**：`EventBus` 的 Observe / Waterfall / Parallel / Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）

**没有"内核豁免"**：联邦的执行层（路由表 / 代理工具 / 裁决）与自更新都已是内置模块（`echo-agent.federation` / `echo-agent.selfupdate`），可各自独立禁用——禁用后内核照常启动，相关能力显式降级并报出「模块未启用」。仍直连组合根的只有**机制本身**：联邦链路泵（握手 / 心跳 / 重连）与实例部署解析，它们不含能力语义，也就没有开关。装载清单与门控规则见 [模块化设计](../modules/modules.md)。

## 组合根与启动装配

二进制 `echo-agent-core`（`source/modules/core/`）是唯一的组合根，启动按序完成：

1. 加载配置（`core.toml`，见 [配置持久化](./config-persistence.md)）
2. 构建 LLM provider（计量 + 脱敏装饰）并登记 `ctx.llm`
3. **启动 `modules.toml` 的子进程模块**（含实例模块进程）——这一步在宿主装配之前，宿主服务（如 `sanitizer`）在启动前先注册好供 `HostCall` 回呼
4. 装配进程级模块宿主：`ModuleRegistry::apply_disabled`（禁用模块只注册不挂载）先于挂载
5. 一次 `mount_plan` 按 `modules.toml` 的 `kind = "builtin"` 行序挂载内置模块（无该行 = 目录顺序全启用）——**工具集 / 技能目录 / 工作区 / subagent / 模块实例 / 管理面 / 循环 / provider 全是 mount 的副作用**，management WS 随管理面模块的 mount 起停；同时注册远程模块门控壳与外部条目
6. 挂载完成后解析 `loop` 服务（由 `agent-module-loop` `provide` 的那一个 `TurnRunner`）——**解析失败即启动失败**，不静默降级：没有驱动的 agent 只会收消息不干活，比崩溃更难查
7. 创建多 agent 监督器，把同一个 `TurnRunner` 注入每个 Agent（`set_loop_runner`；循环只有单会话串行一种准入，没有模式可切）

> `modules.toml` 与 core.toml 同目录，不存在 = 不报错（外部模块特性休眠、内置模块按目录默认全启用）。`echo-agent-core dump-config` 只打印它的分层合成结果（profile / bundle / patch / 内联层 + 每行来源），用来排查"这行配置从哪来"——不需要真启动。

## 服务定位与事件

- `Ctx`：进程级服务登记表——`ctx.llm` / `ctx.loop` / `ctx.sanitizer` 等稳定服务键，供可发现性与诊断；**消费侧走显式参数**（`AgentBuilder` 收 provider、`set_loop_runner` 收 runner），不靠运行期键解析
- `EventBus`：类型化事件分发（Observe / Waterfall / Parallel / Serial）；waterfall 监听器可改写或拒绝（如 `AgentPreStep`）
- `Disposer` / `ScopedRegistry`：一切注册均为**可逆副作用**，卸载时依序回滚
- `EventSink`：**进程级事件汇聚点**——所有人格事件直投于此，Panel 单连接即可看到全部活动；进程级职责由核心服务代理（非人格）承担（无"主智能体"）
- 结构化输入标记：单一事实来源是 `echo_defs::input_marker`（**构造、判定、解析**集中一处；`echo-agent` 侧只留 re-export），平台名由标记本身带出，框架侧不列具体平台

## 模块宿主

- 契约：`Module` trait（生命周期钩子）+ `ModuleManifest`（id / name / version / **kind** / entry）。`kind` 是**开放字符串标签**，词汇表归宿主所有——框架只透传、从不按它分支决策；宿主侧「哪些模块按实例模块处理」的唯一真值是 `modules::INSTANCE_MODULE_KIND`（`"instance"`，线上旧名 `adapter`）
- `mount` 返回 disposer；`register_and_mount` 注册并按启用态挂载；宿主为**进程级单例**（全体人格共享）
- 门控：全局启用 ∧ persona 白名单（`Agent::apply_capabilities` 逐人格计算）
- 热重载：数据类模块（技能/工具/清单）秒级热重载；代码类模块经原子二进制替换 + 进程重启生效
- 两种装载方式：**内联**（编译期、性能零开销、随二进制更新）与**外部进程**（独立二进制 / 任意语言、stdio 帧协议、崩溃自动重启、`modules.toml` 装载）。`modules.toml` 同时是**内置模块**的组合表——`kind = "builtin"` 的行决定内置模块的集合、顺序与启用态（未提到的照旧挂载）——协议与 SDK 见 [模块开发指南](../modules/module-authoring.md)

## 进程结构

- Cargo 二进制名 `echo-agent-core`（`source/modules/core/Cargo.toml`），由 systemd 用户服务运行；`install.sh` 安装后落盘为 `$LIBEXEC_DIR/echo-agent-core-bin`（libexec 文件名，非 Cargo bin 名）
- 库结构：框架机制在 `source/modules/echo-agent`（`agent/`）；模块机制在 `source/modules/`（host 领域中立、adapter 做领域适配），**内置模块实现各自成 crate**（`agent-module-tools` / `-shell` / `-skills` / `-subagent` / `-workspace` / `-management` / `-loop` / `-provider-llm` / `-federation` / `-self-update`），宿主侧 `packages/` 只剩三个内联包（`tools_builtin` 端口接线 / `instance_ops` 入站桥 / `federation` 兼容面）；Shell 机制在 `source/modules/agent-module-shell`。契约层 crate（`echo-defs` / `echo-context` / `echo-session` / `agent-module-loop` / `agent-module-management` 等）见 [架构总览](../overview/architecture.md) 的 crate 地图
- 运维命令与部署布局见 [部署与自更新](../ops/deploy.md)

---

运行时子系统：[多 Agent 与会话](./agents.md)（Persona 装配、会话模型、工作区通道）、[Agent 循环](./agent-loop.md)、[会话记忆](./memory.md)、[多模态输入](./multimodal.md)、[配置持久化](./config-persistence.md)；模块体系见 [模块化设计](../modules/modules.md)；安全见 [敏感信息隔离（脱敏服务）](./security-redaction.md)。
