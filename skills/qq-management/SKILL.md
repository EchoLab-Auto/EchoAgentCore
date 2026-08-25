---
package: echo-agent.adapter.qq
name: qq-management
description: Manage QQ/OneBot adapter lifecycle — start, stop, restart, and check status
keywords: [QQ, 适配器, 连接, 启动, 停止, 重启, 状态, adapter, NapCat, OneBot]
---

# QQ Adapter Management

You can manage the QQ/OneBot adapter using the following tools:

- `adapter_status` — Check whether the QQ adapter is running, connected, or stopped
- `adapter_start` — Start an adapter. Use `name: "qq"` for QQ or `name: "all"` for all adapters
- `adapter_stop` — Stop an adapter. Use `name: "qq"` for QQ or `name: "all"` for all adapters
- `adapter_restart` — Restart an adapter (stop then start). Use `name: "qq"`

## When to use

- User asks "启动QQ" / "登录QQ" / "连接QQ" → call `adapter_start` with name "qq"
- User asks "断开QQ" / "停止QQ" → call `adapter_stop` with name "qq"
- User asks "QQ状态" / "查看连接" → call `adapter_status`
- QQ messages not being received → call `adapter_restart` with name "qq"
- QQ connected but not responding → call `adapter_restart`

## Important notes

- The adapter must be configured with `[adapters.qq] enabled = true` in config
- NapCat must be running and logged into QQ
- If `adapter_status` shows "running (no client)", NapCat hasn't connected yet — ask the user to check NapCat WebUI
- If `adapter_status` shows "stopped", use `adapter_start` to start it
