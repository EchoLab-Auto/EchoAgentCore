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

- Core 改动需要重启服务：`systemctl --user restart echo-agent-core.service`
- Panel 改动构建后刷新浏览器即可（服务托管静态目录）
- 框架自更新（`update.sh` / `echo-agent-core-update.service`）也只会在这两个受管仓库的指定目录下工作

## ⛔ 绝对禁止：stop Core 服务

**永远不要执行 `systemctl --user stop echo-agent-core.service`，也不要 kill core 进程。**

Agent 自己就运行在 echo-agent-core 进程内 —— stop 会立刻杀死当前会话，
命令中断在 stop 这一步，后续的 start 永远执行不到，服务就一直停止，
Panel 随之显示"与后端断开，正在重连…"（2026-08-19 已实际发生两次）。

安全替代方式：

- 让改动生效：`systemctl --user restart echo-agent-core.service`（原子操作，会自动拉起）
- 框架更新：`systemctl --user start echo-agent-core-update.service`（oneshot，内部负责构建与重启）
- 确需先停后启（如清理数据文件）：把整个操作交给脱离本会话的单元执行，例如
  `systemd-run --user --collect bash -c 'systemctl --user stop echo-agent-core.service; <操作>; systemctl --user start echo-agent-core.service'`
  绝不在会话内直接分步 stop。

## 注意

- 当前机器的 Core 以 systemd 用户服务 `echo-agent-core.service` 运行（Panel 为 `echo-agent-panel.service`），/tmp 下的仅为检查副本，不要在其中修改
- 未显式要求时不要动 `~/.config` 之外的全局配置
