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
- `run_tool` 统一分派：编排工具（定时器/子代理/后台任务/自更新/sudo）由 agent 内联处理；普通工具走注册表

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
- 外圈守卫 = `max(tool_timeout_secs（默认 120s）, hint + 15s)`（自声明硬上限 600s；`run_sudo` 特判为授权+执行双超时之和 + 30s，`present_menu` 特判为等待窗口 + 30s——守卫必须长过用户的思考时间，否则会在选择前把 future drop 掉）
- hint 计算在 `agent/tool_exec.rs` 的 `tool_timeout`（agent 循环只保留
  `run_sudo`/`present_menu` 两个编排工具特判）
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

## present_menu：人机交互选单（menu 插件）

让 agent 在 Panel 里向用户发起**选单**（2-10 个选项），用户选定后再继续下一步：
模型提出选项 → 用户在 Panel 点选 → 选择结果作为工具结果喂回同一次 turn 的上下文。

- 链路：`present_menu` 向 `MenuBroker` 注册未决请求 → 发 `MenuRequest` 事件（标题/说明/选项/超时）→ 带超时等待 oneshot；management server 收到 `menu_answer` 帧后**直接** `broker.submit`——不经 agent 命令队列（应答不是"新输入"，不开启新 turn），也不进会话日志
- Panel 呈现：**会话区内联卡片**（`MenuCard`，追加在消息流尾部，按 `session_id` 归属过滤；非弹窗，见 [模态与覆盖层](./panel-modals.md)§8.1b）
- 结果语义（喂回模型的工具结果）三态分明：**选定**返回 `用户选择了「label」（id: …）——description。请据此继续下一步`；**取消**（用户 Esc/点取消）返回"用户取消了选单（未做任何选择）"，提示模型不要当成选项；**超时/中断**返回错误文本
- 选项归一化：`id` 可省略（按 1-based 位置生成 `"1".."n"`），显式 id 必须唯一非空；`title`、`label` 必填。重复 id / 数量越界（<2 或 >10）/ 未知字段在参数预检阶段拒绝
- 等待窗口：`MENU_WAIT_TIMEOUT_SECS = 300s`（`menu.rs` 常量）；`MenuResolvedGuard` 的 Drop 兜底保证选定/取消/超时/中断**恰好一次** `MenuResolved`——Panel 选单卡片不会挂在死请求上（与 sudo guard 同模式）
- 门控：普通编排工具级白/黑名单（`enabled_tools` / `disabled_tools`，与其他编排工具同层）；插件维度 `echo-agent.menu` 已于 2026-09 移除（原白名单条目在配置加载期由 `normalize_mode_plugins` 剔除）。schema 随 turn 重建，配置改动即时生效
- 局限：选单是 **Panel 侧交互**——QQ 会话里用户看不到选单卡片，工具描述明确要求改用文字询问；无 Panel 在线时请求超时失败（与 sudo 同语义）

## run_sudo：人机交互 sudo 授权

让 agent 能执行需 root 的命令，同时满足硬约束：**每次 sudo 都由用户在 Panel 输入密码授权；密码绝不进入日志、LLM 上下文与会话日志；模型不知道密码内容**。

- 链路：`run_sudo` 向 `SudoBroker` 注册 pending 请求 → 发 `SudoRequest` 事件 → 带超时等待 oneshot；management server 收到 `SudoPassword` 帧后**直接** `broker.submit`——不经 agent 命令队列、不进会话日志（结构性带外通道）
- 拿到密码后 `sudo -S -p '' -- sh -c <command>` 把密码写入 stdin 管道，缓冲区立即零化；只有 stdout/stderr 返回给模型；`PendingSudo` Drop 时取消 broker 条目（外层超时也不泄漏）
- 密码只在「Panel 输入框 → WS 帧 → broker oneshot → sudo stdin」四个暂存点间流转，随后零化；密码从未 model-visible，"模型可见 ⟺ 已记录"不变量不受影响
- 配置：`[agent.sudo] enabled`（模板默认开启、代码默认关闭）、`auth_timeout_secs = 120`、`command_timeout_secs = 60`；`run_sudo` 仅在 enabled 时进入 schema
- `bash` 检测到开头 `sudo` 时提示改用 `run_sudo`
- 已知限制：密码每次请求输入（不做 credential 缓存）；多个并发 sudo 请求时 Panel 只展示最新一个（旧请求超时失败）；无 Panel 在线时请求超时失败（安全降级）
- 协议帧与打码约定见 [协议与数据流](./protocol.md)；systemd `NoNewPrivileges` 配合见 [部署与自更新](./ops-deploy.md)
