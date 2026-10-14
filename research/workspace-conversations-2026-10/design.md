# 工作区多对话（Workspace Conversations）设计草案

> **状态**：草案 v2（2026-10-14 修订：全对话平等，仅存量 id 例外），未经实现。
> **背景**：当前一个工作区 = 一条本地通道会话（`local:workspace:<ws_id>:local_user`，
> 1:1 映射）。用户场景：同一项目里并行推进多条任务线，各自独立上下文
> （类�� ChatGPT 的「项目 vs 会话」）。
> **读者**：维护 workspace 插件 / 会话体系 / 面板的开发者。
> 相关：[多 Agent 与会话](../../document/core-agents.md)§工作区会话与项目通道。
>
> **v2 修订说明**：v1 的「默认对话（main）特权」被用户质疑后重审——结论是
> **全对话平等**：任何对话都可建/改/删，无特权槽位；唯一例外是**存量通道
> id**（旧工作区历史挂在无对话段位的 id 上，事件日志 append-only 不可改写），
> 它只是一条通道 id 恰好是旧格式的普通对话。详见 §6。

## 1. 现状约束（为什么现在只能是 1:1）

1. **通道 id 无对话维度**：`SessionKey::local_workspace(ws_id)` 把四段式
   `platform:scope:scope_id:user_id` 的 `scope_id` 给了工作区 id、`user_id`
   恒为 `local_user`——没有承载"对话序号"的槽位（`session.rs:92`）。
2. **激活标记只有工作区维度**：`WorkspaceDocument.active: Option<String>` 是
   工作区 id，不含对话（`packages/workspace/mod.rs:43`）。
3. **元数据单列表**：`WorkspaceSessionInfo` 无对话清单字段；面板
   SessionSwitcher 把每条工作区通道当一行显示。

其余体系（压缩/清理/归档按 `session_id` 精确作用、token 预算逐会话、
事件归属过滤）**天然兼容多对话**，无需改动。

## 2. 核心决策

### 决策 1：通道 id 方案——新对话带 `/` 后缀；存量通道 id 原样保留

```text
存量通道（旧数据）：  local:workspace:<ws_id>:local_user            （不动）
新对话通道：          local:workspace:<ws_id>/<conv_id>:local_user  （scope_id 内嵌）
```

- `SessionKey` 结构**不动**：`scope_id` 直接携带 `<ws_id>/<conv_id>` 字符串，
  消费方（workspace 插件、面板分组）以 `split_once('/')` 拆分。
  `SessionKey::parse` 用 `splitn(4, ':')`，`/` 不与冒号冲突；
  `slugify` 只产出 `[a-z0-9-]`（`mod.rs:333`），`/` 天然不出现在 id 中，
  无解析歧义。conv_id 同样 slug 化，且门禁不允许 `/`。
- **存量 id 例外的唯一落点**：`channel_id(ws_id, conv_id)` 一个函数——
  存量对话（`legacy == true`）返回旧格式，其余返回新格式。它只在
  「通道 id 派生」处存在，不泄漏到激活/删除/展示语义。

### 决策 2：对话元数据存 WorkspaceStore，通道 Session 惰性注册

与现有「工作区列表存 store、通道惰性注册」的模式同构：

```rust
// WorkspaceSessionInfo 增加（serde default，旧档反序列化为空）
conversations: Vec<WorkspaceConversationInfo>,   // [{id, name, legacy}]

// WorkspaceDocument.active 扩展为二元组（推荐方案 B：双字段，见 §6）
active: Option<String>,               // 既有字段：工作区 id
active_conversation: Option<String>,  // 新增：对话 id
```

- `WorkspaceConversationInfo { id, name, legacy }`：
  `legacy: true` 仅用于标记存量通道 id 的对话（`channel_id` 派生时消费），
  不参与任何权限/展示差异。
- **读取期惰性补位**：旧档工作区（`conversations` 为空但存在旧通道历史）
  逻辑上视为含一条 `{id: "main", legacy: true}` 对话——**不落盘**，直到
  用户重命名/新建第二条时才物化。旧档零改写。
- 对话的**历史**仍在事件日志（Session 体系），store 只存轻量元数据；
  删除对话 = `clear_session(通道 id)` + store 移除条目。

### 决策 3：激活语义——激活单位是「(工作区, 对话)」，无默认特权

- `WorkspaceDocument` 每工作区不记默认对话，改记 `last_conversation`
  （最后使用的对话 id，写入 `WorkspaceSessionInfo` 或独立 map，实现期定）。
- 激活规则：

| 操作 | 语义 |
| --- | --- |
| 激活工作区（不带对话） | 进 `last_conversation`；不存在/对话列表为空 = **自动新建一条** |
| 激活对话 | 同时激活所属工作区与该对话；更新 `last_conversation` |
| 取消激活 | `active = None`（回默认本地会话；现状语义保留） |
| 切对话（工作区已激活） | 只换对话，提示词中的工作区目录不变 |

- **删除**：任何对话都可删。删当前激活的 → 切到同工作区最近的对话；
  对话数为 0 是合法状态（工作区仍在，下次激活时新建）。删对话前 Panel
  两步确认（历史清空不可恢复）；`ArchiveHistory` 可按通道 id 先归档。

- 系统提示词注入仍以**工作区**为单位（目录清单不变），追加一行当前对话名
  （`# 当前工作区会话\n<ws>（当前对话：<conv>）`），token 开销增量可忽略。

## 3. 分层改动清单

### Core（echo-agent）

1. `packages/workspace/mod.rs`
   - `WorkspaceConversationInfo { id, name, legacy }`（新类型）
   - `WorkspaceSessionInfo.conversations`（serde default）
   - `WorkspaceDocument.active_conversation: Option<String>`（serde default；
     `active` 保留 String 存工作区 id，向后兼容）
   - `set_active(ws, conv)` / `conversation upsert/delete/rename` /
     `last_conversation` 维护
   - conv id：slugify + 同工作区内去重（数字后缀），禁止 `/`
   - `prompt_text()` 追加当前对话名
2. `session.rs`
   - `ensure_workspace_channel(ws_id, conv_id, legacy, name)` 签名扩展
   - helper：`split_workspace_scope(scope_id) -> (ws_id, conv_id)`
3. `packages/workspace/commands.rs`
   - `ActivateWorkspaceSession { id, conversation? }`（协议新增可选字段）
   - `workspace_after_change`：注册当前对话通道 + 广播
4. `workspace` 工具：`use` 增加可选 `conversation`；新增子命令
   `conversations`（`action: list/create/rename/delete`，`workspace` 缺省 =
   当前激活工作区）——模型可自主开新任务线。

### 协议（echo-protocol）

- `WorkspaceSessionInfo.conversations: Vec<WorkspaceConversationInfo>`
  （serde default；旧 Core 缺失 → Panel 视为仅存量对话）
- `WorkspaceSessions` 事件增加 `active_conversation: Option<String>`
  （active 保留 workspace id，向后兼容）
- `ActivateWorkspaceSession` 增加 `conversation: Option<String>`

### Panel

- `protocol.ts` 镜像上述字段。
- **SessionSwitcher**：Local 分组中工作区通道按对话展开为独立行
  （标签「工作区 · 对话名」）；选中行 = 发送带 `conversation` 的激活命令。
- **WorkspacePanel**：工作区条目内嵌对话管理（新建/重命名/删除/切换）；
  删除任意对话前两步确认（清空其通道历史用 `ClearHistory` 按 session_id）。
- 旧 Panel 连新 Core：只看到存量对话通道（其余通道 id 含 `/`，旧面板
  分组逻辑按 scope=workspace 识别，仍能在会话列表显示——可接受降级）。

## 4. 兼容性矩阵

| 组合 | 行为 |
| --- | --- |
| 新 Core + 旧数据 | 存量对话惰性补位（`legacy`），旧通道 id 不变，零迁移 |
| 新 Core + 旧 Panel | 存量对话显示如旧；额外对话以通道 id 裸显（降级但不崩） |
| 旧 Core + 新 Panel | `conversations` 缺省 → 只显示存量对话；不发 `conversation` 字段 |

## 5. 实施步骤（建议 3 笔提交）

1. **Core 数据模型 + 通道 id**：`WorkspaceConversationInfo` / `active_conversation`
   / `ensure_workspace_channel` 扩展 / conv slug 门禁 / `channel_id` 例外函数 +
   单测（幂等、旧档迁移、id 往返解析、删除当前对话切换）。
2. **协议 + 命令 + 工具**：`ActivateWorkspaceSession.conversation`、
   `WorkspaceSessions.active_conversation`、`workspace` 工具 conversations
   子命令 + prompt_text 追加对话名 + 集成测试。
3. **Panel**：protocol 镜像 + SessionSwitcher 展开 + WorkspacePanel 对话管理
   + vitest。

## 6. 风险与开放问题

- **active 的序列化迁移**：推荐方案 B——保留 `active: Option<String>` 字段名
  存 ws_id，新增独立 `active_conversation: Option<String>` 字段。两份字段
  一致性由 `set_active` 单点写入保证；旧档 `"active": "ws"` 读为
  `{ws, None}` → 激活时按 `last_conversation` / 自动新建解析。
- **对话删除 vs 归档**：删对话直接清历史还是提示先归档？建议默认
  `ClearHistory` 语义（该通道事件+投影+时间线），删除前 Panel 两步确认；
  可选「归档后删除」按钮。
- **联邦**：会话迁移（MigrateSession）按 session_id 工作，通道 id 含 `/`
  不影响；跨机目录不感知对话维度，无需改动。
- **对话数为 0 合法**：工作区可零对话存在；激活时自动新建一条。删除工作区
  仍级联清全部对话通道（`ClearHistory` 按各通道 id 逐一执行）。
- **命名**：存量对话的显示名建议「主对话」（`legacy: true` 时由 Panel/Core
  展示层决定，不锁死在数据里）；工作区改名不影响 conv id。
