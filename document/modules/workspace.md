---
id: workspace
title: "工作区会话模块"
group: Core
x: 948
y: 216
---

# 工作区会话模块

> **定位**：`echo-agent.workspace` **这个模块本身**——工作区会话的数据归属、9 条命令域与 `workspace` 工具、模块与宿主之间那条**单向数据流**（模块写 state、宿主读镜像）、以及跨机目录的降级策略。「激活工作区 = 进入项目对话通道」这件事对 agent 意味着什么，见 [多 Agent 与会话](../core/agents.md)「工作区会话与项目通道」；Panel 侧的入口行面板见 [会话入口行](../panel/chat-entry.md)。读者：改工作区行为、或排查「切了目录模型还在老地方干活」的开发者。

## 工作区是什么

工作区会话 = 用户从 Panel 管理的一组工作目录（创建 / 改名 / 删除 / 激活），可逐目录查看 `git status`、浏览文件、选择目录。激活会话的目录会注入系统提示词，让模型知道自己该在哪里干活。

## 身份与归属（2026-11 迁出 Core）

模块已从 Core 的 `source/modules/` **迁出为独立仓库**
（`EchoLab-Auto/agent-module-workspace`），Core 以 git 依赖引入：

```toml
agent-module-workspace = { git = "https://github.com/EchoLab-Auto/agent-module-workspace", branch = "main" }
```

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.workspace`（**唯一真值在模块侧**；宿主的 `WORKSPACE_MODULE_ID` 是别名） |
| kind | `tool`（persona 白名单参与） |
| 名称 / 路由 | 「工作区会话」/ `workspace` |
| 来源 / 运行形态 | 独立仓库 / 进程内（`kind = "builtin"` 行，编译进 Core 二进制） |
| 仓库 | `agent-module-workspace`（`wire` / `store` / `git` / `files` / `prompt` / `commands` / `tool` 七个模块） |

**为什么是"独立仓库 + 进程内"而不是进程外模块**：`Contribution::Command` 与
`Contribution::Text` 目前只有进程内实现（stdio 轨只消费 `Tool` /
`ModuleInstance`），而工作区同时需要命令域与提示词块——见
[模块系统](modules.md)「进程外模块的贡献支持面」。

## 组成

| 组件 | 职责 | 所在 |
|---|---|---|
| `WorkspaceStore` | 会话文档（每 persona 一个 JSON，原子写）+ 变更钩子 | 模块 |
| `workspace` 工具 | list / status / use / create / delete / conversations | 模块（宿主只留代理壳） |
| 只读采集 | `collect_dir_git` / `collect_dir_files` / `collect_child_dirs` + 沙箱解析 | 模块 |
| 通道标识 | `channel_scope_id` / `parse_workspace_channel` | 模块 |
| 状态镜像 | `WorkspaceMirror`（只读快照）+ 通道注册 + 广播 | **宿主**（`agent/workspace_mirror.rs`） |
| 工具代理 | `WorkspaceTool`（转发到模块、补 `team_id`） | **宿主**（`agent/workspace_tool.rs`） |

## 单向数据流（迁出后的核心约束）

模块是**唯一写者**，宿主只读镜像：

```
面板命令 / 模型工具
      │
      ▼
模块（store 落盘 + emit 模块事件）
      │  workspace.changed {team_id, active, active_conversation, sessions}
      ▼
宿主 workspace_mirror::WorkspaceSnapshot
      │
      ▼
宿主会话子系统：确保通道会话 + 会话级 API 恢复 + SessionUpdated / WorkspaceSessions
```

三条模块事件（宿主在 `host_services::workspace_from_module_event` 翻译）：

| 事件 | 载荷 | 宿主动作 |
|---|---|---|
| `workspace.backend_event` | 一个 `BackendEvent` JSON | 按 `team_id` 找人格出口，原样发给面板 |
| `workspace.changed` | `{team_id, active, active_conversation, sessions}` | 刷新镜像 → 确保通道 + 广播 |
| `workspace.conversation_deleted` | `{team_id, workspace_id, conversation}` | 清空该对话的通道历史 |

**为什么这样切**：会话通道（`TrunkStore`）、会话级 provider、事件出口都是宿主的
会话模型，模块不该认识它们；而 store 的数据与校验归模块。迁出前 store 在宿主里，
两边靠"不要同时写"的纪律维持一致；现在宿主**没有** store 的写句柄，分叉在类型上
就不可能发生。

## 命令域（9 条）

面板 7 条（线协议，`BackendCommand`）：`RequestWorkspaceSessions` /
`SaveWorkspaceSession` / `DeleteWorkspaceSession` / `ActivateWorkspaceSession` /
`RequestWorkspaceGitStatus` / `RequestWorkspaceFiles` / `RequestBrowseDirectories`。

宿主内部 2 条（**不进线协议**，宿主经 `ModuleHost::dispatch_raw` 调）：

| 命令 | 用途 | 为什么必须跨仓 |
|---|---|---|
| `WorkspaceSetConversationApiProfile` | 会话级 API 覆盖写进工作区对话 | 宿主不再持有 store，只能请求唯一写者 |
| `WorkspaceRenameApiProfile` | 供应商改名后传播 `api_profile` 引用 | 同上 |

外加 `WorkspaceToolCall`（`workspace` 工具的调用入口，见下）。

## `workspace` 工具

模型看到的是宿主的**代理壳**（`agent/workspace_tool.rs`），描述与参数 schema 直接
取自模块 crate（`tool::description` / `tool::parameters`）——模型面与实现只有一个
真值。调用时宿主把 `ToolContext.team_id` 补进载荷，转发到模块的
`WorkspaceToolCall`，模块按 `team_id` 定位自己的 store。

> 顺序坑：工具入口必须出现在模块的 `with_commands` 声明里（宿主的命令分发只问
> 声明过的名字）。漏了它的表现是「工具存在但每次都说模块未启用」。

## 落盘位置与兼容

- 文档路径：`$XDG_CONFIG_HOME/echo-agent-core/echo-workspaces-{team_id}.json`
  （模块配置 `[module.config] state_dir` 可覆盖）——**与迁出前同一路径**，旧数据
  直接可读，无需迁移。
- 线类型：模块仓 `wire` 模块是 `agent-module-management` 的**镜像**（字段名逐字一致）。
  为什么不用同一套类型：Core 以 git 依赖引入模块仓，模块反向依赖 Core 的 crate
  就是包级循环依赖，Cargo 直接拒绝。两侧的边界收在
  `workspace_commands::to_protocol_session`，任一侧加字段都会在编译期报错。

## 跨机与降级

跨机工作区（`node://<peer>/…`）需要向对端查询，而**模块仓不依赖联邦实现**
（同样的循环依赖约束）。当前行为：

| 能力 | 本机目录 | 跨机目录 |
|---|---|---|
| 会话 / 对话 / 激活 / 持久化 | ✅ | ✅（目录归属照常保留） |
| git 快照 | ✅ | ⚠️ 占位条目 + 原因 |
| 文件浏览 / 目录选择器 | ✅ | ⚠️ 空结果 + 原因 |

降级是刻意的（单机部署根本走不到），多机部署要由宿主补一个「远程目录代理」
（可做成宿主订阅的另一个模块事件）。宿主侧的**联邦沙箱**（`federation_rt`
的 `executor_roots` / `QueryKind::WorkspaceFiles`）仍读镜像里的目录并集，因此
跨机文件查询的**判定依据**不受影响。

## 启停与门控

- `kind = tool`，`enabled_modules` / `enabled_tools` 都能按人格收窄；全局取消勾选后
  `workspace` 工具从工具表消失、命令无人处理（宿主回明确错误，不静默）
- 禁用模块**不会删除已存的工作区文档**——重新启用即恢复
- 工具注册时打 `echo-agent.workspace` 包标签，于是按包启停与 persona 门控作用于
  同一个单位
