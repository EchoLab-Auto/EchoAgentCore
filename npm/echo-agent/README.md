# @echolab-auto/echo-agent

EchoAgentCore + Panel + NapCat 的 **Docker 一键部署 CLI**。

它不做"原生二进制分发"，而是编排三个官方镜像：本包只负责生成
`docker-compose.yml` + 配置模板，并代理 `docker compose` 的日常操作
（npm 仅作安装/管理入口，实际运行全部在容器里）。

## 快速开始

```bash
npx @echolab-auto/echo-agent up
```

命令会：

1. 在 `~/.echo-agent` 生成部署文件（首次运行时自动 `init`）；
2. `docker compose up -d` 启动三个容器：

| 容器 | 作用 | 入口 |
|---|---|---|
| `echo-agent-core` | Agent 核心（适配器 / 会话 / 工具） | 内部 `core:3131`（NapCat 反向 WS）· `core:3132`（Panel 管理面） |
| `echo-agent-panel` | Web 面板（Rust 后端 + 前端） | <http://localhost:8080> |
| `napcat` | QQ 协议端 | <http://localhost:6099>（首次扫码登录） |

3. 打印面板地址与扫码地址。

**首次使用前**先填 LLM Key 再启动（或启动后在面板设置里填）：

```bash
echo-agent init        # 若尚未初始化
$EDITOR ~/.echo-agent/config/core.toml   # [agent] api_key = "sk-..."
echo-agent up
```

## 命令

| 命令 | 说明 |
|---|---|
| `echo-agent init` | 生成部署文件（compose + 配置模板 + `.env`）；已存在默认保留，`--force` 覆盖 |
| `echo-agent up` | 拉取镜像并启动（未初始化自动初始化） |
| `echo-agent down` | 停止并移除容器（保留配置与数据） |
| `echo-agent restart` | 重启所有容器（改配置后生效） |
| `echo-agent update` | `compose pull` + 重建容器（升级版本） |
| `echo-agent logs [服务]` | 日志（`-f` 跟随；服务名 `core` / `panel` / `napcat`） |
| `echo-agent status` | 容器状态 |
| `echo-agent doctor` | 环境自检：docker / daemon / compose / 端口 / 容器名冲突 |
| `echo-agent version` | 版本 |

全局选项：`--dir <路径>`（默认 `$ECHO_AGENT_HOME` 或 `~/.echo-agent`）。

## 部署目录结构

```text
~/.echo-agent/
├── docker-compose.yml   # core + panel + napcat（可编辑）
├── .env                 # 镜像引用（默认 ghcr；本地构建改 :local）
├── config/
│   ├── core.toml        # Core 配置：LLM Key、QQ 适配器、下载目录
│   └── panel.toml       # Panel 配置：监听 / Core 地址 / 媒体目录
└── data/                # 持久数据：媒体库（图片）、文件下载、NapCat 编排
```

配置修改后执行 `echo-agent restart` 生效（Core 启动时读取配置）。

## 与宿主安装（install.sh）的关系

| | npm + Docker（本包） | `install.sh`（宿主） |
|---|---|---|
| 运行位置 | 全部容器 | systemd 用户服务 + NapCat 容器 |
| 依赖 | Docker + Node（仅安装时） | Linux + systemd + Docker |
| 适用 | 任何有 Docker 的平台（含 macOS / Windows） | Linux 服务器 / 开发机 |

两者共用一个数据模型（同一个 core.toml / panel.toml 语义），但**不要同时跑**：
容器名（`napcat` 等）与端口（8080/6099）会冲突，`echo-agent doctor` 会给出提示。

## 本地构建镜像（无预发布镜像时）

```bash
# 在 EchoAgentCore / EchoAgentPanel 仓库根目录：
docker build -t echo-agent-core:local  .
docker build -t echo-agent-panel:local .

# 修改部署目录的 .env：
ECHO_CORE_IMAGE=echo-agent-core:local
ECHO_PANEL_IMAGE=echo-agent-panel:local

echo-agent up
```

## 常见问题

- **容器起不来 / 名字冲突**：`echo-agent doctor`（检测已有 `napcat` 等容器），
  先停掉旧部署再 `up`。
- **QQ 不回复**：确认 `napcat` 已扫码登录（<http://localhost:6099>），
  且 `core.toml` 的 `[adapters.qq] enabled = true`。
- **面板图片 404**：`panel.toml` 的 `[server] media_dir` 与 compose 注入的
  `ECHO_MEDIA_DIR` 必须指向同一卷（默认都是 `/data/media`，改一个要同步另一个）。
- **改了配置没生效**：`echo-agent restart`。
- **需要宿主侧访问管理面**：把 compose 里 core 的 `3132:3132` 打开，
  并同时在 `core.toml` 设置 `management_access_token`（随机长串）。
