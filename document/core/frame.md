---
id: frame
title: "框架（内核）"
group: 框架
x: 640
y: 820
link: ["agents | 多 Agent 与会话", "memory | 会话记忆", "multimodal | 多模态输入 | r>l", "config-persistence | 配置持久化 | r>l", "core-logging | 日志与可观测性", "security-redaction | 敏感信息隔离"]
---

# 框架（内核）

> **定位**：本文描述 EchoAgent 的**内核**——插件化软件体系里"让一切插件都能挂上去"的最小机制集：组合与装配、服务定位、事件总线、插件宿主、进程结构。读者：需要理解系统骨架的开发者。
> 设计原则的完整表述在 [架构总览](../overview/architecture.md)「设计原则」，本文只写内核如何落地它们。姊妹篇：[插件化设计](../plugins/core-plugins.md)（插件体系与装载清单）；运行时子系统见 [多 Agent 与会话](./core-agents.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)。

## 内核与插件：一切皆插件

内核只含机制（服务注册表 / 事件总线 / 插件宿主 / 线协议 / 组合根）；能力以**插件**装载——循环、provider、工具、技能、插件实例、工作区、subagent、联邦、自更新——配置可禁用到"只剩内核仍能启动"。无特权核心与能力接缝两条原则见 [架构总览](../overview/architecture.md)「设计原则」；内核自己直接落地的两条是：

- **服务登记 + 显式注入**：服务以稳定键登记在 `Ctx`（`ctx.llm` / `ctx.loop` / `ctx.sanitizer`）供**可发现性与诊断**；实际消费走显式参数——provider 进 `AgentBuilder`、`TurnRunner` 由 `set_loop_runner` 下发。**挂载顺序由 `plugins.toml` 的挂载计划决定，不由服务可用性驱动**
- **类型化事件 = 扩展点**：`EventBus` 的 Observe / Waterfall / Parallel / Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）

**没有"内核豁免"**：联邦的执行层（路由表 / 代理工具 / 裁决）与自更新都已是内置插件（`echo-agent.federation` / `echo-agent.selfupdate`），可各自独立禁用——禁用后内核照常启动，相关能力显式降级并报出「插件未启用」。仍直连组合根的只有**机制本身**：联邦链路泵（握手 / 心跳 / 重连）与实例部署解析，它们不含能力语义，也就没有开关。装载清单与门控规则见 [插件化设计](../plugins/core-plugins.md)。

## 组合根与启动装配

二进制 `echo-agent-core`（`source/core/`）是唯一的组合根，启动按序完成：

1. 加载配置（`core.toml`，见 [配置持久化](./core-config-persistence.md)）
2. 构建 LLM provider（计量 + 脱敏装饰）并登记 `ctx.llm`
3. **启动 `plugins.toml` 的子进程插件**（含实例插件进程）——这一步在宿主装配之前，宿主服务（如 `sanitizer`）在启动前先注册好供 `HostCall` 回呼
4. 装配进程级插件宿主：`PluginRegistry::apply_disabled`（禁用插件只注册不挂载）先于挂载
5. 一次 `mount_plan` 按 `plugins.toml` 的 `kind = "builtin"` 行序挂载内置插件（无该行 = 目录顺序全启用）——**工具集 / 技能目录 / 工作区 / subagent / 插件实例 / 管理面 / 循环 / provider 全是 mount 的副作用**，management WS 随管理面插件的 mount 起停；同时注册远程插件门控壳与外部条目
6. 挂载完成后解析 `loop` 服务（由 `echo-plugin-loop` `provide` 的那一个 `TurnRunner`）——**解析失败即启动失败**，不静默降级：没有驱动的 agent 只会收消息不干活，比崩溃更难查
7. 创建多 agent 监督器，按人格回填循环注入（`loop.single` / `loop.parallel`；同一 runner 实例，模式只改准入策略）

> `plugins.toml` 与 core.toml 同目录，不存在 = 不报错（外部插件特性休眠、内置插件按目录默认全启用）。`echo-agent-core dump-config` 只打印它的分层合成结果（profile / bundle / patch / 内联层 + 每行来源），用来排查"这行配置从哪来"——不需要真启动。

## 服务定位与事件

- `Ctx`：进程级服务登记表——`ctx.llm` / `ctx.loop` / `ctx.sanitizer` 等稳定服务键，供可发现性与诊断；**消费侧走显式参数**（`AgentBuilder` 收 provider、`set_loop_runner` 收 runner），不靠运行期键解析
- `EventBus`：类型化事件分发（Observe / Waterfall / Parallel / Serial）；waterfall 监听器可改写或拒绝（如 `AgentPreStep`）
- `Disposer` / `ScopedRegistry`：一切注册均为**可逆副作用**，卸载时依序回滚
- `EventSink`：**进程级事件汇聚点**——所有人格事件直投于此，Panel 单连接即可看到全部活动；进程级职责由核心服务代理（非人格）承担（无"主智能体"）
- 结构化输入标记：单一事实来源是 `echo_defs::input_marker`（**构造、判定、解析**集中一处；`echo-agent` 侧只留 re-export），平台名由标记本身带出，框架侧不列具体平台

## 插件宿主

- 契约：`Plugin` trait（生命周期钩子）+ `PluginManifest`（id / name / version / **kind** / entry）。`kind` 是**开放字符串标签**，词汇表归宿主所有——框架只透传、从不按它分支决策；宿主侧「哪些插件按实例插件处理」的唯一真值是 `plugins::INSTANCE_PLUGIN_KIND`（`"instance"`，线上旧名 `adapter`）
- `mount` 返回 disposer；`register_and_mount` 注册并按启用态挂载；宿主为**进程级单例**（全体人格共享）
- 门控：全局启用 ∧ persona 白名单（`Agent::apply_capabilities` 逐人格计算）
- 热重载：数据类插件（技能/工具/清单）秒级热重载；代码类插件经原子二进制替换 + 进程重启生效
- 两种装载方式：**内联**（编译期、性能零开销、随二进制更新）与**外部进程**（独立二进制 / 任意语言、stdio 帧协议、崩溃自动重启、`plugins.toml` 装载）。`plugins.toml` 同时是**内置插件**的组合表——`kind = "builtin"` 的行决定内置插件的集合、顺序与启用态（未提到的照旧挂载）——协议与 SDK 见 [插件开发指南](../plugins/plugin-authoring.md)

## 进程结构

- Cargo 二进制名 `echo-agent-core`（`source/core/Cargo.toml`），由 systemd 用户服务运行；`install.sh` 安装后落盘为 `$LIBEXEC_DIR/echo-agent-core-bin`（libexec 文件名，非 Cargo bin 名）
- 库结构：框架机制在 `source/backend/echo-agent`（`agent/`）；插件机制在 `source/plugin/`（host 领域中立、adapter 做领域适配），**内置插件实现各自成 crate**（`echo-plugin-tools` / `-shell` / `-skills` / `-subagent` / `-workspace` / `-management` / `-loop` / `-provider-llm` / `-federation` / `-self-update`），宿主侧 `packages/` 只剩三个内联包（`tools_builtin` 端口接线 / `instance_ops` 入站桥 / `federation` 兼容面）；Shell 机制在 `source/shell/echo-shell`。契约层 crate（`echo-defs` / `echo-context` / `echo-session` / `echo-loop` / `echo-protocol` 等）见 [架构总览](../overview/architecture.md) 的 crate 地图
- 运维命令与部署布局见 [部署与自更新](../ops/ops-deploy.md)

---

运行时子系统：[多 Agent 与会话](./core-agents.md)（Persona 装配、会话模型、工作区通道）、[Agent 循环](./core-agent-loop.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)；插件体系见 [插件化设计](../plugins/core-plugins.md)；安全见 [敏感信息隔离（脱敏服务）](./security-redaction-design.md)。
