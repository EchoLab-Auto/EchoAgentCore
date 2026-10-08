---
id: frame
title: "框架（内核）"
group: 框架
x: 640
y: 320
link: ["agents | 多 Agent 与会话 | r>l", "memory | 会话记忆 | r>l", "multimodal | 多模态输入 | r>l", "config-persistence | 配置持久化 | r>l", "security-redaction | 敏感信息隔离 | b>t"]
---

# 框架（内核）

> **定位**：本文描述 EchoAgent 的**内核**——插件化软件体系里"让一切插件都能挂上去"的最小机制集：组合与装配、服务定位、事件总线、插件宿主、进程结构。读者：需要理解系统骨架的开发者。
> 姊妹篇：[插件化设计](./core-plugins.md)（插件体系与装载清单）；运行时子系统见 [多 Agent 与会话](./core-agents.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)。

## 内核与插件：一切皆插件

本仓库按 DeepSeek Harness 的设计模式组织，五条核心原则：

1. **无特权核心**：agent 循环、模型适配器、工具、技能、平台适配器都可挂载/替换；注册是可逆副作用（返回 disposer）
2. **服务定位 + 依赖注入**：服务通过稳定键从 `Ctx` 解析（`ctx.llm` / `ctx.loop`），加载顺序由服务可用性驱动，扩展插件只依赖定义层
3. **类型化事件 = 扩展点**：`EventBus` 的 Observe / Waterfall / Parallel / Serial 四种分发；waterfall 是 around-middleware（`next()` 委托、不调即短路）
4. **能力接缝**：Service Definition / Service Provider / Consumer 三角独立演进、独立成 crate；依赖方向单向无环
5. **事件溯源会话日志**：日志是唯一事实来源，模型上下文由日志投影（"模型可见 ⟺ 已记录"）

内核只含机制（服务注册表 / 事件总线 / 插件宿主 / 线协议 / 组合根）；全部能力以**插件**装载——循环、provider、工具、技能、适配器、工作区、subagent、联邦——配置可禁用到"只剩内核仍能启动"。装载清单与门控规则见 [插件化设计](./core-plugins.md)。

## 组合根与启动装配

二进制 `echo-agent-core`（`source/core/`）是唯一的组合根，启动按序完成：

1. 加载配置（`core.toml`，见 [配置持久化](./core-config-persistence.md)）
2. 构建 LLM provider 工厂，注册 `ctx.llm`
3. 装配插件宿主：`apply_disabled`（禁用插件只注册不挂载）先于挂载执行
4. 注册内置工具、技能目录与各包（`packages/`），创建多 agent 监督器
5. 启动 QQ 适配器（受 `[adapters.qq].enabled` 与插件状态双重门控）与 management WS
6. 各人格启动循环回填按人格的循环注入（`loop.single` / `loop.parallel`）

## 服务定位与事件

- `Ctx`：进程级服务注册表——`ctx.llm` / `ctx.loop` 等稳定服务键，加载顺序由服务可用性驱动
- `EventBus`：类型化事件分发（Observe / Waterfall / Parallel / Serial）；waterfall 监听器可改写或拒绝（如 `AgentPreStep`）
- `Disposer` / `ScopedRegistry`：一切注册均为**可逆副作用**，卸载时依序回滚
- `EventSink`：**进程级事件汇聚点**——所有人格事件直投于此，Panel 单连接即可看到全部活动；进程级职责由核心服务代理（非人格）承担（无"主智能体"）
- 结构化输入标记（`<qq_message_hook>` / `<backend_message_hook>` / `<timer_event>`）的单一事实来源是 `input_marker.rs`：构造、判定、解析集中一处

## 插件宿主

- 契约：`Plugin` trait（生命周期钩子）+ `PluginManifest`（id / name / version / kind / entry）+ `PluginKind`（skill / tool / provider / loop / adapter / orchestration / management / interaction）
- `mount` 返回 disposer；`register_and_mount` 注册并按启用态挂载；宿主为**进程级单例**（全体人格共享）
- 门控：全局启用 ∧ persona 白名单（`Agent::apply_capabilities` 逐人格计算）
- 热重载：数据类插件（技能/工具/清单）秒级热重载；代码类插件经原子二进制替换 + 进程重启生效
- 两种装载方式：**内联**（编译期、性能零开销、随二进制更新）与**外部进程**（独立二进制 / 任意语言、stdio 帧协议、崩溃自动重启、`plugins.toml` 装载）——协议与 SDK 见 [插件开发指南](./plugin-authoring.md)

## 进程结构

- Cargo 二进制名 `echo-agent-core`（`source/core/Cargo.toml`），由 systemd 用户服务运行；`install.sh` 安装后落盘为 `$LIBEXEC_DIR/echo-agent-core-bin`（libexec 文件名，非 Cargo bin 名）
- 库结构：框架机制在 `source/backend/echo-agent`（`agent/`），各插件实现按包分目录在 `packages/`；契约层 crate（`echo-defs` / `echo-context` / `echo-session` / `echo-loop` / `echo-protocol` 等）见 [架构总览](./architecture.md) 的 crate 地图
- 运维命令与部署布局见 [部署与自更新](./ops-deploy.md)

## 延伸阅读

- 运行时子系统：[多 Agent 与会话](./core-agents.md)（Persona 装配、会话模型、工作区通道）、[Agent 循环](./core-agent-loop.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)
- 插件体系：[插件化设计](./core-plugins.md)——内置插件清单、能力开关、外部进程插件
- 安全：[敏感信息隔离（脱敏服务）](./security-redaction-design.md)——出口卡口与保证边界
