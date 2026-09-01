---
group: 开发指南
x: 1792
y: 48
link: ["dev-testing | 测试策略"]
---

# 开发指南

EchoAgentCore 开发速查：仓库结构、关键抽象、关键流程、构建与测试。深入主题见各模块文档。

## 文档地图

| 文档 | 内容 |
|---|---|
| [architecture.md](./architecture.md) | 架构脊柱：组合、crate、接缝、事件、会话、扩展点 |
| [protocol.md](./protocol.md) | 前后端线协议（management WS 契约） |
| [ops-deploy.md](./ops-deploy.md) | 安装、systemd 服务、受控自更新 |
| [core-config-persistence.md](./core-config-persistence.md) | ConfigStore、配置/门控/会话持久化 |
| [core-background-tasks.md](./core-background-tasks.md) | 后台任务、并行分支、有序整合、投递目标 |
| [adapter-qq-gating.md](./adapter-qq-gating.md) | QQ 消息门控管道（五层）、运行时可变 |
| [dev-testing.md](./dev-testing.md) | 测试策略：单元 / proptest / 集成 / 并发 |
| [adr-index.md](./adr-index.md) | 架构决策记录（ADR 0001-0017） |

> 架构/Agent/Adapter/Config 的通用细节通过源码注释（`//! module doc`）和 README 维护，避免文档与代码分叉。

## 仓库结构

```text
EchoAgentCore/
├── config/                    # Core TOML 配置模板
├── skills/                    # SKILL.md 技能定义（运行时热重载）
├── source/
│   ├── defs/echo-defs/        # Service Definition 层（词汇 + trait，零实现）
│   ├── protocol/echo-protocol/# 前后端线契约 crate
│   ├── backend/
│   │   ├── echo-core/         # OneBot v11 协议类型
│   │   ├── echo-server/       # 反向 WebSocket 服务器
│   │   ├── echo-adapter/      # Adapter trait + 过滤管道 + ConfigStore
│   │   ├── echo-agent/        # Agent 框架（agent/commands、adapter_bridge）
│   │   ├── echo-adapter-qq/   # QQ/OneBot 适配器
│   │   └── echo-test-utils/   # 共享测试 mock（仅 dev-dependency）
│   └── core/                  # echo-agent-core 二进制（组合根）
├── document/                  # 本文档群（ProDoc 格式）
├── packaging/systemd/         # 用户服务模板
├── scripts/                   # 安装器与受控更新器
├── napcat/                    # NapCat Docker 配置
└── Cargo.toml                 # Workspace 清单
```

## 关键 Trait

- `LlmProvider`（echo-defs）— LLM 后端接缝；providers：OpenAI/Anthropic/Ollama
- `Tool`（echo-defs）— LLM 可调用函数；`timeout_hint` 自声明执行超时
- `SkillProvider` / `Skill`（echo-defs）— 技能接缝；`SkillRegistry` 在 echo-agent
- `ChatAdapter`（echo-defs）— 平台接缝（Message/Target/Channel 词汇）
- `Adapter`（echo-adapter）— 平台集成 trait（QQ/OneBot），逐步收敛到 `ChatAdapter`
- `MessageFilter` — 入站消息过滤器（白名单、黑名单、频率、关键词、长度）
- `Handler` — OneBot 事件处理器（优先级链、panic 隔离）

## 关键抽象

- `ConfigStore` — TOML 原子读改写（Agent + QqAdapter 共享，杜绝并发写覆盖）
- `GateMode` — 强类型门控枚举（`none`/`allowlist`/`denylist`），echo-defs 定义、echo-protocol 共享
- `AgentMessageHook` — echo-agent 实现的单向 `InboundMessageHook`；平台输出必须走工具
- `FanoutHandle` — 多订阅者事件扇出（自动清理失效订阅者）

## 关键流程

1. **QQ 输入 hook**：QqHandler → 触发门控 → 过滤管道 → AgentMessageHook → 结构化 `qq_message` 输入
2. **QQ 输出工具**：Agent 工具调用 → `send_private_msg` / `send_group_msg` → QqAdapter::send_message
3. **前端输入**：WS 命令帧 → `BackendCommand::SendMessage` → agent.process_message → `BackendEvent::AgentOutput`
4. **API 配置**：Panel /api → `UpdateApiConfig` → 应用 + ConfigStore 持久化 → `ApiConfigUpdated` 广播
5. **适配器生命周期**：QqAdapter::start() → 绑定端口 → 服务器任务 → 接受连接 → QqHandler
6. **技能热重载**：周期扫描 → 发现 SKILL.md → 保留启用状态 → 替换注册表 → 清提示词缓存
7. **自更新**：授权会话 → 固定 framework_update 工具 → echo-agent-core-update.service → 构建 → 原子替换 → Core 重启（排空优先）
8. **后台任务**：上下文快照 → 分离分支 → 完成缓冲 → 创建序整合 → 显式目标投递

## 构建与测试

```bash
# Core（Agent + QQ 适配器 + management 服务）
cargo run --release -- --config config/echo-agent-core.local.toml

# 全量测试（各 crate 单元 + proptest + 集成）
cargo test --workspace

# 质量门禁（CI 三项全卡）
cargo clippy --workspace --all-targets
cargo fmt --all --check

# 指定 crate
cargo test -p echo-agent
```
