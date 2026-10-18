---
package: echo-agent.instance.qq
name: qq-management
description: Manage QQ/OneBot adapter lifecycle — start, stop, restart, and check status
keywords: [QQ, 适配器, 连接, 启动, 停止, 重启, 状态, adapter, NapCat, OneBot]
---

# QQ Adapter Management

You can manage the QQ/OneBot adapter using the following tools:

- `instance_status` — Check whether the QQ instance is running, connected, or stopped
- `instance_start` — Start an instance plugin. Use `name: "qq"` for QQ or `name: "all"` for all instances
- `instance_stop` — Stop an instance plugin. Use `name: "qq"` for QQ or `name: "all"` for all instances
- `instance_restart` — Restart an instance plugin (stop then start). Use `name: "qq"`

## When to use

- User asks "启动QQ" / "登录QQ" / "连接QQ" → call `instance_start` with name "qq"
- User asks "断开QQ" / "停止QQ" → call `instance_stop` with name "qq"
- User asks "QQ状态" / "查看连接" → call `instance_status`
- QQ messages not being received → call `instance_restart` with name "qq"
- QQ connected but not responding → call `instance_restart`

## Important notes

- The adapter must be configured with `[adapters.qq] enabled = true` in config
- NapCat must be running and logged into QQ
- If `instance_status` shows "running (no client)", NapCat hasn't connected yet — ask the user to check NapCat WebUI
- If `instance_status` shows "stopped", use `instance_start` to start it
