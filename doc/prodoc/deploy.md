---
id: deploy
title: "部署与自更新"
x: 400
y: 560
group: 运维
link: ["core | Core 框架", "panel | Panel 前端"]
---

# 部署与自更新

EchoAgent 以 systemd **用户服务**运行（Core + Panel 各自独立）。源码托管于受管 git 仓库，更新走 oneshot 更新单元（构建 → 原子替换 → 重启）。

## 服务与文件布局

| 组件 | 服务单元 | 二进制/静态目录 | 管理仓库 |
| --- | --- | --- | --- |
| Core | `echo-agent-core.service` | `~/.local/libexec/echo-agent-core/echo-agent-core-bin` | `~/.local/share/echo-agent-core/source` |
| Panel | `echo-agent-panel.service` | `~/.local/libexec/echo-agent-panel/{echo-agent-panel-bin,web}` | `~/.local/share/echo-agent-panel/source` |
| 更新 | `echo-agent-core-update.service` / `echo-agent-panel-update.service` | — | 同上 |

- Core 工作目录：`~/.local/share/echo-agent-core/source`；配置：`~/.config/echo-agent-core/core.toml`
- Panel 配置：`~/.config/echo-agent-panel/panel.toml`（`[core].connect_url` 指向 Core management WS）

## 更新流程（update.sh）

- `--local`：构建当前源码树（含未提交改动）；`--remote`：先 fetch + fast-forward 再构建
- **原子替换**：先备份现有二进制/静态目录到 rollback 文件，替换后重启服务并健康检查（3 次活跃确认）
- 失败自动回滚（恢复备份 + 重启）；状态写入 `~/.local/state/echo-agent-*/update-status`
- Panel 更新包含 `npm run build`（前端产物从 `web/dist` 拷贝到静态目录）

## 自更新注意事项

- **绝不** `systemctl --user stop echo-agent-core.service`：agent 本身运行在 core 进程内，stop 会杀死当前会话，后续 start 永远执行不到
- 安全替代：`systemctl --user restart echo-agent-core.service`（原子自动拉起）；确需先停后启时用 `systemd-run --user` 脱离会话执行
- 更新会优雅排空：Core 关闭时等待进行中的回复（最多 120s）再退出，会话定期保存 + 关闭时 flush

## 日常体检

- `systemctl --user status echo-agent-{core,panel}.service`
- `systemctl --user is-active echo-agent-{core,panel}.service`
- 查看更新状态：`cat ~/.local/state/echo-agent-{core,panel}/update-status`

## 回滚

更新失败时 update.sh 自动回滚；人工回滚可执行 `~/.local/libexec/echo-agent-{core,panel}/update.sh` 前保留的 rollback 文件恢复（或从 git 仓库构建旧提交）。
