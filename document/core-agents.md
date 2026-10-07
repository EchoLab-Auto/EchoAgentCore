---
id: agents
title: "多 Agent 与会话"
group: Agent 运行时
x: 955
y: 1512
link: ["core-memory | 会话记忆"]
---

# 多 Agent 与会话

Core 支持**多 agent 人格**：`[agent.teams.*]` 每项 = 一个独立 Agent（独立 trunk、会话文件、系统提示词、能力白名单）。

> **无「主智能体」**（2026-09-13）：所有智能体一律平等，没有受保护的"主/默认"角色。进程级职责（管理面、插件宿主、全局策略、Shell、适配器）由组合根的**核心服务代理**（`__core`，非人格、不出现在 TeamsList）承担；会话类命令必须显式携带 `team_id`，缺失直接报错。

> **单一注册表 + `(core, team_id)` 身份**（2026-10 修复）：persona 注册表由
> `echo_agent::AgentManager` **独占持有**（单一真源），`AgentSupervisor` 只是它的
> 薄适配（`get`/`personas`），不再维护第二份 HashMap——此前两套注册表对「teams
> 非空但全部被禁用时是否合成 `default`」处理不一致，会让命令按 `team_id=default`
> 路由时解析不到，Panel 报「智能体 default 不存在」。多接入点（联邦网络多节点）
> 下人格身份是 **`(core, team_id)`**：Panel 的 `TeamsList` 按 core 合并、命令按
> 人格归属 core 路由（会话归属优先，其次人格归属，最后 `activeCore`），并带一致
> 性守卫（目标 core 不拥有该人格时重定向）——详见 [联邦](./federation.md)。

## Persona 装配

- `AgentSupervisor` 按配置创建所有人格；`make_agent` 闭包为每人格独立组装工具注册表（内置工具 + QQ 工具 + 包元数据）
- 每人格独立事件总线；**所有人格**（含运行期新建）的 `emit` 都直投进程级事件汇聚点（`EventSink`），Panel 单连接即可看到全部活动（不再有"镜像进默认人格"的转接）
- 会话文件隔离：统一 `echo-sessions-{id}.json`（旧 `echo-sessions.json` 首次启动自动改名迁移）；会话持久化路径与配置 TOML **解耦**（`set_config_store` 只管 `[agent]` TOML，`set_session_persist_path` 另设 JSON 事件溯源文件，绝不可同路径互写）
- **Persona 级 API**（2026-09）：`[agent.teams.{id}].api_profile = "<供应商池名>"` 只做**引用**，值存于全局 `[agent].api_profiles`；配置了引用的人格启动时用 `apply_named_profile` 解析并**独立构建自己的 provider**（不共享默认 provider）；未配置 = 跟随全局默认（顶层 + active_api）

## 会话模型

- 会话（Session）= 对话身份，由 `SessionKey`（platform:scope:user_id，多实例带 `@account`）标识，带昵称/群名/最后活跃/team_id
- **会话归属的三层保证**（2026-09-18）：① 运行期创建时按 trunk 归属打标（`get_or_create` 取 `TrunkStore::team_id`）；② `get_or_create` 命中既有会话时若 team_id 为空就地补标（persona 组装顺序中 set_team_id 可能晚于会话恢复）；③ 恢复兜底——identities 元数据持久化/恢复 team_id，且事件日志里出现但元数据缺失的会话按事件归属补建注册（`ensure_identities_for_events`：事件是事实来源，元数据只是缓存）。缺任一层的后果：Panel 按 team 过滤会话时该 persona 的持久化会话全部消失（入口行「会话」按钮不出现）
- **每个会话拥有独立的模型上下文**（多会话，2026-09）：事件日志是唯一事实来源，事件带 `session` 归属；`TrunkStore` 按会话投影出各自的 `history`（token 预算逐会话生效），不同会话的上下文互不可见——QQ 私聊、QQ 群、本地 TUI 是独立对话（投影机制详见 [会话记忆](./core-memory.md)）
- **本地工作区通道**（2026-09-14）：本地来源（`platform=local`）的工作区专属对话上下文（`local:workspace:<workspace_id>:local_user`）——激活工作区即切换到的对话，见下节
- 不同 agent 的上下文完全隔离（各自独立的事件日志与投影）
- 上下文快照（`RequestContext`）按会话返回（`session_id`；`ContextSnapshot` 回带归属）；`CompactHistory` 按会话分别压缩——**预览 → 归档快照（`archives/*-precompact.json`）→ LLM 交接摘要（失败按组回退规则统计文案）→ 落地**（机制与统计见 [会话记忆](./core-memory.md)§压缩与归档）；`ClearHistory` 清空该智能体全部会话
- `RequestState` 返回**所有人格**的会话（各会话携带自身 team_id），Panel 按当前 agent 过滤展示
- **单会话模式下的 QQ 入站投递**（2026-09-18）：hook 格式化的消息（`<qq_message_hook>`，带完整平台/发送者元数据）投递到该 persona 的**默认本地会话**（`local:tui::local_user`）排队跑 turn——不开临时回复分支（ReplyBranch 是并行模式专属可见性机制）、不发 QQ 临时回复；`group_id=None` 保证普通输出不推 QQ，**是否回复由 agent 自行判断**（qq-transport 技能：回复必须走 send_* 工具，明显无需回应的消息可不回复）。QQ 会话键仍按作用域注册（Panel 会话列表可见），并行多会话模式保持原临时回复分支行为

## 工作区会话与项目通道（2026-09-14 重定义）

工作区会话（`echo-agent.workspace` 插件）从「目录组 + 提示词注入」升级为**项目上下文**：
「激活」不再只是换提示词，而是**进入项目对话**。

**① 对话会话扩展——本地工作区通道**。本地来源的对话会话分两种形态：

| 形态 | id | 说明 |
| --- | --- | --- |
| 默认本地会话 | `local:tui::local_user` | 未进入任何项目时的本地对话（原状） |
| 工作区通道 | `local:workspace:<workspace_id>:local_user` | 工作区会话的专属本地对话；昵称 = 工作区名；**惰性注册**（首次激活时注册并广播 `SessionUpdated`，此后常驻） |

通道与其他会话完全平等：独立上下文投影、独立 token 预算、出现在会话列表 Local
分组，同样受压缩/清理等现有手段管理。`<workspace_id>` 为工作区 id（创建时由
名称 slug 派生、改名不变），通道 id 因此永久稳定。

**② 工作区会话 = 项目上下文**，三要素：**目录组**（多目录 + git 状态，原样）、
**专属对话通道**（见上表）、**激活状态**（per-persona 单例 `active`，字段不变、语义升级）。

**③ 激活 = 进入项目对话**：
- 本地对话切换到该工作区的专属通道——上下文随之切换，通道历史从激活起独立积累；
- 系统提示词注入目录信息（对 persona 的**全部来源会话**生效：本地与 QQ 都会看到）；
- `workspace` 工具默认操作该工作区。

**④ 单一事实来源 = `active` 标记**（`echo-workspaces-{id}.json`）。所有激活来源——
面板按钮、会话切换器、模型 `use` 工具——共用**同一条广播路径**：store 变更钩子
（`WorkspaceStore::set_on_change`，由 `Agent::set_workspace_store` 装配期接线，
弱引用不构成循环）。任何成功变更（新建/重命名/删除/激活）→ 确保通道会话注册 +
`SessionUpdated`（active 存在时）→ 广播 `WorkspaceSessions`；面板的「本地当前
对话」是 active 的**投影**（active 空 → 默认会话；active=W → W 通道），不是独立
状态。重启恢复：装配时持久化的 `active` 直接补注册通道（幂等，不广播）。

**⑤ 前端联动规则**：
- 会话切换器 Local 分组 = 默认会话 + 已注册通道：**选通道 = 激活对应工作区；选默认会话 = 取消激活**；
- 收到 active 变化（含模型 `use` 触发）：**仅当当前活动会话为 Local 来源时**跟随切换（正在查看 QQ 会话时不打扰；切回 Local 时落到 active 对应通道）；
- QQ 等外部来源不受切换影响：查看/切换不改 active，独立上下文照旧按会话隔离，激活只改变其提示词注入。

**⑥ 文件浏览器（Panel 侧，2026-09-15）**：工作区面板左栏提供**只读文件浏览器**
（`RequestWorkspaceFiles` → `WorkspaceFiles`）——多根切换、面包屑导航、目录下钻与
回退、文件大小展示。服务端以 canonical 路径前缀校验把浏览范围**限定在会话声明的
工作区目录及其子孙内**（符号链接按解析后的真实路径比较；`/a/bc` 不会被 `/a/b` 误放行），
越界或不存在经 `WorkspaceFiles.error` 回传；隐藏项（`.` 开头）跳过、单目录超
`FILES_CAP`（500）截断。弹层几何见 [Panel 会话视图](./panel-chat.md)§7.7。

**⑦ 边界**：
- 既有历史留在默认会话，**不自动迁移**；通道历史从激活起从头积累；
- 删除工作区**不删除**其对话历史（通道会话保留在列表）；
- 未激活时本地新消息落默认会话；
- 通道机制与循环模式（single/parallel）无关；会话切换器可见性 = 存在可切换会话（含通道）。

## 临时分支

每条入站消息 = 一个临时回复分支。

- 分支可见性由**循环模式**控制：并行多会话模式发射 ReplyBranch* 可见性事件、侧边栏有分支卡；单会话模式不发射、无分支卡，但分支照常执行合并（可见性开关而非执行开关）；单会话模式还会排队同一会话的 turn（串行准入，见 [Agent 循环](./core-agent-loop.md)§循环模式）
- 分支运行期活动实时进主时间线（推理/工具/回答按序穿插），结束仅清理标签

## 前端切换体验

- 当前 agent 持久化于 localStorage（`echo-panel-active-team`），刷新/重连后停留原 agent
- 切换 agent：先渲染该 agent 的时间线缓存（零等待），再以 `since_seq` 增量刷新
- 侧边栏分支卡与「全局」会话项仅**并行多会话模式**显示；单会话模式隐藏（会话切换统一在入口行「会话」按钮，2026-09-14 起）

## 能力隔离

- `enabled_*` 白名单：非空时仅列出的能力可见（空表 = 全部启用）；工具/技能另有 `disabled_tools`/`disabled_skills` 黑名单细化。**插件维度只有白名单**——原 `disabled_plugins` 黑名单已于 2026-09-11 移除（与白名单语义重复），既有配置在加载期物化进白名单
- 插件/工具/技能三个维度独立配置；未配置白名单时默认全部启用
- **保存即热生效**（2026-09）：`SaveTeam` 后目标人格立即重算门控（工具/技能逐名双向：取消勾选禁用、重新勾选恢复；插件包维度按 `GATED_PLUGIN_IDS`）；`ToggleTeam` 重新启用的人格也会补应用门控。全局禁用（`ToggleTool`/`ToggleSkill`/`TogglePlugin` 卸载）优先级高于 persona 名单，且这些全局开关会逐 persona 重算而不是只作用于管理面
- **Package 跨维度**（2026-09）：勾选/取消勾选一个门控插件 = 启停整个包（plugin + 同名包工具 + 同名包技能，QQ 包示例：send_* 工具与 qq-management/qq-transport 一起开关）；详见 [插件化设计](./core-plugins.md)「Package」章节
- **循环模式**（互斥插件 `echo-agent.loop.{single,parallel}`）由白名单推导：空表 = single（默认）；含 `loop.parallel`（或旧编排模式 id）= parallel。推导单一来源 `TeamMember::loop_mode()`（只看白名单）
- 全局 `[agent].disabled_tools`（Panel ToggleTool 持久化）启动时逐人格应用

## 设计取舍与边界

- **多实例而非单实例多上下文**（对人而言）：每个人格一辆"车"（独立 `Agent::new` + 事件溯源日志），复用现有结构、互不干扰
- **人格内多会话上下文**（2026-09 落地）：同一人格内再按来源（本地/QQ 私聊/QQ 群）分区上下文——单份事件日志 + `session` 归属字段 + 按会话投影（`derive_messages_for`），避免"每个聊天一份完整 Agent"的内存与调度开销
- **配置驱动**：人格在配置文件中定义，`enabled=false` 跳过实例化；运行时可通过 `ToggleTeam` 启停、`SaveTeam`/`DeleteTeam` 新建/删除（写回 `[agent.teams]`，立即生效）
- **兼容性**：无 `[agent.teams]` 的旧配置 = 单 agent（id 仍可为 `default`，但**无特权**）；旧协议 `SendMessage`/会话类命令若无 `team_id` 会被明确拒绝（破坏性变更，2026-09-13），旧 Panel 需同步升级
- **删除保护**：仅"至少保留一个智能体"；不再有受保护成员
- ~~所有 persona 共享同一 provider/模型~~（2026-09 起支持 persona 级引用，见上「装配」）；运行期 `SaveTeam` 改 `api_profile` 立即经 `apply_persona_api` 重建该人格 provider（全局 `UpdateApiConfig`/`SwitchApi` 只作用于管理面/全局默认，不再自动广播到其他人格）
- QQ/平台消息按**实例归属人格**路由：每个启用 `echo-agent.adapter.qq` 的人格拥有自己的 QQ 实例（容器 + 反向 WS 通道），见 [QQ 适配器门控](./adapter-qq-gating.md)「多实例」
- 每人格可独立配置记忆预算：`[agent.teams.{id}].memory_limit_tokens` / `context_window_tokens`（未配置 = 继承全局）

## 典型配置

```toml
[agent.teams.alix]
name = "Alix"
description = "管理型人格"
# 空 enabled_plugins = 全部启用 → 单会话（默认）循环模式
# （纯对话型人格：enabled_plugins 列全部插件、唯独去掉 tools.builtin 与
#   skills.dir——黑名单已移除，唯一名单就是白名单）

[agent.teams.self-coding]
name = "self-coding"
# 显式开启并行多会话循环模式（侧栏分支卡/全局项/可见分支）
enabled_plugins = ["echo-agent.tools.builtin", "echo-agent.management.panel", "echo-agent.loop.parallel"]
# 可选：persona 级 API（引用全局供应商池 [agent].api_profiles 中的 profile 名）
# api_profile = "openai"   # 不配置 = 跟随全局默认配置
# 显式写法（等价）：追加 "echo-agent.loop.single"；并行多会话模式则列
# "echo-agent.loop.parallel"。旧编排模式 id（branch.reply 等）
# 与旧驱动 id（loop.runner）在加载期自动迁移。
```
