---
id: tools
title: "工具系统"
group: 后端模块
x: 1283
y: 2047
---

# 工具系统

## 注册与分发

- `Tool` trait 定义在 echo-defs（Service Definition 层）；`ToolRegistry`（echo-agent）按名注册，启停热切换（`disabled_tools` 持久化于 `[agent]`）
- 注册是可逆副作用：`register_reversible` 返回 `Disposer`，插件卸载即撤销
- `run_tool` 统一分派：`spawn_subagent`（异步委派受理）由 agent 内联分派；其余工具走注册表

## 文件工具的路径约定（2026-09-24）

`read_file` / `list_files` / `search_code` / `write_file` / `edit_file` 共用同一
套路径解析（`tool/builtin/coding.rs::resolve_tool_path`）：

- **相对路径**：相对工作区（Core 进程 cwd）解析，解析结果必须落在工作区内
  （`guard_relative_path`：canonicalize 目标或最近的已存在祖先；`../..` 逃逸、
  指向区外的符号链接都拒绝，且写工具在**创建目录之前**校验，区外不留空目录）
- **绝对路径**：原样使用（显式意图）——多仓库工作流需要：agent 的工作区落在
  一个仓库（如 Core），同时要改另一个仓库（如 Panel、ui-frame）。此前
  write/edit 拒绝绝对路径，只能退回 `bash` 绕行，反而更不透明
- 相对路径限定是**防误伤**（模型手滑/提示注入导致的越界写），不是权限边界：
  `bash` 工具本身即可访问整个文件系统；需要强制隔离时应约束 `bash`/`write_file`
  的工具白名单（persona 级）

## 参数预检

- 非法 JSON / 缺少必需字段时返回**纠正性错误**（说清"你发了什么、应该发什么"），让模型自我纠正而不是盲目重试
- 实现在 `agent/tool_exec.rs`：`tool_arguments_error`（注册表 schema 查询 + 判定）/
  `invalid_tool_arguments`（纯函数：比对 schema `required` 与实参，缺失列表 +
  原始参数回显 + schema 一并写入文案——只说"command required" 会让模型原样重试，
  形成空参调用退化循环）

## 超时治理

- `Tool::timeout_hint`：工具从自己的参数自声明执行超时（如 `bash` 的 `timeout_secs`，默认 120s、上限 300s）
- 外圈守卫 = `max(tool_timeout_secs（默认 120s）, hint + 15s)`（自声明硬上限 600s）；内置循环与 echo-loop 两条驱动路径同一口径（`Agent::tool_guard_timeout`）
- hint 计算在 `agent/tool_exec.rs` 的 `tool_timeout`
- 超时只中止单个调用：`bash` 的 `sh -c` 整组进程被 SIGKILL（不留孤儿）；结果以 notice 文本喂回模型，**不中断 turn**，模型可重试或带已有信息继续作答

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
- **常驻进程规范**：agent 需要跑常驻进程（文档/开发服务器、watch 构建、本地服务
  等）时**必须经 shell 会话启动**（`shell_start` + `shell_exec`），禁止用
  `nohup`/`&`/disown 挂野进程——野进程脱离会话模型：不在 Shell 视图可见、无停止
  入口、机器重启即丢失且无人知晓；shell 会话内的常驻进程可见、可停止、输出可回读
  （技能侧同一约定见 `skills/coding/SKILL.md`「Long-running processes」）
- **命令**：`RequestShellSessions`（可选 `team_id` = 只看该 persona 的会话）/ `ShellStart` / `ShellExec` / `ShellStop`（面板直控）
- **事件**：`ShellSessionsList`（回带会话 `team_id` 归属）/ `ShellSessionStarted` / `ShellExecStarted` /
  `ShellExecOutput`（流式）/ `ShellExecDone` / `ShellSessionClosed`
- **生命周期**：`ShellStop` 销毁；进程意外退出（try_wait）自动清理并广播关闭事件；
  Core 重启后会话不保留（一次性的运行期资源）

## 已废弃工具（2026-09 移除）

`run_sudo`（人机交互 sudo 授权）、`present_menu`（人机交互选单）、
`framework_update`（受控自更新）三个内置工具及其全部配套（broker、协议事件、
配置段 `[agent.sudo]` / `[agent.self_update]`、Panel 弹窗/选单卡片）已于
2026-09-27 正式废弃移除——三者在此前的编排体系清理中已丢失派发入口，
本次把残留配套一并清光。提权执行与面板选单不再提供；自更新改经受管更新器
（`update.sh` / `echo-agent-core-update.service`）执行，见 [部署与自更新](./ops-deploy.md)。
