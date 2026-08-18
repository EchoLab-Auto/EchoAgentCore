---
name: framework-repos
description: 框架代码仓库位置 — Core 在 ~/.local/share/echo-agent-core/source，Panel 在 ~/.local/share/echo-agent-panel/source；框架自更新与代码修改一律在指定目录进行
keywords: [框架, 自更新, 更新, 仓库, 源码, 修改, framework, update, repo, source, core, panel]
---

# 框架代码仓库位置（框架自更新专用）

EchoAgent 框架由两个 Git 仓库组成，所有框架相关的修改、构建、提交、推送都必须在下面这两个受管仓库中进行，禁止在其他副本（如 /tmp 下的检查副本）里改代码。

## 仓库位置

- EchoAgentCore（后端核心）：
  `~/.local/share/echo-agent-core/source`
- EchoAgentPanel（前端面板）：
  `~/.local/share/echo-agent-panel/source`

## 目录要点

- Core 的源码在仓库内 `source/` 子目录（Rust workspace），例如：
  - `source/backend/echo-agent/src/agent/mod.rs`
  - `source/protocol/echo-protocol/src/event.rs`
- Panel 的源码在仓库内 `web/src/`（React/TS）与 `source/`（Rust 后端）
- 配置文件：
  - Core：`~/.config/echo-agent-core/core.toml`
  - Panel：`~/.config/echo-agent-panel/panel.toml`
- 运行中的服务二进制分别位于 `~/.local/libexec/echo-agent-core/` 与 `~/.local/libexec/echo-agent-panel/`（由 systemd 用户服务管理）

## 修改流程

1. 先确认仓库状态：`git -C <仓库> status`，如与 origin/main 有分歧先 rebase 再改
2. 在指定仓库内修改并本地验证：
   - Core：`cargo check --workspace`、`cargo test -p echo-agent`
   - Panel：`cd web && npm run build`
3. 提交并推送：`git -C <仓库> commit` + `git -C <仓库> push origin main`

## 生效方式

- Core 改动需要重启服务：`systemctl --user restart echo-agent-core-fixed.service`
- Panel 改动构建后刷新浏览器即可（服务托管静态目录）
- 框架自更新（framework_update 工具）也只会在这两个受管仓库的指定目录下工作

## 注意

- 当前机器的 Core 以 systemd 用户服务 `echo-agent-core-fixed.service` 运行，二进制在 /tmp 下的仅为检查副本，不要在其中修改
- 未显式要求时不要动 `~/.config` 之外的全局配置
