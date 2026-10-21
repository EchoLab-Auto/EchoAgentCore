# EchoAgentCore

可独立部署的 **Agent 核心服务**：LLM agent 循环 + 工具/技能系统 + 会话记忆 + 插件体系，
通过 management WebSocket 向前端提供统一的命令/事件协议。

平台接入（QQ / OneBot v11 等）由**独立进程插件**提供——插件崩溃不影响内核。
本仓库是 **Core（后端）**；Web 面板在独立仓库
[EchoAgentPanel](https://github.com/EchoLab-Auto/EchoAgentPanel)。

```
 NapCat (QQ)                 EchoAgentCore                 EchoAgentPanel (Web)
 ┌──────────┐  reverse WS   ┌──────────────────────┐   WS/JSON   ┌─────────────┐
 │ OneBot11 │ ◄──────────►  │ echo-agent-core :3131 │ ◄─────────► │ :3132 连接方 │
 └──────────┘   :3131       │  Agent · Tools ·      │   :3132     └─────────────┘
                            │  Skills · Memory      │
                            └──────────────────────┘
```

**能力一览**：多 agent 人格与会话隔离 · 工具/技能热插拔 · 事件溯源记忆与压缩 ·
进程外插件（崩溃隔离 + 跨语言）· Core↔Core 联邦（多机去中心化）· 脱敏与 token 治理。

---

## 快速部署

### 一键安装（systemd 用户服务，推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/install.sh | bash
```

无需先 clone：引导脚本会浅克隆仓库到临时目录、接力完整安装流程（前置依赖自动安装
→ 受管检出 → 构建 → systemd 服务）。

**已 clone 仓库时**：

```bash
./scripts/install.sh [--no-start] [--no-deps]
./scripts/install.sh --dry-run          # 先看它要做什么
```

脚本会自动检查并安装前置依赖（系统命令走 apt/dnf/pacman/zypper；Rust 工具链缺失时
走官方 rustup 用户级安装，无需 root）。`--no-deps` 只检查不安装。

**国内网络（镜像加速）**：GitHub / rustup / crates.io 在部分网络下缓慢或不可达，
`--cn-mirror` 一键启用镜像（默认 gh-proxy.com + rsproxy.cn，`=tuna` / `=ustc` 换站）：

```bash
# 一键安装
curl -fsSL https://gh-proxy.com/https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/install.sh \
    | ECHO_CN_MIRROR=1 bash

# 已 clone
./scripts/install.sh --cn-mirror
# 关闭：--no-cn-mirror；只覆盖单环节：ECHO_GITHUB_PROXY / ECHO_CRATES_MIRROR / …
```

默认全直连，每个环节失败自动回退官方端点。

### 安装后

| 路径 | 用途 |
|---|---|
| `~/.local/bin/echo-agent-core` | Core 启动器 |
| `~/.local/libexec/echo-agent-core/` | 运行二进制与 updater |
| `~/.config/echo-agent-core/core.toml` | 持久化配置 |
| `~/.local/share/echo-agent-core/source` | 受管 Git 检出（自更新源） |
| `~/.local/state/echo-agent-core/` | 更新锁、版本与状态 |
| `echo-agent-core.service` | 常驻 Core 服务 |
| `echo-agent-core-update.service` | 一次性更新器 |

首装自动生成 `management_access_token`（32 字节随机 hex）——把 management WS 暴露到
局域网开箱即有认证。服务单元设 `NoNewPrivileges=true`，agent 无提权能力。

```bash
systemctl --user status echo-agent-core     # 看状态
journalctl --user -u echo-agent-core -f     # 看日志
echo-agent-core port                        # 查看/修改监听端口
echo-agent-core token [--rotate]            # 查看/轮换管理令牌
```

### 卸载与重装

```bash
./scripts/uninstall.sh                  # 保留配置与会话历史；--purge 连配置删；--dry-run 预览
./scripts/uninstall-remote.sh | bash    # 免 clone 版本

# 重装（保留配置）
curl -fsSL https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/reinstall.sh | bash
# 以最新模板重置配置（旧 core.toml 自动备份；token 沿用）
curl -fsSL https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/reinstall.sh \
    | bash -s -- --overwrite-config
```

### Docker

```bash
npx @echolab-auto/echo-agent up      # core + panel + napcat 三容器，面板在 :8080
```

包源码见 [`npm/echo-agent/`](npm/echo-agent/)；镜像发布见 `.github/workflows/docker.yml`。

```bash
# 手动构建/运行
docker build -t echo-agent-core .
docker run -v ./config:/config -v ./data:/data -p 3131:3131 -p 3132:3132 echo-agent-core
```

镜像内置 docker CLI（可选挂 `/var/run/docker.sock`：QR 取图 / 文件桥 / NapCat 管理）。

---

## 配置

模板与内联注释：[`config/echo-agent-core.toml`](config/echo-agent-core.toml)。最常改的几项：

| 段 | 用途 |
|---|---|
| `[agent]` | provider / model / base_url / api_key（env 覆盖：`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `DEEPSEEK_API_KEY`）、记忆预算、技能目录、多 API profile |
| `[core]` | `management_address`（默认 `0.0.0.0:3132`）、`management_access_token` |
| `[adapters.qq]` | 平台实例——Core 只读 `enabled` 与启动段，**其余键原样透传插件** |
| `[federation]` | Core↔Core 联邦；**零配置默认开启**，缺省合并至管理端口 |

前端设置视图发起的修改由 Core 经 `ConfigStore` 原子写回该文件。
完整配置语义见 [部署与自更新](document/ops/ops-deploy.md) 与 [配置持久化](document/core/core-config-persistence.md)。

**开发运行**（改本仓库代码时）：

```bash
cp config/echo-agent-core.toml config/echo-agent-core.local.toml   # 填 api_key
cargo run -- --config config/echo-agent-core.local.toml
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```

---

## 文档索引

文档群按 [ProDoc](https://github.com/EchoLab-Auto/DocRenderer) 组织，入口
**[document/index.md](document/index.md)**（`echo-prodoc view document/` 可视化浏览）。

**架构与实现**（本 README 不展开，从这里进）

| 文档 | 内容 |
|---|---|
| [overview/architecture.md](document/overview/architecture.md) | 架构总览：crate 布局、能力接缝、扩展点 |
| [core/frame.md](document/core/frame.md) | 框架内核：装配、服务定位、事件总线、插件宿主、进程结构 |
| [core/core.md](document/core/core.md) | Core 后端入口：组成与运行速览 |
| [core/core-agents.md](document/core/core-agents.md) | 多 Agent 与会话：persona、能力隔离 |
| [core/core-agent-loop.md](document/core/core-agent-loop.md) | Agent 循环：turn/step 状态机 |
| [core/core-memory.md](document/core/core-memory.md) | 会话记忆：事件溯源、投影、压缩 |
| [core/core-multimodal.md](document/core/core-multimodal.md) | 多模态输入 |
| [core/core-config-persistence.md](document/core/core-config-persistence.md) | 配置原子持久化 |
| [core/security-redaction-design.md](document/core/security-redaction-design.md) | 脱敏出口卡口与保证边界 |
| [core/core-logging.md](document/core/core-logging.md) | 日志与可观测性：通道分工、打点约定、轮转 |

**扩展系统（插件 / 工具 / 技能）**

| 文档 | 内容 |
|---|---|
| [plugins/core-plugins.md](document/plugins/core-plugins.md) | 插件化设计：模型、内置清单、进程外插件 |
| [plugins/plugin-authoring.md](document/plugins/plugin-authoring.md) | **插件开发指南**（外部作者入口） |
| [plugins/core-tools.md](document/plugins/core-tools.md) | 工具系统：注册分发、路径约定、调用治理 |
| [plugins/core-skills.md](document/plugins/core-skills.md) | 技能系统：分层、SKILL.md、热重载、Git 安装 |
| [plugins/core-subagent.md](document/plugins/core-subagent.md) | Subagent 插件 |
| [plugins/qq-gating.md](document/plugins/qq-gating.md) | QQ 实例门控 |

**协议与联邦**

| 文档 | 内容 |
|---|---|
| [protocol/protocol.md](document/protocol/protocol.md) | 前端 ⇄ Core 线协议契约（`ws://<management_address>` JSON 文本帧） |
| [federation/federation.md](document/federation/federation.md) | 多机去中心化联邦：链路 / 远程工具 / 委派 / 安全模型 |

**运维与开发**

| 文档 | 内容 |
|---|---|
| [ops/ops-deploy.md](document/ops/ops-deploy.md) | 一键安装、systemd、受控自更新、回滚 |
| [dev/dev-guide.md](document/dev/dev-guide.md) | 开发指南：仓库结构、关键抽象、编码约定、门禁 |
| [dev/dev-testing.md](document/dev/dev-testing.md) | 测试策略 |

**仓库结构**：`source/` 按层分目录（`defs → context / session / loop / tool / shell →
protocol / federation → plugin → backend → core`），逐 crate 职责见
[架构总览的 crate 表](document/overview/architecture.md)。

---

## 从 EchoAgentPanel 单体仓库迁移

旧部署（`echo-agent-panel-core.service`）迁移到本仓库：

```bash
systemctl --user disable --now echo-agent-panel-core.service
mkdir -p ~/.config/echo-agent-core
cp ~/.config/echo-agent-panel/core.toml ~/.config/echo-agent-core/core.toml
```

Panel 端无需改动（协议兼容），升级到新版 EchoAgentPanel 即可。

## 许可

MIT，见 [LICENSE](LICENSE)。
