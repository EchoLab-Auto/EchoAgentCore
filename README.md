# EchoAgentCore

可独立部署的 Agent 核心服务：LLM agent 循环 + 工具/技能系统 + 会话记忆 + QQ（OneBot v11）适配器，通过 management WebSocket（默认 `127.0.0.1:3132`）向前端提供统一的命令/事件协议。

本仓库是 **Core（后端）**。Web 面板前端在独立仓库 [EchoAgentPanel](../EchoAgentPanel)（`echo-agent-panel` 二进制，Rust 中继 + Vue 3），任何实现了协议的前端都可以连接本服务。

```
 NapCat (QQ)                 EchoAgentCore                 EchoAgentPanel (Web)
 ┌──────────┐  reverse WS   ┌──────────────────────┐   WS/JSON   ┌─────────────┐
 │ OneBot11 │ ◄──────────►  │ echo-agent-core :3131 │ ◄─────────► │ :3132 连接方 │
 └──────────┘   :3131       │  Agent · Tools ·      │   :3132     └─────────────┘
                            │  Skills · Memory      │
                            └──────────────────────┘
```

## 仓库结构

```
EchoAgentCore/
├── config/echo-agent-core.toml   # Core 配置模板
├── skills/                       # SKILL.md 技能定义（Core 独占消费，按需重载）
├── source/
│   ├── defs/
│   │   └── echo-defs/            # Service Definition 层：词汇类型 + trait，零实现
│   ├── context/
│   │   └── echo-context/         # 服务定位 Ctx + 类型化 EventBus + 可逆注册 + 作用域
│   ├── session/
│   │   └── echo-session/         # 事件溯源会话存储：append-only 事件日志 + 投影 + 兼容迁移
│   ├── loop/
│   │   └── echo-loop/            # Agent 循环驱动：TurnRunner turn/step 状态机 + 工具执行管道
│   ├── llm/
│   │   ├── echo-llm-openai/      # OpenAI 兼容 provider（Service Provider 角色）
│   │   ├── echo-llm-anthropic/   # Anthropic Messages provider
│   │   └── echo-llm-ollama/      # Ollama provider（薄包装 OpenAI 兼容端点）
│   ├── protocol/
│   │   └── echo-protocol/        # 前后端线协议 crate（BackendCommand/BackendEvent/bridge）
│   ├── backend/
│   │   ├── echo-core/            # OneBot v11 协议类型（纯类型，无 I/O）
│   │   ├── echo-server/          # 反向 WebSocket 服务器（NapCat 接入）
│   │   ├── echo-adapter/         # 适配器抽象 + 过滤管道 + ConfigStore
│   │   ├── echo-agent/           # Agent 框架（packages/ 第一层按插件分包；packages/tool/ 为工具注册表机制）
│   │   ├── echo-adapter-qq/      # QQ/OneBot 适配器（门控、NapCat 客户端）
│   │   └── echo-test-utils/      # 共享测试 mock（仅 dev-dependency）
│   └── core/                     # echo-agent-core 二进制（组合根）
├── document/                     # 项目文档群（ProDoc 格式：index.md 入口）
├── npm/echo-agent/               # 容器化部署 CLI（Docker 编排 core+panel+napcat）
├── docker/entrypoint.sh          # Core 容器入口（配置解析 / socket 能力探测）
├── packaging/systemd/            # 用户级 systemd 单元模板
├── scripts/                      # install.sh / update.sh（受控自更新）
├── napcat/                       # NapCat Docker 配置（宿主安装路径用）
├── docker-compose.yml            # NapCat 容器（宿主安装路径用）
└── Dockerfile                    # Core 容器镜像（含 docker CLI；GHCR 发布）
```

### Crate 职责

| Crate | 职责 |
|---|---|
| `echo-defs` | **Service Definition 层**：LLM/工具/技能/平台消息词汇与 trait（`LlmProvider`/`Tool`/`SkillProvider`/`ChatAdapter`）、策略枚举（`GateMode` 等）、token 纯函数。零实现、零 harness 依赖 |
| `echo-context` | **服务定位与事件机制**：`Ctx`（按 key 注册/解析服务）、`EventBus`（Observe/Waterfall/Parallel/Serial 类型化事件）、`Disposer`（可逆注册）、`ScopedRegistry`（per-scope shadowing）。零 echo-* 依赖 |
| `echo-session` | **事件溯源会话存储**：`SessionEvent` 事件集、`EventLog`（append-only 持久化）、`derive_messages` 投影、compaction、`SessionHeader`（fork/resume）、v1–v4 旧格式兼容迁移 |
| `echo-loop` | **Agent 循环驱动**：`TurnRunner` turn/step 状态机（`turn/*`/`step/*`/`agent/*` 生命周期事件）、`ToolPipeline` 工具执行管道（pre/execute/post waterfall 中间件） |
| `echo-llm-openai` / `echo-llm-anthropic` / `echo-llm-ollama` | **LLM provider（Service Provider 角色）**：各自实现 `echo_defs::LlmProvider`,只依赖定义层 |
| `echo-protocol` | **前后端契约的唯一来源**：`BackendCommand`/`BackendEvent`/`WsMessage`、bridge；`GateMode`/`ThinkingMode`/`ReasoningEffort` 从 `echo-defs` re-export。前端只需依赖它 |
| `echo-agent` | Agent 框架：agent 循环、LLM provider（OpenAI/Anthropic/Ollama）、工具注册表、技能系统、trunk 记忆、编排（异步子任务 `spawn_subagent` / 自更新） |
| `echo-adapter` | 协议无关的适配器抽象：`Adapter` trait、`InboundMessageHook`、过滤管道、`ConfigStore` |
| `echo-adapter-qq` | QQ 适配器：反向 WS 接入、5 层门控、NapCat HTTP 客户端 |
| `echo-core` / `echo-server` | OneBot v11 类型 / 反向 WS 服务器（仅供 echo-adapter-qq 使用） |
| `echo-agent-core`（bin） | 组合根：加载配置、装配 Agent + QQ 适配器、对外提供 management WS |

依赖方向（单向，无环，扩展只依赖定义层）：

```
echo-defs ◄── echo-protocol ◄── echo-adapter ◄── echo-agent ◄── echo-agent-core (bin)
    ▲              ▲                ▲   ▲                          ▲
    └── echo-context ◄──────────────┴───┴── echo-adapter-qq ◄──────┘
                                          (→ echo-core, echo-server)
```

## 构建与运行

```bash
cp config/echo-agent-core.toml config/echo-agent-core.local.toml  # 填入 api_key 等

# 开发运行
cargo run -- --config config/echo-agent-core.local.toml

# 发布构建
cargo build --release -p echo-agent-core

# 测试与质量门（CI 强制执行）
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## 安装部署（systemd 用户服务）

```bash
./scripts/install.sh [--no-start]
# 先检查路径：./scripts/install.sh --dry-run
```

安装后：

| 路径 | 用途 |
|---|---|
| `~/.local/bin/echo-agent-core` | Core 启动器 |
| `~/.local/libexec/echo-agent-core/` | 运行二进制与 updater |
| `~/.config/echo-agent-core/core.toml` | 持久化 Core 配置 |
| `~/.local/share/echo-agent-core/source` | 受管 Git 检出（自更新源） |
| `~/.local/state/echo-agent-core/` | 更新锁、版本与状态 |
| `echo-agent-core.service` | 常驻 Core 服务 |
| `echo-agent-core-update.service` | 一次性更新器（agent 自更新工具触发） |

详见 [document/ops-deploy.md](document/ops-deploy.md)。卸载：`./scripts/uninstall.sh`
（保留配置与会话历史；`--purge` 连配置一起删，`--dry-run` 先预览）。

> 注意：长驻 `echo-agent-core.service` 与一次性更新器服务均设
> `NoNewPrivileges=true`——agent 不再提供 setuid 提权（`run_sudo` 已废弃移除），
> 两个单元都限制被攻破进程/构建脚本的提权面。

### Docker

**一键部署（npm 编排，推荐）**：

```bash
npx @echolab-auto/echo-agent up   # core + panel + napcat 三容器，面板在 :8080
```

包源码见 [`npm/echo-agent/`](npm/echo-agent/)（生成 compose + 配置模板并代理
`docker compose` 的日常操作；镜像发布见 `.github/workflows/docker.yml`）。

**手动构建/运行**（无 registry 镜像时）：

```bash
docker build -t echo-agent-core .
docker run -v ./config:/config -v ./data:/data -p 3131:3131 -p 3132:3132 echo-agent-core
```

镜像内置 docker CLI（可选挂 `/var/run/docker.sock`：QR 取图 / 文件桥 /
NapCat 容器管理）；容器模式的完整配置模板见
[`npm/echo-agent/templates/core.toml`](npm/echo-agent/templates/core.toml)。

仓库根目录的 `docker-compose.yml` 只编排 NapCat（QQ 协议端），宿主安装
（systemd）时由 Core 用来起停 NapCat 容器。

## 配置

见 [config/echo-agent-core.toml](config/echo-agent-core.toml) 内联注释。要点：

- `[agent]`：LLM provider/model/base_url/api_key（env 覆盖：`OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`DEEPSEEK_API_KEY`）、`memory_limit_tokens`（trunk token 预算）、`skills_dir`、多 API profile。
- `[plugins.system_prompt]`：全局系统提示词（Panel 中编辑保存的基础提示词层）。
- `[adapters.qq]`：QQ 适配器开关、NapCat HTTP API、owner_qq、命令前缀；`[adapters.qq.server]` 反向 WS 监听 `:3131` 与访问令牌（`ECHO_ACCESS_TOKEN` env 可覆盖）。
- `[core] management_address`：前端连接地址（默认 `127.0.0.1:3132`）。

前端通过设置视图（API/技能/工具/插件/智能体）与 QQ 管理面板发起的修改，由 Core 经 `ConfigStore` 原子写回本文件（见 [document/core-config-persistence.md](document/core-config-persistence.md)）。

## 前端协议

Core 与前端之间是 `ws://<management_address>` 上的 JSON 文本帧协议，类型定义在 `echo-protocol` crate，完整契约见 [document/protocol.md](document/protocol.md)。serde 表示即线格式，向后兼容演进（新增字段必须 `#[serde(default)]`）。

## 文档索引

文档群按 [ProDoc](https://github.com/EchoLab-Auto/DocRenderer) 规范组织（文档图模型：`echo-prodoc view document/` 可视化浏览）。入口 [document/index.md](document/index.md)。

| 文档 | 内容 |
|---|---|
| [document/architecture.md](document/architecture.md) | 架构总览（crate、接缝、事件、会话、扩展点） |
| [document/protocol.md](document/protocol.md) | 前端 ⇄ Core 线协议契约 |
| [document/ops-deploy.md](document/ops-deploy.md) | 一键安装、systemd 服务、受控自更新 |
| [document/core.md](document/core.md) | Core 框架（进程结构、会话记忆、配置） |
| [document/panel.md](document/panel.md) | Panel 前端（仓库布局、数据流、视图规范） |
| [document/core-subagent.md](document/core-subagent.md) | Subagent 插件（异步委派、隔离上下文、完成回报） |
| [document/core-config-persistence.md](document/core-config-persistence.md) | ConfigStore 原子持久化 |
| [document/adapter-qq-gating.md](document/adapter-qq-gating.md) | QQ 5 层门控管道 |
| [document/dev-testing.md](document/dev-testing.md) | 测试策略 |

## 从 EchoAgentPanel 单体仓库迁移

本仓库由 EchoAgentPanel 单体仓库拆分而来。旧部署（`echo-agent-panel-core.service`）迁移：

```bash
systemctl --user disable --now echo-agent-panel-core.service   # 停旧 Core
./scripts/install.sh                                            # 装新 Core
# 迁移旧配置：~/.config/echo-agent-panel/core.toml → ~/.config/echo-agent-core/core.toml
# Panel 端无需改动（协议兼容），升级到新 EchoAgentPanel 即可
```
