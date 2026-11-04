---
id: workspace
title: "工作区会话插件"
group: Core
x: 4160
y: 1200
---

# 工作区会话插件

> **定位**：`echo-agent.workspace` **这个插件本身**——工作区会话的数据归属、7 条命令域与 `workspace` 工具、宿主必须交给它的四项能力、以及跨机目录的降级策略。「激活工作区 = 进入项目对话通道」这件事对 agent 意味着什么，见 [多 Agent 与会话](../core/agents.md)「工作区会话与项目通道」；Panel 侧的入口行面板见 [会话入口行](../panel/chat-entry.md)。读者：改工作区行为、或排查「切了目录模型还在老地方干活」的开发者。

## 工作区是什么

工作区会话 = 用户从 Panel 管理的一组工作目录（创建 / 改名 / 删除 / 激活），可逐目录查看 `git status`。激活会话的目录会注入系统提示词，让模型知道自己该在哪里干活。

插件自身由四部分组成：

| 组件 | 职责 |
|---|---|
| `WorkspaceStore` | 会话文档（每 persona 一个 JSON，原子写） |
| `WorkspaceTool` | 模型可见的 `workspace` 工具（list / status / use / create / delete） |
| 只读采集 | `collect_dir_git` / `collect_dir_files` / `collect_child_dirs` + 沙箱解析 `resolve_within_directories` / `resolve_within_browse_roots` |
| 通道标识 | `channel_scope_id` / `parse_workspace_channel`——会话 id 走 `echo_session::SessionKey::local_workspace`，**不在这里另定义格式** |

## 身份与贡献

| 项 | 值 |
|---|---|
| 插件 id | `echo-agent.workspace`（**唯一真值在插件侧**；宿主的 `WORKSPACE_PLUGIN_ID` 是别名） |
| kind | `tool`（persona 白名单参与） |
| 名称 / 路由 | 「工作区会话」/ `workspace` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/plugin/echo-plugin-workspace`（3 文件，约 3300 行） |

| 贡献 | 内容 |
|---|---|
| 工具 × 1 | `workspace`（多会话 / 多目录管理、git 状态、只读文件浏览） |
| 命令域 × 7 | `RequestWorkspaceSessions` / `SaveWorkspaceSession` / `DeleteWorkspaceSession` / `ActivateWorkspaceSession` / `RequestWorkspaceGitStatus` / `RequestWorkspaceFiles` / `RequestBrowseDirectories` |
| 服务依赖 | `echo-agent.host.workspace`（`WORKSPACE_HOST`） |
| 不 provide 服务键 | 它的能力只通过工具与命令暴露，没有别的插件需要解析它 |

id 放在插件侧而不是宿主，理由写在代码里：id 是「这个插件叫什么」，属于插件自己；宿主定义一份常量再让插件猜，两边一改就漂移。

## 宿主接缝：四项能力

命令域这 7 条原本是宿主 `impl Agent` 的 565 行方法。搬进插件后逻辑没变，但**依赖方向掉了个头**——它不再需要 `Agent`，需要的是四项能力：

| 需要什么 | 为什么归宿主 |
|---|---|
| `store` | store 是 per-persona 的，按 `team_id` 解析 |
| `emit` | 事件出口绑在人身上（`Agent::emit` 才带 team 标注） |
| `browse_roots` | 要遍历**所有** persona 的 store |
| `remote_query` | 联邦传输属宿主 |

**刻意不在四项里的**：store 变更后的列表/激活广播、会话通道注册、会话级 API 恢复。那是 store 的**变更钩子**，宿主在 `set_workspace_store` 时就接好了——插件只要改 store，广播自动发生。这一条很重要：它让命令处理器不需要知道「会话通道」这种东西存在。

## 跨机与降级

跨机工作区（`node://<peer>/…` 目录）需要向对端查询，但**本 crate 不认识联邦实现**：查询能力经 `RemoteWorkspaceQuery` 注入，语义方法（问 git / 问文件 / 问目录）定义在定义层 `echo_defs::workspace`——既不做成 workspace 插件的私有词汇（那会让实现方依赖本插件），也不住在联邦里（那会让本插件依赖联邦）。

未注入时远程目录一律降级为「远程不可采集」占位，本机功能完全不受影响。这是**降级而不是错误**：单机部署根本不需要该接缝。

## 启停与门控

- `kind = tool`，`enabled_plugins` / `enabled_tools` 都能按人格收窄；全局取消勾选后 `workspace` 工具从工具表消失、7 条命令无人处理
- store 是 runner 之外的第二份 per-persona 状态，**禁用插件不会删除已存的工作区文档**——重新启用即恢复
- 工具注册时打 `echo-agent.workspace` 包标签，于是按包启停与 persona 门控作用于同一个单位
