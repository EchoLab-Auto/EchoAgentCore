# EchoAgentCore

可独立部署的 **Agent 核心服务**：LLM agent 循环 + 工具/技能系统 + 会话记忆 + 模块体系，
通过 management WebSocket 向前端提供统一的命令/事件协议。

平台接入（QQ / OneBot v11 等）由**独立进程模块**提供——模块崩溃不影响内核。
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
进程外模块（崩溃隔离 + 跨语言）· Core↔Core 联邦（多机去中心化）· 脱敏与 token 治理。

---

## 快速部署

三种方式，按"机器上有什么"选：

| 方式 | 需要什么 | 适合 |
|---|---|---|
| [源码安装](#一键安装systemd-用户服务推荐) | rust / cargo / git（脚本会自动装）+ systemd | 想跟代码走、要自更新 |
| [二进制安装](#二进制安装不装编译链) | 只要 systemd | 新机器、不想装编译链 |
| [Docker 镜像编排](#docker官方镜像编排) | 只要 docker | 隔离部署、多实例 |

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

### 二进制安装（不装编译链）

新机器上不想为了跑服务装一整套 Rust 工具链时，直接下载预构建产物：

```bash
curl -fsSL https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/install-binary.sh | bash
```

不需要 rust / cargo / git / cc，只要 `curl` `tar` `sha256sum` `systemd`。产物由
[`.github/workflows/release.yml`](.github/workflows/release.yml) 在 tag `v*` 上构建，
每个架构一个 tarball 加 `.sha256` 侧车。

```bash
# 指定版本 / 本地 tarball / 先看它要做什么
curl -fsSL .../install-binary.sh | bash -s -- --version v0.2.0
./scripts/install-binary.sh --artifact ./echo-agent-core-0.2.0-linux-x86_64.tar.gz
./scripts/install-binary.sh --dry-run
```

**装完即带 Web Panel。** Panel 的模块代码编译在 Core 二进制里，安装器只额外落一个
前端目录（`~/.local/libexec/echo-agent-core/web`），内嵌模块自动发现它。浏览器开
`http://127.0.0.1:8080` 就是面板，无需额外配置。

**与源码安装的差异**：没有源码检出，所以不装 updater 单元，`modules.toml` 里
`echo-agent.selfupdate` 被关掉，更新方式是重跑安装器（幂等，配置保留）。两种装法
布局一致，`uninstall.sh` 通用，也可以原地互换。

**网络受限**（直连 GitHub 慢或不通）：

```bash
curl -fsSL .../install-binary.sh \
    | bash -s -- --mirror https://ghfast.top/https://github.com
```

**系统要求**：x86_64 需 glibc ≥ 2.34，aarch64 需 ≥ 2.39。安装器读产物
`manifest.json` 里的基线做预检，不匹配直接拒绝（比启动时报 `GLIBC_2.xx not found`
清楚）；`--allow-glibc-mismatch` 可强行尝试。

### 在 Docker 里二进制安装

要点只有一条：**容器必须跑 systemd**。普通 `docker run ubuntu:24.04` 没有 init，
脚本会在 `systemctl` 检查处拒绝（服务管不起来，装了也用不了）。

```bash
docker run -d --name echo-agent \
  --privileged --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock \
  -p 8080:8080 \
  ubuntu:24.04 /sbin/init
```

然后用 `--scope system` 安装（容器里没有 user manager，用户作用域起不来）：

```bash
docker exec echo-agent bash -c '
  apt-get update -qq && apt-get install -y -qq curl ca-certificates &&
  curl -fsSL https://raw.githubusercontent.com/EchoLab-Auto/EchoAgentCore/main/scripts/install-binary.sh \
  | bash -s -- --scope system'
```

国内网络末尾再加 `--mirror https://ghfast.top/https://github.com`。

**要从容器外访问面板，还得改配置**：内嵌 Panel 默认只监听 `127.0.0.1:8080`，而
docker 的端口映射是往容器 IP 转发、不会落到容器的回环上。非回环地址必须**显式放行
并配令牌**，否则模块拒绝启动（安全边界，不会静默裸奔）：

```bash
docker exec echo-agent sh -c '
  openssl rand -hex 32 > /root/.config/echo-agent-core/panel-token
  chmod 600 /root/.config/echo-agent-core/panel-token
  cat >> /root/.config/echo-agent-core/modules.toml <<EOF

[module.config]
bind = "0.0.0.0:8080"
allow_remote = true
access_token_secret_file = "/root/.config/echo-agent-core/panel-token"
EOF
  systemctl restart echo-agent-core'
```

令牌文件内容即浏览器要输入的访问令牌（至少 16 位，短于此会被拒绝）。

漏配 `allow_remote` 时 Core 起不来，且因为 `Restart=always` 会一直重启（`systemctl
is-active` 显示 `activating`）——原因在日志里：

```bash
journalctl -u echo-agent-core -n 20 | grep "内嵌 Panel"
# 内嵌 Panel 的 bind "0.0.0.0:8080" 不是回环地址；要对外暴露请同时配置
# access_token_secret_file 与 allow_remote=true
```

验证（默认回环、未配令牌时）：

```bash
docker exec echo-agent systemctl is-active echo-agent-core
docker exec echo-agent curl -s http://127.0.0.1:8080/api/panel-identity
# {"panel_key_set":false,"discover":false,"embedded":true}
```

配了令牌之后同一路径要带 Bearer，否则是 401：

```bash
TOKEN=$(docker exec echo-agent cat /root/.config/echo-agent-core/panel-token)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/panel-identity
```

> 这条路径适合"在容器里跑一个完整的、带 systemd 的 Core"，与上面的
> `npx @echolab-auto/echo-agent`（编排官方镜像、无 systemd 依赖）是两种不同的用法。

### 安装后

| 路径 | 用途 |
|---|---|
| `~/.local/bin/echo-agent-core` | Core 启动器 |
| `~/.local/libexec/echo-agent-core/` | 运行二进制；**二进制安装**额外含 `web/`（内嵌面板前端） |
| `~/.config/echo-agent-core/core.toml` | 持久化配置 |
| `~/.local/share/echo-agent-core/source` | 受管 Git 检出（自更新源，**仅源码安装**） |
| `~/.local/state/echo-agent-core/` | 更新锁、版本与状态 |
| `echo-agent-core.service` | 常驻 Core 服务 |
| `echo-agent-core-update.service` | 一次性更新器（**仅源码安装**） |

首装自动生成 `management_access_token`（32 字节随机 hex）——把 management WS 暴露到
局域网开箱即有认证。服务单元设 `NoNewPrivileges=true`，agent 无提权能力。

```bash
systemctl --user status echo-agent-core     # 看状态（system 作用域去掉 --user）
journalctl --user -u echo-agent-core -f     # 看日志
echo-agent-core port                        # 查看/修改监听端口
echo-agent-core token [--rotate]            # 查看/轮换管理令牌
echo-agent-core --version                   # 版本（与 release tag 一致）
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

### Docker（官方镜像编排）

```bash
npx @echolab-auto/echo-agent up      # core + panel + napcat 三容器，面板在 :8080
```

这条路径不依赖 systemd（容器只跑 Core 进程）。若你想在容器里跑一个带完整
systemd 的实例、并自己管服务，见上面的[在 Docker 里二进制安装](#在-docker-里二进制安装)。

包源码见 [`npm/echo-agent/`](npm/echo-agent/)；镜像发布见 `.github/workflows/docker.yml`。

```bash
# 手动构建/运行
docker build -t echo-agent-core .
docker run -v ./config:/config -v ./data:/data -p 3131:3131 -p 3132:3132 echo-agent-core
```

镜像内置 docker CLI（可选挂 `/var/run/docker.sock`：QR 取图 / 文件桥 / NapCat 管理）。

镜像**自带 QQ 适配器模块**（`echo-adapter-qq`）：适配器自 2026-10 起是进程外模块
（独立仓 [`agent-module-adapter-qq`](https://github.com/EchoLab-Auto/agent-module-adapter-qq)），
Core 按平台名派生去 PATH 里找它，装成派生名后容器模板不需要写 `module.command`。
需要可复现构建时固定适配器版本：

```bash
docker build --build-arg ADAPTER_REF=<commit-or-tag> -t echo-agent-core:local .
```

---

## 配置

模板与内联注释：[`config/echo-agent-core.toml`](config/echo-agent-core.toml)。最常改的几项：

| 段 | 用途 |
|---|---|
| `[agent]` | provider / model / base_url / api_key（env 覆盖：`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `DEEPSEEK_API_KEY`）、记忆预算、技能目录、多 API profile |
| `[core]` | `management_address`（默认 `0.0.0.0:3132`）、`management_access_token` |
| `[adapters.qq]` | 平台实例——Core 只读 `enabled` 与启动段，**其余键原样透传模块** |
| `[federation]` | Core↔Core 联邦；**零配置默认开启**，缺省合并至管理端口 |

前端设置视图发起的修改由 Core 经 `ConfigStore` 原子写回该文件。
完整配置语义见 [部署与自更新](document/ops/deploy.md) 与 [配置持久化](document/core/config-persistence.md)。

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
| [core/frame.md](document/core/frame.md) | 框架内核：装配、服务定位、事件总线、模块宿主、进程结构 |
| [core/core.md](document/core/core.md) | Core 后端入口：组成与运行速览 |
| [core/agents.md](document/core/agents.md) | 多 Agent 与会话：persona、能力隔离 |
| [core/agent-loop.md](document/core/agent-loop.md) | Agent 循环：turn/step 状态机 |
| [core/memory.md](document/core/memory.md) | 会话记忆：事件溯源、投影、压缩 |
| [core/multimodal.md](document/core/multimodal.md) | 多模态输入 |
| [core/config-persistence.md](document/core/config-persistence.md) | 配置原子持久化 |
| [core/security-redaction.md](document/core/security-redaction.md) | 脱敏出口卡口与保证边界 |
| [core/logging.md](document/core/logging.md) | 日志与可观测性：通道分工、打点约定、轮转 |

**扩展系统（模块 / 工具 / 技能）**

| 文档 | 内容 |
|---|---|
| [modules/modules.md](document/modules/modules.md) | 模块化设计：模型、内置清单、进程外模块 |
| [modules/module-authoring.md](document/modules/module-authoring.md) | **模块开发指南**（外部作者入口） |
| [modules/tools.md](document/modules/tools.md) | 工具系统：注册分发、路径约定、调用治理 |
| [modules/skills.md](document/modules/skills.md) | 技能系统：分层、SKILL.md、热重载、Git 安装 |
| [modules/subagent.md](document/modules/subagent.md) | Subagent 模块 |
| [modules/qq-gating.md](document/modules/qq-gating.md) | QQ 实例门控 |

**协议与联邦**

| 文档 | 内容 |
|---|---|
| [protocol/protocol.md](document/protocol/protocol.md) | 前端 ⇄ Core 线协议契约（`ws://<management_address>` JSON 文本帧） |
| [federation/federation.md](document/federation/federation.md) | 多机去中心化联邦：链路 / 远程工具 / 委派 / 安全模型 |

**运维与开发**

| 文档 | 内容 |
|---|---|
| [ops/deploy.md](document/ops/deploy.md) | 一键安装、systemd、受控自更新、回滚 |
| [dev/guide.md](document/dev/guide.md) | 开发指南：仓库结构、关键抽象、编码约定、门禁 |
| [dev/testing.md](document/dev/testing.md) | 测试策略 |

**仓库结构**：`source/` 按层分目录（`defs → context / session / loop / tool / shell →
protocol / federation → module → backend → core`），逐 crate 职责见
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
