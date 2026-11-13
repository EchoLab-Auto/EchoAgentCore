---
id: shell
title: "Shell 会话模块"
group: Core
x: 648
y: 216
---

# Shell 会话模块

> **定位**：`echo-agent.shell` **这个模块本身**——它为什么从内置工具集里拆出来、贡献哪些工具与命令、宿主必须给它哪些端口、启停与门控怎么走。Shell 会话的运行时机制（进程组生命周期、哨兵分帧、超时与命令护栏）**不在这里**，见 [工具系统](./tools.md)「后台 Shell 会话」。读者：改 shell 工具、或排查「关了 shell 却还能跑」这类问题的开发者。

## 为什么是独立模块

它此前和 `bash-*` 一起住在 `agent-module-tools`（6000 行的内置工具集）里，拆分理由是两者只共享「都是模型能调用的工具」这一点——而这一点几乎不构成理由：

| 维度 | 内置工具集 | Shell 会话 |
|---|---|---|
| 机制依赖 | 纯函数 + 可选 `agent-module-shell` | **必须** `agent-module-shell`（进程组生命周期） |
| 状态 | 无（除 checklist 的会话内状态） | 长驻子进程 + 哨兵分帧 |
| 面板 | 工具列表里几行 | 独立的 Shell 页面（会话卡 / 实时流） |
| 关闭需求 | —— | 「我不要 shell」不该连计算器一起关 |

于是它成为自己的模块：想关 shell 只关它，想换实现（例如换成容器内执行、换成受限沙箱）只换它。宿主的适配器一行不改——`ShellToolProvider` 是 trait，模块换实现即换行为。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.shell` |
| kind | `tool`（persona 白名单参与） |
| 名称 / 路由 | 「Shell 会话」/ `shell` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-shell`（4 文件，约 900 行） |

它贡献三类东西，**都在模块自己的 crate 里定义**，宿主只提供端口：

| 贡献 | 内容 |
|---|---|
| 服务键 `"shell"` | `mount` 时 provide 一个 `Arc<dyn ShellToolProvider>`；组合根建每个 agent 的工具表时解析它，调 `registry(&ShellHostCtx)` 拿工具箱 |
| 工具 × 3 | `bash-start` / `bash-exec` / `bash-stop`（注册时打上包标签 `echo-agent.shell`） |
| 命令域 × 4 | `RequestShellSessions` / `ShellStart` / `ShellExec` / `ShellStop` |
| 服务依赖 | `echo-agent.host.shell`（`SHELL_HOST`，声明在 `inject()` 里） |

**工具由模块造、命令由模块认领**是全模块化 S7 之后的统一形态：宿主不再知道「有哪些 shell 工具」，它只知道「这个键上有一个 provider」。

## 宿主接缝

模块向宿主只索取两样东西，装在一个结构体里交进来：

| 字段 | 类型 | 谁提供 |
|---|---|---|
| `shell` | `ShellHandle`（`agent-module-shell` 的会话管理器句柄） | 组合根，与 Panel 的 Shell 页面**共用同一个进程级实例** |
| `guard` | `Option<CommandGuard>` | 组合根按 `[agent]` 配置装配；`None` = 不做该检查 |

命令域则走另一条路：`mount` 时把宿主实现 `ShellCommandHost` 的适配器绑定到 `SHELL_HOST` 键，处理命令时取用——会话管理器、事件出口、归属人格、进度回调全在这一个 trait 后面。

键名值得记一笔：2026-11 之前是 `echo-agent.host.tools.shell`，挂在 `tools.` 命名空间下。搬成独立模块后那个前缀会误导（本模块不提供任何 `tools.*` 能力），改成 `echo-agent.host.shell`——**同一份宿主实现，只换归属声明**。改键要两边同改（模块 `host_keys.rs` + 组合根 `host_services.rs`）；漏改的表现是 `MissingService` 启动失败并列出当时可用的键。

## 启停与门控

- **挂载/卸载是一门事，「谁看得到它」是另一门事**：门控不在模块里。宿主监听 `module.mounted` / `module.unmounted` 事件统一拨 persona 可见性（`reapply_module_gating`）
- **全局开关**：面板「启用模块」取消勾选 `echo-agent.shell` → 绑定撤销 → 每个 persona 的工具表里三个 `bash-*` 消失，Panel 的 Shell 页面命令也无人处理（回明确错误，不是静默失败）
- **persona 白名单**：`kind = tool`，所以 `enabled_tools` / `enabled_modules` 都能按人格收窄——某个 agent 不给 shell 是常规用法
- **包标签**：工具注册时打 `echo-agent.shell`，因此按包启停（Package 门控）与 persona 门控作用于同一个单位

## 边界

- 它**不管**会话存活：进程组回收、孤儿清理、输出分帧都是 `agent-module-shell` 的事，模块只把工具与命令翻译成语义调用
- 它**不做**权限判定：命令护栏由宿主装配成 `CommandGuard` 传进来；模块不知道也不关心规则内容
- 与 [工具系统](./tools.md) 的分界：那边写「一次 `bash-exec` 在宿主里怎么走完生命周期」，这边写「这个能力作为一个可插拔单位长什么样」
