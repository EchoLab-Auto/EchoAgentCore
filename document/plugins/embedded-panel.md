---
id: embedded-panel
title: "内嵌 Panel 插件"
group: Core
x: 7040
y: 1200
---

# 内嵌 Panel 插件

> **定位**：`echo-agent.panel.web` 把 Web Panel 作为 Core 的可选进程内插件挂载；**独立 `echo-agent-panel` 部署继续支持**。两者共用 Panel 仓 `echo-web-server::runtime`，路由、静态资源、上游、SSH 隧道与联邦发现不复制。

## 两种部署，一份实现

| 形态 | 入口 | 生命周期 | 典型用途 |
|---|---|---|---|
| 独立 Panel | Panel 仓 `echo-agent-panel` | 独立 systemd 服务 | 一个 Panel 管联邦多个 Core、独立故障与升级 |
| Core 内嵌 Panel | Core `echo-plugin-panel` | 插件 mount/unmount 的 Disposer | 本机少一个进程，随 Core 一起部署 |

本机当前保留 Panel → Core management WS 的中继，**协议不变**；将来再做同进程管理面直连优化。

## 默认不开启

插件编进 Core 只代表能力存在。`plugins.toml` 必须显式声明并 `enabled = true`；否则不读配置、不 bind 端口。生产现有独立 Panel 不受影响。

```toml
[[plugin]]
id = "echo-agent.panel.web"
kind = "builtin"
name = "Web Panel"
enabled = true

[plugin.config]
bind = "127.0.0.1:8080"
static_dir = "/home/me/.local/libexec/echo-agent-panel/web"
config_file = "/home/me/.config/echo-agent-panel/panel.toml"
```

`config_file` 是可选的：不指定时是 `core`（默认 `ws://127.0.0.1:3132`）的单上游形态；指定时复用它的 `[[cores]]`、SSH 和 `[federation]`，插件 bind/static_dir 优先。

## 凭证与失败边界

- 浏览器访问令牌用 `access_token_secret_file`，Core 上游令牌用 `core_access_token_secret_file`。
- **不要把 token 放进 `[plugin.config]`**：它会进入插件诊断面。
- 默认只允许 loopback。对外监听必须同时配置 `allow_remote = true` 与访问令牌 secret 文件，否则 mount 失败。
- 端口占用、配置错误、静态目录缺失会使挂载明确失败，不会假装启动。
- 禁用/卸载停止 HTTP listener 和探活任务；独立 Panel 的停止路径相同。
