---
id: tools
title: "工具系统"
group: 插件
x: 960
y: 1620
link: ["tool-catalog | 内置工具目录"]
---

# 工具系统

> **定位**：本文描述工具系统——工具的注册与分派、文件工具路径约定、调用治理（参数预检 / 危险命令判定 / 超时）、事件持久化与后台 Shell 会话（含已废弃工具）。工具集作为插件（`echo-agent.tools.builtin`）的装载清单与 persona 门控见 [插件化设计](./core-plugins.md)；工具调用相关的协议事件见 [协议与数据流](../protocol/protocol.md)。读者：工具系统的使用者与维护者。


## 注册与分发

> **完整工具清单**见 [内置工具目录](./tool-catalog.md)——由
> `echo-agent::tool::builtin::register_all` **自动生成并校验**（加工具忘了
> 更新会在门禁红）。本文只讲机制与门控，不复述清单，避免两处漂移。

- `Tool` trait 定义在 echo-defs（Service Definition 层）；`ToolRegistry`（`echo-tool` crate；`echo-agent` 保留 `echo_agent::tool::…` 兼容路径）按名注册，启停热切换（`disabled_tools` 持久化于 `[agent]`）
- 注册是可逆副作用：`register_reversible` 返回 `Disposer`，插件卸载即撤销
- 分派入口有两条：turn 内走 echo-loop 的工具管线（`spawn_subagent` 经其异步 hook 通道，见 [Subagent 插件](./core-subagent.md)）；集成测试与直接调用走 `run_tool`（对 `spawn_subagent` 同样特判）。其余工具一律走注册表

### 工具从哪来（三个来源）

`ToolRegistry` 是唯一的注册面，但工具的**声明方**有三类——框架不硬编码任何具体工具：

| 来源 | 声明方式 | 归属包 | 例子 |
| --- | --- | --- | --- |
| 内置工具 | 组合根在 `main.rs` 直接 `register` | `echo-agent.tools.builtin` | `bash` / `read_file` / `write_file` / `calculator` / `web_search` |
| 子进程插件工具 | 插件 `Contribution::Tool`（`ToolContribution`）→ 宿主 `RemoteTool` 转 `Tool` | 插件自报 `package` | 试点插件 `echo-plugin-example` 的 `calculator` / `web_search` |
| **插件实例声明的工具** | 实例插件 `InstanceContribution.tools` → `PluginInstance::tool_specs()` → 宿主通用桥 | `ToolContribution.package` | QQ 的 10 个工具（见下） |

#### 实例工具：声明驱动 + 通用桥

平台插件实例对模型暴露的工具**全部由插件声明**，宿主（`core/instance_tools.rs`）只做三件与平台无关的事：

1. **合并**——同一 persona 的多个实例声明同名工具时只注册一次；
2. **寻址**——多实例时给参数 schema 注入可选 `account`，执行时按它选实例
   （`account` 是宿主参数，**不进插件 payload**；缺省且多实例时明确报错列出可选项）；
3. **过闸**——所有实例工具都跨平台边界，统一走 `OutboundGate`
   （文本命中脱敏器按策略阻断/替换；文件参数落在配置/凭据目录直接拒发）。

执行路径是 `invoke_custom(<工具名>, <参数>)`（`InstanceManagement` 透传给插件），
返回对象带 `success: false` 时转为工具错误。

**`Tool::produces_visible_output`**：工具自声明「成功调用即产生平台可见输出」。
循环用它决定是否抑制「临时等待回复」——此前是按工具名清单判断（`send_private_msg`
/ `send_group_msg` / …），等于把平台语义写进框架；现在实例工具按
`ToolContribution.visible_reply` 给值，未知工具默认 `false`。

字段全貌与声明示例见 [插件开发指南](./plugin-authoring.md)「实例插件」节；
QQ 的 10 个工具与其平台语义见 [QQ 实例门控](./qq-gating.md)。

## 文件工具的路径约定

`read_file` / `list_files` / `search_code` / `write_file` / `edit_file` 共用同一
套路径解析（`echo-plugin-tools/src/coding.rs::resolve_tool_path`）：

- **相对路径**：相对工作区（Core 进程 cwd）解析，解析结果必须落在工作区内
  （`guard_relative_path`：canonicalize 目标或最近的已存在祖先；`../..` 逃逸、
  指向区外的符号链接都拒绝，且写工具在**创建目录之前**校验，区外不留空目录）
- **绝对路径**：原样使用（显式意图）——多仓库工作流需要：agent 的工作区落在
  一个仓库（如 Core），同时要改另一个仓库（如 Panel、ui-frame）；若拒绝
  绝对路径，只能退回 `bash` 绕行，反而更不透明
- 相对路径限定是**防误伤**（模型手滑/提示注入导致的越界写），不是权限边界：
  `bash` 工具本身即可访问整个文件系统；需要强制隔离时应约束 `bash`/`write_file`
  的工具白名单（persona 级）

## 调用治理

模型可见的工具 schema 之外，调用落地前还有三道治理——参数能否执行、命令是否会
伤及系统、单次调用允许跑多久。三者都在工具内部实施，与 persona 门控（哪些工具
可用）正交。

### 参数预检

- 非法 JSON / 缺少必需字段时返回**纠正性错误**（说清"你发了什么、应该发什么"），让模型自我纠正而不是盲目重试
- 实现在 `agent/tool_exec.rs`：`tool_arguments_error`（注册表 schema 查询 + 判定）/
  `invalid_tool_arguments`（纯函数：比对 schema `required` 与实参，缺失列表 +
  原始参数回显 + schema 一并写入文案——只说"command required" 会让模型原样重试，
  形成空参调用退化循环）
- 异步编排工具（`spawn_subagent`）经 echo-loop 的 hook 通道直入管线、不经注册表
  预检，缺参由工具自身的纠正性文案兜底

### 危险命令判定

`bash` 工具（`echo-plugin-tools/src/coding.rs` 的 `RunCommandTool::execute`）
执行前过闸，命中即返回执行错误、命令不执行：

| 闸 | 实现 | 拦什么 |
| --- | --- | --- |
| 自停拦截 | `echo-agent::packages::tools_builtin::reject_core_self_stop`（**留在宿主**：它拦的是「从自己进程里停掉 Core」，属组合根/宿主职责，不在工具插件里） | 从自己的进程里 `systemctl stop/kill echo-agent-core`（含 `bash -c` 包裹）——会让后续清理/启动步骤来不及跑 |
| rm 目标判定 | `coding.rs::dangerous_rm_root_target` | 递归 + 强制的 `rm` 指向根目录、根级通配 `/*`、或根下一级系统目录 |
| 危险模式 | `coding.rs` 的模式表（`mkfs.` / `dd if=` / fork bomb / `> /dev/sda`） | 对整条命令串子串匹配 |
| 提权 | `RunCommandTool::execute` 的行首 `sudo` 检测 | `bash` 无交互 tty、提权工具已废弃——直接拒绝并提示改用无需 root 的方案 |

**rm 判定按词元与目标路径语义，不按命令串子串**（`dangerous_rm_root_target`）：

- 先按 `;` `|` `&` 与换行切段，逐词扫 `rm`（也认 `/usr/bin/rm` 这类 `/rm`
  结尾写法；词元两端引号剥掉，故 `bash -c 'rm -rf /'` 同样命中）
- 生效条件：**同时**带递归（`-r` / `-R` / `--recursive`）与强制（`-f` /
  `--force`），或显式 `--no-preserve-root`；其余情形不进判定
- 只看**目标**：去掉尾部 `/` 后等于 `/`、`/*`，或落在受保护列表
  `PROTECTED_ROOT_DIRS`（`/bin` `/boot` `/dev` `/etc` `/home` `/lib` `/lib64`
  `/opt` `/proc` `/root` `/run` `/sbin` `/srv` `/sys` `/usr` `/var`）

**误拦规避**：删除落在系统目录**之下**不受限——`rm -rf /tmp/x`、
`rm -rf /var/log/myapp`、`rm -rf /home/u/.cache/x` 放行；`cd /tmp && rm -rf x`
按段切分后目标是相对路径 `x`，同样放行；不带 `-r -f` 的 `rm /` 根本不进该判定。
（沿革：早前是对整条命令串做 `rm -rf /` 子串匹配，这类正常清理会被一并误伤。）

**边界**：这是**防误伤**判定，不是权限边界——它只认词元，不解释 shell 语义
（相对路径 `rm -rf ../`、变量展开、命令替换都不追），绕过方式存在；需要强制
隔离时用 persona 级工具白名单约束 `bash`。判定目前只覆盖 `bash`：`bash-exec`
仅有自停拦截，不经 rm 判定与危险模式表。

### 超时治理

- `Tool::timeout_hint`：工具从自己的参数自声明执行超时（如 `bash` 的 `timeout_secs`，默认 120s、上限 300s）
- 外圈守卫 = `max(tool_timeout_secs（默认 120s）, hint + 15s)`（自声明硬上限 600s）；由 turn harness 的工具执行器统一实施（`Agent::tool_guard_timeout`）
- hint 计算在 `agent/tool_exec.rs` 的 `tool_timeout`
- 超时只中止单个调用：`bash` 的 `sh -c` 整组进程被 SIGKILL（不留孤儿）；结果以 notice 文本喂回模型，**不中断 turn**，模型可重试或带已有信息继续作答

## 事件与持久化

- `ToolCall` / `ToolResult` 都写入事件日志（事件溯源），重启后可完整重放
- 异步编排工具（`spawn_subagent`）与普通工具同口径：先写 `ToolCall`（参数脱敏）+ 受理/拒绝后写 `ToolResult`（回执文本），带 `tool_call_id` 与耗时（`agent/turn.rs` 的 `execute_async_subagent`）——委派事件在日志里可查、时延可算
- 协议事件携带 `tool_call_id` + `timed_out`：前端精确配对（同名并行调用不再配错对），超时渲染为失败；显示时间线就地更新会重新投递完成态（见 [协议与数据流](../protocol/protocol.md)）

## 后台 Shell 会话（持久 bash）

进程级 `ShellManager`（机制 crate `echo-shell`；`backend/echo-agent/src/shell.rs` 只是兼容 re-export）管理**持久 bash 会话**：
与 Panel Shell 面板共用同一会话，LLM 与用户看到的是同一终端上下文。

- **会话模型**：每个会话 = 一个 `bash --noprofile --norc` 子进程（stdin/stdout/stderr 管道），
  spawn 时建立**独立进程组**（`process_group(0)`）；会话串行执行（一次一条命令），
  不同会话并行；上限 8（`MAX_SHELL_SESSIONS`）
- **命令执行**：写入命令 + 随机哨兵标记（**stdout/stderr 双写**，
  两路哨兵收齐才判命令结束——防末尾 stderr 被两管道读取调度竞争截断）；
  stdout/stderr 由**两路专职读取任务并发消费**（通道汇聚，缓冲 4096 行封顶施加
  背压），任一流空闲都不会拖住命令结束；迟到哨兵（前一条超时命令遗留）
  按前缀识别，不外泄进输出
- **输出**：按行流式广播（`ShellExecOutput` 事件，stderr 行带 `[stderr]` 前缀）；
  主时间线/面板终端实时可见
- **超时**：单条命令超时（默认 120s，最大 300s）只中止读取、**保留会话**；
  超时文本以 notice 语义返回（面板显示"可能仍在运行"）
- **工具**：`bash-start` / `bash-exec` / `bash-stop`（2026-11 起由**独立插件**
  `echo-agent.shell` 提供，按人格白名单门控；此前是 `shell_start` / `shell_exec`
  / `shell_stop`，隶属内置工具集——改名与拆分是同一轮：它们与一次性工具 `bash`
  是同一件事的两种模式，名字统一到 `bash-` 前缀后模型不再在两套命名间犹豫）
- **常驻进程规范**：agent 需要跑常驻进程（文档/开发服务器、watch 构建、本地服务
  等）时**必须经 shell 会话启动**（`bash-start` + `bash-exec`），禁止用
  `nohup`/`&`/disown 挂野进程——野进程脱离会话模型：不在 Shell 视图可见、无停止
  入口、机器重启即丢失且无人知晓；shell 会话内的常驻进程可见、可停止（`ShellStop`
  终止**整个进程组**：bash 及其全部子孙，含后台任务；自行 `setsid` 脱离进程组的
  后代除外）、输出可回读（技能侧同一约定见 `skills/coding/SKILL.md`「Long-running
  processes」）
- **跨生命周期服务**：需要**跨 Core 重启存活 / 开机自启 / 对外长期
  可达**的进程（文档站点、长期服务等）不属于会话模型——用 `systemd-run --user
  --unit=<名称>` 交给用户级 systemd 托管（`systemctl --user status/stop <名称>`
  管理）。shell 会话是运行期资源（Core 重启即回收），刻意不承担该职责
- **命令**：`RequestShellSessions`（可选 `team_id` = 只看该 persona 的会话）/ `ShellStart` / `ShellExec` / `ShellStop`（面板直控）
- **事件**：`ShellSessionsList`（回带会话 `team_id` 归属）/ `ShellSessionStarted` / `ShellExecStarted` /
  `ShellExecOutput`（流式）/ `ShellExecDone` / `ShellSessionClosed`
- **生命周期**：`ShellStop` 销毁（killpg 整组终止 + 回收直接子进程防僵尸）；进程
  意外退出（try_wait）自动清理（含残余进程组）并广播关闭事件；Core 重启后会话不
  保留（一次性的运行期资源）

## 已废弃工具

- `run_sudo`（人机交互 sudo 授权）、`present_menu`（人机交互选单）、`framework_update`（受控自更新）三个内置工具及其全部配套（broker、协议事件、配置段 `[agent.sudo]` / `[agent.self_update]`、Panel 弹窗/选单卡片）已废弃移除
- 现状：提权执行与面板选单不再提供；自更新改经受管更新器（`update.sh` / `echo-agent-core-update.service`）执行，见 [部署与自更新](../ops/ops-deploy.md)
