---
id: tools
title: "工具系统"
group: 后端模块
x: 970
y: 1644
link: ["plugins | 插件化设计"]
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

## 后台 Shell 会话（持久 bash）

进程级 `ShellManager`（`backend/echo-agent/src/shell.rs`）管理**持久 bash 会话**：
与 Panel Shell 面板共用同一会话，LLM 与用户看到的是同一终端上下文。

- **会话模型**：每个会话 = 一个 `bash --noprofile --norc` 子进程（stdin/stdout/stderr 管道）；
  会话串行执行（一次一条命令），不同会话并行；上限 8（`MAX_SHELL_SESSIONS`）
- **命令执行**：写入命令 + 随机哨兵标记，读 stdout/stderr 直到哨兵出现（或超时）；
  输出按行流式广播（`ShellExecOutput` 事件）；主时间线/面板终端实时可见
- **超时**：单条命令超时（默认 120s，最大 300s）只中止读取、**保留会话**；
  超时文本以 notice 语义返回（面板显示"可能仍在运行"）
- **工具**：`shell_start` / `shell_exec` / `shell_stop`（builtin 注册，按人格白名单）；
- **命令**：`RequestShellSessions` / `ShellStart` / `ShellExec` / `ShellStop`（面板直控）
- **事件**：`ShellSessionsList` / `ShellSessionStarted` / `ShellExecStarted` /
  `ShellExecOutput`（流式）/ `ShellExecDone` / `ShellSessionClosed`
- **生命周期**：`ShellStop` 销毁；进程意外退出（try_wait）自动清理并广播关闭事件；
  Core 重启后会话不保留（一次性的运行期资源）

## run_sudo：人机交互 sudo 授权

让 agent 能执行需 root 的命令，同时满足硬约束：**每次 sudo 都由用户在 Panel 输入密码授权；密码绝不进入日志、LLM 上下文与会话日志；模型不知道密码内容**。

- 链路：`run_sudo` 向 `SudoBroker` 注册 pending 请求 → 发 `SudoRequest` 事件 → 带超时等待 oneshot；management server 收到 `SudoPassword` 帧后**直接** `broker.submit`——不经 agent 命令队列、不进会话日志（结构性带外通道）
- 拿到密码后 `sudo -S -p '' -- sh -c <command>` 把密码写入 stdin 管道，缓冲区立即零化；只有 stdout/stderr 返回给模型；`PendingSudo` Drop 时取消 broker 条目（外层超时也不泄漏）
- 密码只在「Panel 输入框 → WS 帧 → broker oneshot → sudo stdin」四个暂存点间流转，随后零化；密码从未 model-visible，"模型可见 ⟺ 已记录"不变量不受影响
- 配置：`[agent.sudo] enabled`（模板默认开启、代码默认关闭）、`auth_timeout_secs = 120`、`command_timeout_secs = 60`；`run_sudo` 仅在 enabled 时进入 schema，background 分支禁用（脱离交互不应触发 sudo 弹窗）
- `run_command` 检测到开头 `sudo` 时提示改用 `run_sudo`
- 已知限制：密码每次请求输入（不做 credential 缓存）；多个并发 sudo 请求时 Panel 只展示最新一个（旧请求超时失败）；无 Panel 在线时请求超时失败（安全降级）
- 协议帧与打码约定见 [协议与数据流](./protocol.md)；systemd `NoNewPrivileges` 配合见 [部署与自更新](./ops-deploy.md)
