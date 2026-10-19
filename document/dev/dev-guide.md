---
id: dev-guide
title: "开发指南"
group: 工程
x: 1920
y: 2050
link: ["dev-testing | 测试策略", "documentation-guide | 文档规范"]
---

# 开发指南

> **定位**：EchoAgentCore 开发速查——仓库结构、关键抽象、关键流程、构建与测试；深入主题见各模块文档。读者：本仓库的开发者（含 Agent）。
> 相关文档：[架构总览](../overview/architecture.md)、[协议与数据流](../protocol/protocol.md)、[测试策略](./dev-testing.md)、[文档规范](../documentation-guide.md)。

## 文档地图

全量分层导航见 [文档总览](../index.md)（每篇一句话定位）；开发者常用直达：

| 文档 | 内容 |
|---|---|
| [架构总览](../overview/architecture.md) | 架构脊柱：组合、crate、接缝、事件、会话、扩展点 |
| [框架（内核）](../core/frame.md) | 内核机制：组合与装配、服务定位、事件总线、插件宿主、进程结构 |
| [协议与数据流](../protocol/protocol.md) | 前后端线协议（management WS 契约） |
| [插件化设计](../plugins/core-plugins.md) | 插件模型、内置清单、能力开关与扩展方式 |
| [测试策略](./dev-testing.md) | 测试分层、Mock 夹具、质量门禁 |
| [文档规范](../documentation-guide.md) | 目录分层、单篇体例与模板 |
| [部署与自更新](../ops/ops-deploy.md) | 安装、容器化、受控自更新 |
> 架构/Agent/Instance/Config 的通用细节通过源码注释（`//! module doc`）和 README 维护，避免文档与代码分叉。

## 仓库结构

```text
EchoAgentCore/
├── config/                    # Core TOML 配置模板
├── skills/                    # SKILL.md 技能定义（运行时热重载）
├── source/
│   ├── defs/echo-defs/        # Service Definition 层（词汇 + trait，零实现）
│   ├── context/echo-context/  # Ctx 服务定位 / EventBus / Disposer / ScopedRegistry
│   ├── session/echo-session/  # 事件溯源会话存储（EventLog + derive_messages）
│   ├── loop/echo-loop/        # TurnRunner 默认驱动 + ToolPipeline
│   ├── llm/echo-llm-*/        # LLM provider crates（openai/anthropic/ollama）
│   ├── security/echo-sanitize/     # 脱敏服务（出口卡口，注册表式 Redactor）
│   ├── plugin/echo-plugin/         # Plugin trait / PluginManifest / 注册表
│   ├── plugin/echo-plugin-api/     # 插件协议契约（消息/贡献类型，冻结）
│   ├── plugin/echo-plugin-loader/  # 分层插件配置合成（依赖排序 + provenance）
│   ├── plugin/echo-plugin-sdk/     # 插件侧运行时 SDK（stdio 服务）
│   ├── plugin/echo-plugin-host/    # 插件宿主（Transport/Supervisor/conformance）
│   ├── plugin/echo-plugin-example/ # 试点插件（websearch + calculator）
│   ├── federation/echo-federation/  # 联邦层：Core↔Core 对等链路（frame/invite/link）
│   ├── backend/
│   │   ├── echo-config/        # TOML 原子读改写（不认识任何段语义）
│   │   ├── echo-agent/        # Agent 框架
│   │   │   ├── agent/         # 循环/命令分派/边界/提示词/压缩（框架核心）
│   │   │   └── packages/      # 第一层 = 包归属
│   │   │       ├── tools_builtin/   skills_dir/   instance_ops/  workspace/
│   │   │       ├── subagent/        provider_llm/ federation/
│   │   │       └── tool/      # 特例：工具子系统跨包机制（ToolRegistry，非包）
│   │   └── echo-test-utils/   # 共享测试 mock（仅 dev-dependency）
│   # QQ 插件实例（OneBot 类型 / 反向 WS / NapCat 管理）已迁出为独立进程插件
│   # （独立仓库；宿主经 echo-plugin-host 的 RemoteInstance 接入）
│   └── core/                  # echo-agent-core 二进制（组合根）
├── document/                  # 文档群（index + overview/core/plugins/panel/protocol/federation/ops/dev/design 分层）
├── npm/echo-agent/            # 容器化部署 CLI（Docker 编排；node:test 测试）
├── docker/entrypoint.sh       # Core 容器入口
├── packaging/systemd/         # 用户服务模板
├── scripts/                   # 安装器与受控更新器
├── napcat/                    # NapCat Docker 配置（宿主安装路径用）
├── Dockerfile                 # Core 容器镜像
└── Cargo.toml                 # Workspace 清单
```

## 关键抽象与 Trait

**接缝 trait（定义层）**：

- `LlmProvider`（echo-defs）— LLM 后端接缝；providers：OpenAI/Anthropic/Ollama
- `Tool`（echo-defs）— LLM 可调用函数；`timeout_hint` 自声明执行超时
- `SkillProvider` / `Skill`（echo-defs）— 技能接缝；`SkillRegistry` 在 echo-agent
- `PluginInstance`（echo-defs::plugin_instance）— **平台接缝的唯一实现路径**：生命周期/收发/状态面；进程外插件经宿主 `RemoteInstance` 实现它（`echo-plugin-host` 的 `impl PluginInstance for RemoteInstance`）。`InstanceRegistry` 同在 `echo-defs::plugin_instance`；`ConfigStore` 在独立的 `echo-config`（不认识任何配置段的语义）。平台专属管理动作不走能力扩展 trait，而是经 **`InvokeInstanceOp{instance, method, payload}`** 透传（插件侧 `custom` op 自行解释）——门控模式、名单、登录等取值因此是**参数字符串**而非共享枚举
- `ToolSpec`（echo-defs::plugin_instance，`PluginInstance::tool_specs` 的返回类型）— 插件实例**向模型暴露的工具声明**（name / description / parameters / package / `visible_reply`）。远程插件实例 = 插件 `InstanceContribution.tools` 原样；组合根的通用桥（`core/instance_tools.rs`）只做跨实例合并、`account` 寻址、出站闸门与 `invoke_custom` 透传，**不认识任何具体工具名**
- `Tool::produces_visible_output`（echo-defs）— 工具自声明「成功调用即产生平台可见输出」；循环据此抑制「临时等待回复」，不再按工具名清单判断
- `ChannelType` / `IncomingMessage` / `MessageTarget`（echo-defs::chat）— 跨插件实例边界的**消息词汇**（平台中立）；入站过滤在实例插件进程内做，宿主不持有过滤管道

**机制与运行时抽象**：

- `Ctx` / `EventBus` / `Disposer` / `ScopedRegistry`（echo-context）— 服务定位、类型化事件（Observe/Waterfall/Parallel/Serial）、可逆注册、作用域 shadowing
- `TurnRunner` / `ToolPipeline`（echo-loop）— turn/step 状态机与工具执行管道
- `EventLog` / `SessionEvent` / `derive_messages`（echo-session）— 事件溯源存储、日志投影
- `ConfigStore`（echo-defs::plugin_instance）— TOML 原子读改写（Agent 与插件实例共享，杜绝并发写覆盖）
- `InstanceHostConfig`（core::config）— **单平台的宿主配置视图**：`platform` / `shared`（透传表）/ `enabled` / ``instances` / `launch`。`CoreConfig.instances: BTreeMap<String, InstanceHostConfig>`，`load()` 遍历 `[instances.<name>]` 全部平台
- `instance_deploy`（core，单文件 ~230 行）— **只剩实例解析**：读 `[instances.<p>]` 实例表、按 persona 门控自动建档、推导默认 persona、把 persona 写回配置。**端口分配 / 容器名 / compose / 端口持久化已移交插件**（插件 `deploy` 模块）——它们是 NapCat 的部署形状，框架不持有（2026-10 收尾；原 `mod.rs` + `napcat.rs` 两个文件合并瘦身）
- `PluginManifest` / `PluginRegistry`（echo-plugin）— 插件清单与注册表；`echo-plugin-host` 提供 Transport（inproc/stdio）、监督与 `RemoteInstance`
- `AgentMessageHook`（echo-agent）— 入站消息进入 agent 的落点（`on_incoming_message` 固有方法，**不是 trait 实现**）：组合根把插件 `submit_input` 队列接到它；平台输出必须走工具
- **平台回复指引**：由平台中立的边界块（`BoundaryKind::PlatformHook`，只说「走该平台的发送工具」）+ 该平台的 transport 技能（如 `qq-transport`，说明具体工具名与参数）承担；循环层不做代码校验/纠偏。框架文案里不出现平台名与平台技能名
- `FanoutHandle`（echo-protocol::bridge）— 多订阅者事件扇出（自动清理失效订阅者）；进程内 mpsc 路径不带扇出序号，生产 WS 路径带

## 编码约定

- **错误处理**：库 crate 用 `thiserror` 定义具体错误类型（如 `echo_federation::FederationError`）；`anyhow` 只用于应用层（`echo-agent` 的 harness 与 `echo-agent-core` 组合根）。库错误经 `?` 自动转为 `anyhow::Error`
- **锁**：持锁期间不 `.await` 的同步锁优先用 `parking_lot`（无中毒、`lock()` 直接返回 guard）；必须用 `std::sync` 锁的 crate 一律 `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)`，生产代码不 `unwrap()` / `expect()` 锁（测试代码不限）
- **异步路径不阻塞**：async 函数及其同步调用链里不 `std::thread::sleep` / 自旋重试；阻塞 IO（git、子进程）经 `tokio::task::spawn_blocking`
- **文件规模**：单个源文件超过 ~1500 行（测试文件除外）即按职责拆子模块（`impl` 可分布在多个文件）

## 关键流程

1. **平台入站**：实例插件（进程外）`submit_input` → 宿主有界队列 → per-instance worker → `AgentMessageHook::on_incoming_message` → 结构化 `<X_message_hook>` 输入（先过命令域）
2. **平台出站**：Agent 工具调用 → **实例插件声明的工具**（wire 侧 `echo_plugin_api::ToolContribution`，宿主侧落到 `ToolSpec`）→ 宿主通用桥 → `invoke_custom(<工具名>)` → 插件自行投递（框架不认识任何工具名）
3. **前端输入**：WS 命令帧 → `BackendCommand::SendMessage` → agent.process_message → `BackendEvent::AgentOutput`
4. **API 配置**：Panel /api → `UpdateApiConfig` → 应用 + ConfigStore 持久化 → `ApiConfigUpdated` 广播
5. **插件实例生命周期**：组合根按 `[instances.<platform>.plugin]` 拉起插件进程 → `InstanceContribution` 注册（含工具/UI/能力位）→ `StartInstance` → 插件自己连平台
6. **技能重载**：按需触发（`ReloadSkills` 命令 / 保存·删除技能后）→ 发现 SKILL.md → 保留启用状态 → 替换注册表（**覆盖全部运行人格**）→ 清提示词缓存（周期扫描已移除）
7. **自更新**：agent 经 bash 运行受管更新器（`update.sh` → echo-agent-core-update.service）→ 构建 → 原子替换 → Core 重启（排空优先）
8. **异步子任务**（`spawn_subagent`）：受理即返回子任务 id → 子 agent 隔离上下文执行 → 完成后经 `<subagent_event>` 钩子以新 turn 回报主 agent（见 [Subagent 插件](../plugins/core-subagent.md)；旧「后台任务/并行分支」体系已删除）

## 构建、测试与门禁

```bash
# Core（Agent + 插件宿主 + management 服务）
cargo run --release -- --config config/echo-agent-core.local.toml

# 全量测试（各 crate 单元 + proptest + 集成）
cargo test --workspace

# 质量门禁（CI 三项全卡）
cargo clippy --workspace --all-targets
cargo fmt --all --check

# 指定 crate
cargo test -p echo-agent
```

### 治理门禁（CI）

- **依赖方向 lint**（`scripts/check-deps.sh`，CI `deps-lint` job）：扩展/provider crate 只依赖定义层（echo-defs/echo-context/echo-protocol）；`echo-llm-*` 不得依赖 agent 框架。违规即 CI 失败，不靠人肉 review
- **config 模板测试**（`source/core/tests/config_template.rs`）：`config/echo-agent-core.toml` 必须是合法 TOML 且含运行时依赖的 section，防模板漂移；并入 `cargo test`
- **跨仓库清单同步**（人工核对 + 测试守护）：新增/移除插件时四处清单必须同改——Core `GATED_PLUGIN_IDS` / `BUILTIN_PLUGIN_IDS`、更新器 `expected_ids`、Panel `PACKAGE_GATED_PLUGIN_IDS` / `PACKAGE_DISPLAY_NAMES`；新增**插件类型**（`PluginKind`）时 Core `as_str` wire 名与 Panel `PLUGIN_KIND_LABELS` 同改；协议新增命令/事件时 echo-protocol ⇄ `web/src/protocol.ts` 同改。守护测试：`capabilities.test.ts`（Panel 清单对齐）、`instance_manifests.rs` 的单测（Core `BUILTIN_PLUGIN_IDS` + 实例前缀 ⇄ 更新器 `expected_ids`；曾因 `orchestration` 移除后清单残留、每次更新误报缺 manifest 而补齐）、`update.sh` 插件感知校验（安装后核对二进制）、`protocol-sync.test.ts`（Panel 协议镜像：事件 reduce⊆union、联邦命令存在性快照、unit 命令字符串形态、禁止 `as unknown as` 双重断言绕过——补齐联邦 4 命令后归零）。详见 [插件化设计](../plugins/core-plugins.md)「新增包维度门控插件时的同步清单」

命令与 CI 作业矩阵见 [测试策略](./dev-testing.md)「质量门禁（CI）」。
