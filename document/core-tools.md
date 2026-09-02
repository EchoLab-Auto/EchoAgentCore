---
id: tools
title: "工具系统"
group: 后端模块
x: 970
y: 1673
---

# 工具系统

## 注册与分发

- `Tool` trait 定义在 echo-defs（Service Definition 层）；`ToolRegistry`（echo-agent）按名注册，启停热切换（`disabled_tools` 持久化于 `[agent]`）
- 注册是可逆副作用：`register_reversible` 返回 `Disposer`，插件卸载即撤销
- `run_tool` 统一分派：编排工具（定时器/子代理/后台任务/自更新/sudo）由 agent 内联处理；普通工具走注册表

## 参数预检

- 非法 JSON / 缺少必需字段时返回**纠正性错误**（说清"你发了什么、应该发什么"），让模型自我纠正而不是盲目重试

## 超时治理

- `Tool::timeout_hint`：工具从自己的参数自声明执行超时（如 `run_command` 的 `timeout_secs`，默认 120s、上限 300s）
- 外圈守卫 = `max(tool_timeout_secs（默认 120s）, hint + 15s)`（自声明硬上限 600s；`run_sudo` 特判为授权+执行双超时之和 + 30s）
- 超时只中止单个调用：`run_command` 的 `sh -c` 整组进程被 SIGKILL（不留孤儿）；结果以 notice 文本喂回模型，**不中断 turn**，模型可重试或带已有信息继续作答

## 事件与持久化

- `ToolCall` / `ToolResult` 都写入事件日志（事件溯源），重启后可完整重放
- 协议事件携带 `tool_call_id` + `timed_out`：前端精确配对（同名并行调用不再配错对），超时渲染为失败；显示时间线就地更新会重新投递完成态（见 [协议与数据流](./protocol.md)）

## run_sudo：人机交互 sudo 授权

让 agent 能执行需 root 的命令，同时满足硬约束：**每次 sudo 都由用户在 Panel 输入密码授权；密码绝不进入日志、LLM 上下文与会话日志；模型不知道密码内容**。

- 链路：`run_sudo` 向 `SudoBroker` 注册 pending 请求 → 发 `SudoRequest` 事件 → 带超时等待 oneshot；management server 收到 `SudoPassword` 帧后**直接** `broker.submit`——不经 agent 命令队列、不进会话日志（结构性带外通道）
- 拿到密码后 `sudo -S -p '' -- sh -c <command>` 把密码写入 stdin 管道，缓冲区立即零化；只有 stdout/stderr 返回给模型；`PendingSudo` Drop 时取消 broker 条目（外层超时也不泄漏）
- 密码只在「Panel 输入框 → WS 帧 → broker oneshot → sudo stdin」四个暂存点间流转，随后零化；密码从未 model-visible，"模型可见 ⟺ 已记录"不变量不受影响
- 配置：`[agent.sudo] enabled`（模板默认开启、代码默认关闭）、`auth_timeout_secs = 120`、`command_timeout_secs = 60`；`run_sudo` 仅在 enabled 时进入 schema，background 分支禁用（脱离交互不应触发 sudo 弹窗）
- `run_command` 检测到开头 `sudo` 时提示改用 `run_sudo`
- 已知限制：密码每次请求输入（不做 credential 缓存）；多个并发 sudo 请求时 Panel 只展示最新一个（旧请求超时失败）；无 Panel 在线时请求超时失败（安全降级）
- 协议帧与打码约定见 [协议与数据流](./protocol.md)；systemd `NoNewPrivileges` 配合见 [部署与自更新](./ops-deploy.md)
