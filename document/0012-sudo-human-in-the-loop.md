---
id: adr-0012
title: "ADR-0012 sudo 人机授权"
group: 架构决策
x: 2360
y: 1056
---
# ADR-0012: 人机交互 sudo 授权（run_sudo）

状态: accepted

## 问题

agent 框架无法执行需要 root 权限的命令：`run_command` 以普通用户运行
`sh -c`，LLM 若直接写 `sudo …` 会因无 TTY/密码而失败。希望让 agent 能用
sudo，同时满足硬性安全约束：

1. 每次 sudo 都要由用户在**后台**（Panel）输入密码授权；
2. 密码**绝不进入**日志、LLM 上下文、会话日志（echo-sessions.json）；
3. agent（模型）**不知道密码内容**，只"使用"sudo。

## 决策

新增 `run_sudo` 编排工具 + `SudoBroker`，密码走**专用带外通道**：

- **协议**（echo-protocol）：
  - `BackendEvent::SudoRequest{request_id, command, session_id}` 与
    `BackendEvent::SudoResolved{request_id, accepted, message}`（事件不含密码）；
  - `WsMessage::SudoPassword(SudoPasswordSubmit{request_id, password:
    Option<String>})`——`Some` 授权 / `None` 拒绝，仅 Panel → Core；
  - `SudoPasswordSubmit` 的 `Debug` 打码；专用
    `serialize/deserialize_sudo_password` 失败时**不记录原始文本**。
- **Core**（echo-agent `sudo.rs` + `Agent::run_sudo`）：
  - `run_sudo` 向 `SudoBroker` 注册 pending 请求 → 发 `SudoRequest` → 带超时
    等待 oneshot；management server 收到 `SudoPassword` 帧后**直接**
    `broker.submit`（不经 agent 命令队列、不进会话日志）；
  - 拿到密码后 `sudo -S -p '' -- sh -c <command>` 把密码写入 stdin 管道，
    缓冲区立即零化；stdout/stderr 才返回给模型；
  - `PendingSudo` Drop 时取消 broker 条目（外层 tool 超时也不会泄漏）。
- **Panel**（echo-tui）：
  - `Transport` seam 新增 `send_sudo_password`（WS 序列化为专用帧；进程内
    传输可选挂 `with_sudo_channel`，无通道时安全丢弃→请求超时）；
  - `SudoRequest` 打开遮罩密码弹窗（屏幕只显示 `*`），Enter 提交 / Esc 拒绝；
  - 密码从不作为聊天消息发送。
- **配置**：`[agent.sudo] enabled = true`（模板默认开启，代码默认关闭）、
  `auth_timeout_secs = 120`、`command_timeout_secs = 60`。`run_sudo` 仅在
  enabled 时进入 schema；background 分支禁用（脱离交互不应触发 sudo 弹窗）。
- **systemd 服务**：常驻 `echo-agent-core.service` 关闭 `NoNewPrivileges`
  （sudo 依赖 setuid 提权）；一次性更新器 `echo-agent-core-update.service`
  保留 `NoNewPrivileges=true`（从不运行 sudo，缩小被攻破的构建脚本的提权面）。
- `run_command` 检测到开头 `sudo` 时提示改用 `run_sudo`。

## 备选方案

- **把密码放进 BackendCommand**：会让密码经过 agent 命令泵，且命令枚举
  Debug/序列化面宽，容易误入日志——否决。
- **sudoers NOPASSWD / 预缓存 credential**：把 root 能力交给模型且无人工
  确认，不符合"每次授权"要求——否决。
- **让模型自己向用户要密码**：密码会进入 LLM 上下文与会话日志——否决。

## 后果

- 模型只能触发请求，密码只在 Panel 输入框 → WS 帧 → broker oneshot → sudo
  stdin 四个暂存点间流转，随后零化；任何一步都不落日志/上下文。
- 授权在 Core 侧由 management server 直接路由，结构性绕过 agent 命令队列，
  "model-visible means logged" 不变量不受影响（密码从未 model-visible）。
- 已知限制：密码是**每次请求**输入（不做 sudo credential 缓存）；多个并发
  sudo 请求时 Panel 只展示最新一个（旧请求超时失败）；无 Panel 在线时请求
  超时失败（安全降级）。
- 验证：Core 491 / Panel 183 测试全绿；密码泄漏回归测试
  （Debug 打码、序列化不落日志、工具结果不含密码、屏幕遮罩）。
