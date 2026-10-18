---
id: panel-layout
title: "Panel 布局与导航"
group: 前端
x: 1920
y: 780
---

# Panel 布局与导航

> **定位**：本文覆盖 Panel 的布局骨架与导航形态：视图层级（三主视图 + 设置八分类）、应用外壳几何与层叠秩序、连接生命周期（重连 / 自愈 / 事件流连续性）、侧边栏（临时分支卡）、Agent 切换器与会话切换（入口行）、多窗口语义（哪些状态归窗口、哪些归全局）。读者：维护 Panel 布局与导航的开发者；实现位置以 `web/src/` 相对路径标注。相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)、[设置视图](./panel-settings.md)、[移动端适配](./panel-mobile.md)（< 768px 窄屏形态）、[Panel 前端](./panel.md)（中继模型与状态归属）。

## 一、结构总览

本节以图表达 Panel 的层级逻辑——视图组成、布局组件树、数据流分层；各项的具体定义（行为、常量、协议命令）见后续对应小节。

### 1.1 视图层级

```prodoc-flow
graph TB
  Root[Panel 主视图] --> Chat[会话]
  Root --> ShellV[Shell 详情]
  Root --> Sets[设置]
  Sets --> Api[API 设置]
  Sets --> Skills[技能]
  Sets --> Tools[工具]
  Sets --> Plugins[插件]
  Sets --> Teams[智能体]
  Sets --> Logs[日志]
```

- 三主视图（会话 / Shell 详情 / 设置）；顶栏导航为「聊天」「设置」（Shell 详情经边栏「Shell」卡「详情」进入，§二）；设置视图的八个分类走内部一级菜单（见 [设置视图](./panel-settings.md)）；任务视图已并入会话视图入口行「任务」弹层（见 [会话视图](./panel-chat.md)「入口行按钮与弹出层」）
- 模态与覆盖层（Branch / Agent 配置 / 上下文）浮于全部视图之上（见 [模态与覆盖层](./panel-modals.md)）；侧边栏点选会话强制回会话视图

### 1.2 布局与组件树

```prodoc-flow
graph TB
  App[App.vue 应用壳] --> Layout[NeumorphismLayout 外壳]
  Layout --> Header[顶栏：品牌 · 导航 · 主题]
  Layout --> Main[主区：当前视图组件]
  ChatView --> Rail[双侧边栏 chat-rail：连接状态 · 文件浏览器 · Shell · 临时分支]
  Main --> ChatView[ChatView 会话]
  Main --> SettingsView[SettingsView 设置]
  Main --> ShellPanel[ShellPanel Shell]
  ChatView --> MsgList[消息列表]
  ChatView --> EntryRow[活动浮条 · 入口行]
  ChatView --> Composer[悬浮输入区]
  ChatView --> Pops[弹出层：清单 · 实例 · 任务 TasksPanel]
  ChatView --> Overs[会话区内弹层：上下文 · Agent 配置]
  App --> Modals[模态：Branch]
  App --> Toasts[ToastProvider 右上]
```

几何与层叠常量见 §三；设置视图内部为 一级菜单 → 工具条 → 列表/详情 三段（见 [设置视图](./panel-settings.md)）。

### 1.3 数据流分层

```prodoc-flow
graph LR
  Core[Core 事件流] -->|WS /ws 中继| Conn[connection 单例]
  Conn -->|dispatch 逐事件归约| Store[state reducer]
  Store -->|响应式驱动| Views[视图渲染]
  Views -->|用户操作| Cmd[BackendCommand]
  Cmd -->|sendCommand| Conn
  Conn -->|命令下行| Core
```

- Core 是**业务状态**的唯一事实来源（「窗口在看什么」属窗口本地，§3.7）；断连重连后凭缓存游标增量补齐、缓存失效时由 Core 回退全量（§四）；唯一乐观例外 = 取消任务（见 [会话视图](./panel-chat.md) 与 [实例管理·任务·Shell](./panel-instance-tasks.md)）
- HTTP 旁路仅：日志接口（`/api/logs/{panel,core}`）、接入点管理接口（`/api/upstreams` 增删查，多接入点聚合，见 [Panel 概览](./panel.md)「配置与运维」）与媒体文件路由（`GET /media/{name}`，§3.5），其余全部走 WS——QQ 登录/二维码已迁到 WS（Core 代理）

## 二、视图与导航

Panel 有三个主视图（会话 / Shell 详情 / 设置）；顶栏导航按钮为「聊天」「设置」——Shell 详情经边栏「Shell」卡「详情」进入（无顶栏入口）；地址栏不承载视图状态（无路由）。

```prodoc-flow
graph LR
  Chat[会话视图] -->|顶栏: 设置| Sets[设置视图]
  Sets -->|顶栏: 聊天| Chat
  Chat -->|边栏 Shell 卡: 详情| Shell[Shell 详情视图]
  Shell -->|返回列表| Chat
  Side[边栏点选会话] -->|强制回到| Chat
```

- 当前视图按**标签页**持久化（`echo-panel-view`：本标签页 `sessionStorage` 权威 + `localStorage` 作新标签默认；非法值回退 `chat`；旧值 `caps`/`logs` 自动迁移为 `settings`，旧值 `tasks` 迁移为 `chat`），刷新后恢复本窗口自己的视图（`App.vue` 的 `loadView` / `setView`，见 §3.6）
- 主区同一时间只渲染一个视图组件
- 点选会话项（入口行「会话」切换器）**强制切回会话视图**（`App.vue::selectSession` → `setActiveSession`）
- 原顶栏「Shell」入口已移除（详情经边栏 Shell 卡「详情」进入）；原顶栏「任务」视图已移除——任务入口并入会话视图入口行「任务」按钮（徽标 = 当前会话运行中任务数），弹层复用 `TasksPanel` 且只对应当前 agent 的当前会话（见 [会话视图](./panel-chat.md)「入口行按钮与弹出层」）

> 原「资源」「日志」视图与 API 设置弹窗已合并为「设置」视图（见 [设置视图](./panel-settings.md)），顶栏设置下拉随之移除。

## 三、应用外壳与布局

### 3.1 布局区域与几何

外壳 = ui-frame `NeumorphismLayout`（`App.vue` 模板）：`show-header`、`height="100%"`——无 sider 槽位（原左侧边栏已移除）。

| 区域 | 内容 | 几何 |
|---|---|---|
| 顶栏左 | 品牌 "EchoAgent Panel"（`.brand` 字重 700） | 顶栏分左中右三段，右侧操作 `margin-left:auto` 顶齐 |
| 顶栏中 | （空；连接状态已迁入边栏「连接状态」卡（默认右列），见 [会话视图](./panel-chat.md)「侧栏与多接入点」） | 三段结构保留 |
| 顶栏右 | 两视图导航（聊天/设置）、主题开关（Shell 详情无顶栏入口） | 操作组 gap 6px、允许折行 |
| 双侧边栏 | 会话视图内左右两列 chat-rail 常驻卡片栈（RailStack）：连接状态 / 文件浏览器 / Shell / 临时分支，**卡片可拖动换列**，详见 [会话视图](./panel-chat.md)「侧栏与多接入点」 | 各 324px 透明容器层；**两列底统一让出入口行**（容器底 `entryBottom+45px`，左右对齐）；**列是否渲染由「该列登记过卡片」决定**（`rail-cards.ts::railRegistered`，读 `railLayout[side]`，只在用户拖动落位时变化）——卡片门控因异步状态（workspace 激活态 / loopMode / teams 列表）暂时失效时列**仍在**，显示空态占位「当前无可用卡片」；登记为空（从未拖过 / 最后一张卡被拖走）才不渲染。消息区留白（`trayPadLeft/Right`）用同一判据，因此门控抖动不引起正文行宽跳动。**卡片内容按本窗口解析**：文件浏览器卡由 `rail-cards.ts::workspaceCardFor` 读 `railSessionId` 决定绑哪个工作区（仅本窗口不在通道上时才回退全局 active，§3.7） |
| 主区 | 当前视图组件 | `flex:1` + `min-width:0`，纵向 flex，`overflow:hidden`（滚动交给视图内部） |
| （移动端 <768px） | 双侧边栏不渲染、入口行横滚、弹出层全宽贴底、设置走 Master-Detail | 见 [移动端适配](./panel-mobile.md)（断点与库 `useTouchDevice.isMobile` 同源；桌面零回归） |

页面级约束（`styles.css` 的 `html, body, #app` 段与头注）：

- `html/body/#app` 高 100%、禁止页面级滚动（`overflow:hidden` + `overscroll-behavior:none`）；背景 `var(--nm-bg-color)`
- 自定义样式层只写类选择器，**禁止裸元素选择器**（`button/input/…` 会污染库组件）——`styles.css` 头注

### 3.2 层叠秩序（z-index）

| 层 | z-index | 内容 |
|---|---|---|
| 基础视图 | 0 | 当前视图组件 |
| 边栏列 | 4 | 左右侧栏卡片列 |
| 输入区与「回到底部」 | 5 | 会话视图底部输入区、回到底部按钮 |
| 入口行 | 6 | 配置/上下文/清单/插件实例/任务入口行 |
| （已无独立清单卡） | — | 清单已并入入口行弹层（`PanelPopover` 内渲染 `ChecklistSidebar`）；`.checklist-float` 只是弹层内的 sticky 容器（`styles.css` 的 `z-index: 3`），不再在会话视图里单独悬浮 |
| 入口行弹出层 | 30 | `PanelPopover`（清单/会话/工作区/插件实例/任务浮层共用容器，`teleport` 到 body + fixed） |
| Agent 菜单 | 30 | AgentSwitcher 上弹菜单（设置视图同段 30–31） |
| 根 chip 速览 | 40 | 文件浏览器根路径 chip 悬停卡（仅文本、`pointer-events: none`） |
| 浮层遮罩 | 50 | 会话区覆盖层 |
| 拖动幽灵 | 200 | 边栏卡片拖拽幽灵 |
| 模态 / Toast | 库管理 | BranchModal、ToastProvider 等由 ui-frame 统一分配，始终高于应用层 |

会话视图内的悬浮元素按层内 z-index 叠放（边栏列 4 / 输入区与「回到底部」5 / 入口行 6 / 弹出层 30 / 会话区覆盖层 50 / 拖动幽灵 200），不参与全局竞争，详见 [会话视图](./panel-chat.md)。

### 3.3 主题与设计令牌

视觉体系来自 `@echolab-auto/ui-frame`（`--nm-*` 令牌 + `[data-theme='dark']` 机制），Panel 在其上覆盖（`styles.css` 的 `:root` 与 `[data-theme='dark']` 令牌段）：

| 令牌 | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--nm-primary-color` | `#0969da` | `#4c9aff` | 主色（库默认 lime 改为 panel 蓝） |
| `--panel-accent` / `-soft` | `#0969da` / 12% 透明 | `#4c9aff` / 16% 透明 | 选中态、忙碌点、徽标底 |
| `--panel-ok/warn/err` | `#1a7f37/#9a6700/#cf222e` | `#3fb950/#d29922/#f85149` | 状态语义色 |
| `--panel-hover` | 文本色 6% 混合（`color-mix`） | 同 | 悬停底 |

### 3.4 顶栏交互

- **连接指示**（已迁入边栏「连接状态」卡，默认右列、可拖动换列）：`已连接`（online 绿点）/ `连接中…`（connecting 呼吸点），驱动源 `state.connected`；卡片同时逐实例列出实例的运行态（全部平台），详见 [会话视图](./panel-chat.md)「侧栏与多接入点」
- **设置入口**：顶栏导航「设置」直达设置视图（见 [设置视图](./panel-settings.md)）——已不再有设置下拉与 API 弹窗
- **主题开关**：三态循环（浅色 → 跟随系统 → 深色），由 ui-frame `ThemeProvider` + `NeumorphismThemeToggle` 提供；持久化 `localStorage: echo-panel-theme`；外壳默认 `auto`，index.html 内联脚本防闪烁

### 3.5 媒体文件路由

`GET /media/{name}`：Panel web 后端读**媒体库目录**（`[server] media_dir`，
缺省 `~/.local/share/echo-agent-core/media`，与 Core 的 `$ECHO_MEDIA_DIR` 缺省一致）
返回 Core 落盘的入站图片。内容哈希命名 → `immutable` 强缓存；id 字符集校验 +
前缀包含检查防目录穿越（非法/缺失一律 404）。前端消息图片以
`<img loading=lazy>` 引用（Core 侧协议字段里的 `/media/<id>`）；Core 侧
"图片已省略"占位（空串）渲染为文字标。协议字段仍为 `images: string[]`。

### 3.6 持久化的界面状态

键分两类：**窗口偏好**（本标签页权威，`tab-prefs.ts` 的两层语义）与**共享键**（同源全标签页一份）。所有读写包 try/catch（隐私模式降级为不持久化）。

| 键 | 内容 | 归属与存储 | 缺省 |
|---|---|---|---|
| `echo-panel-view` | 当前视图 | 窗口（`sessionStorage` 权威 + `localStorage` 作新标签默认，`App.vue` 的 `loadView` / `setView` 经 `readPref` / `writePref`） | `chat` |
| `echo-panel-session` | 当前会话（含工作区通道） | 窗口（**仅** `sessionStorage`，`App.vue::setActiveSession` 经 `writeTabOnly`——它绝不该被别的窗口继承） | `local:tui::local_user` |
| `echo-panel-active-team` | 当前 Agent（刷新后停留原 Agent） | 窗口（两层同上；新标签继承最近一次的默认值） | 列表首个 |
| `echo-panel-active-team-core` | 当前 Agent 所属接入点 core（多接入点聚合） | 窗口（两层同上） | 无（单接入点） |
| `echo-panel-rail-cards` / `echo-panel-rail-layout` | 边栏卡片折叠/高度比例、归属与顺序 | 共享（`localStorage`，最后写者胜；见 [会话视图](./panel-chat.md)「侧栏与多接入点」） | 默认布局 |
| `echo-panel-caps-list-width` | 设置视图列表宽 | 共享（最后写者胜） | 分类缺省宽 |
| `echo-panel-trunk-cache` | 时间线磁盘缓存（按 `(core, team)` 键） | 共享但**合并写**（本窗口条目优先 + 保留他窗口条目，见 §3.7） | 无 |
| `echo-panel-token` | Bearer 访问令牌（`[server].access_token` 非空时；URL `?token=` 播种一次后持久化） | 共享（设计如此） | 无 |
| `echo-schedule-policy` | 分布式调度策略（`least_busy` / `round_robin` / `prefer:<name>`，`scheduler.ts`） | 共享 | `least_busy` |
| `echo-panel-theme` | 主题三态 | 共享 | `auto` |

### 3.7 多窗口语义

**概念**：多个标签页/窗口同时连 Panel 是正常用法——每个窗口一条 WS 连接、一份内存 `state`（`store.ts` 的响应式单例天然 per-tab）。串台的根因不是「共享了状态」，而是**把窗口态当成全局态**：视图、当前会话、当前 Agent 是窗口自己的选择；工作区 `active`、插件实例运行态、配置是 Core 的单例广播。据此分三层：

1. **窗口态**：当前视图 / 当前会话 / 当前 Agent / 侧栏展开 / 草稿 / 滚动位置——只在本窗口生效；持久化走 `tab-prefs.ts`（`readPref` / `writePref` = 本标签页权威 + 全局默认兜底；`readTabOnly` / `writeTabOnly` = 严格 per-tab）。
2. **Core 广播态**（per-persona 或进程级单例）：工作区 `active` / `active_conversation`、激活成功提示、TeamsList 等经 EventBroker 扇出给**所有**连接。收到时**只同步状态，不迁移视图**。
3. **共享偏好与缓存**：边栏布局、列表宽、令牌、时间线磁盘缓存——共享是设计，写时必须合并而不是整体覆盖。

**工作区激活回推的跟随判据**（唯一会改写导航的全局事件，`workspace.ts::decideOnWorkspaceBroadcast`）：

| 情形 | 处置 | 判据 |
|---|---|---|
| 本窗口刚发起激活，回推命中 `(active, active_conversation)` | `follow`：投影到该通道（reason `workspace-follow`） | `noteLocalActivateIntent` 登记 → `takeLocalActivateIntent` 一次性消费；5s TTL（`LOCAL_INTENT_TTL_MS`）+ 对话维度严格匹配 |
| 他端发起（另一窗口 / 模型 `use` 工具） | `keep`：只更新工作区面板与切换器激活标记 | 无意图或意图不匹配 |
| 本窗口停在非本地平台 / 「全局」等非 Local 来源会话 | `keep`：不把视图抢回工作区通道 | 意图命中但 `activeSessionId` 不以 `local:` 开头 |
| 本窗口停留的通道已失效（工作区或对话被删） | `reset` → 中立的 `LOCAL_DEFAULT_SESSION` | 快照里找不到该工作区/对话 |
| 工作区快照尚未同步到当前 Agent（`state.workspaceTeamId ≠ 当前 Agent`） | `keep`：不猜，等快照到达 | `App.vue` 跟随 watcher 的前置守卫 |

- 意图必须在 `sendCommand(ActivateWorkspaceSession)` **之前**登记（`App.vue::sendWorkspaceActivate`、`WorkspacePanel` 的「激活」切换与「点选对话」，均调 `noteLocalActivateIntent`）。**TTL 是必需项**：跟随 watcher 在 `workspaceTeamId` 尚未同步时提前返回、消费不掉意图，无 TTL 的残留会让本窗口此后被任意窗口的同目标激活带走。
- 落点规则：用户主动切 Agent（reason `team-select`）落该 Agent 自己的 active 投影；「当前 Agent 消失」与「全局会话被禁用」两条兜底（`team-reset` / `global-fallback`）一律落中立默认会话——用 active 投影会把本窗口带到别人刚激活的工作区。
- 排障：`activeSessionId` 的所有赋值收敛到 `App.vue::setActiveSession`（带 reason），与工作区广播决策各记一条内存环形日志（`window.__echoPanelSessionLog` / `window.__echoPanelWorkspaceLog`，各留最近 20 条，复现后一条 `copy(JSON.stringify(...))` 导出，无 IO 无 console 输出）。

**跨 team 广播隔离与缓存共存**：

- `WorkspaceSessions` 在 reducer 里按 `team_id` 过滤（`state.ts` 的 `WorkspaceSessions` 分支；缺省帧放行、本端 Agent 未定时不猜）——工作区状态在 Panel 是单槽，不过滤会让他 Agent 的广播污染成本窗口快照（文件浏览器卡门控失败、落点错、每次点选都重发激活）。
- 文件浏览器卡绑**本窗口会话**：`rail-cards.ts::workspaceCardFor` 先按 `railSessionId` 解析所属工作区，只在本窗口不在通道上时才回退全局 `state.workspaceActive`——他窗口激活/取消不再换掉本窗口卡片的标题与目录树。
- 成功提示按本窗口意图过滤：Core 用 `Error{session_id: null}` 广播「已激活工作区会话：X」，只在 `workspace.ts::hasRecentLocalActivateIntent` 为真（本窗口刚发起，窥探不消费）时显示，否则他窗口的操作会在本窗口弹提示。
- 时间线磁盘缓存合并写：`trunk-cache.ts::serializeTrunkCache` 本窗口条目优先，预算余量内保留既有文档里其他 `(core, team)` 条目（`foreignTeamEntries`，脏数据/版本不符/超限丢弃）；`activeTeamKey` 读内存 `state.activeTeamId`（仅引导期回落共享默认键），避免把另一窗口的 Agent 当成「当前」而挤掉自己的缓存。
- `TrunkTimeline` 响应是广播：增量条目按游标下界去重（`msgSeq <= cached.seq` 跳过，`state.ts` 的 `TrunkTimeline` 分支）——各窗口按自己的 `since_seq` 补拉互不重复追加，消息 key 不重建。

> 归属分类的完整表与 reducer 写码约束见 [Panel 前端](./panel.md)「多窗口语义」。

## 四、连接生命周期

```prodoc-flow
graph LR
  Closed[未连接] -->|ws onopen| Open[已连接: Bootstrap]
  Open -->|onclose| Toast[toast: 与后端断开…]
  Toast -->|500ms| Retry[重连尝试]
  Retry -->|失败: 退避×2 上限30s| Retry
  Retry -->|成功| Rebuild[清空运行期状态；时间线按游标增量补齐]
  Rebuild --> Open
```

- WS 地址 `ws(s)://{host}/ws`（随页面协议）；单例连接（`connection.ts`）
- **Bootstrap 命令组**（每次 onopen 按序发送）：`RequestState` → `RequestTeamsList`（这两条为只读发现命令，**裸广播全部在线接入点**，中继白名单放行）→ `RequestInstanceStatus` → `RequestShellSessions` →（有保存的 Agent 时）`RequestTrunkTimeline{team_id: 上次Agent, since_seq: 缓存游标}`（经 `sendCommand` 统一路由；多接入点下 core 未知时兜底裸广播，TeamsList 到达后 App watcher 定向补拉覆盖竞态；`connection.ts::startConnection` 的 `onopen` 段）。引导期「上次 Agent」读共享默认键，本窗口随后按自己的选择校正
  - **首屏优先级**：`RequestInstanceStatus` / `RequestShellSessions` 两条单值管理面命令**仅当目标区域已解析**（刷新/重连场景，activeRegion 自 localStorage 还原）时发送；首访区域未定时跳过——多接入点下会因解析不出目标被拒并误弹「尚未选择 agent」，随后区域解析完成由 activeRegion watcher 统一拉取（含这两条）
  - 去主智能体后 `team_id` 必填：没有保存过 Agent 时**跳过**时间线请求（发了必被 Core 拒绝），等 `TeamsList` 到达后由 App watcher 用列表首个发起（有缓存则同样带 `since_seq`）
  - **切换 core 时重拉「单值管理面」**（`connection.ts::requestActiveRegionManagement`）：`RequestState` / `RequestSystemPrompt` / `RequestInstanceStatus` / `RequestSkillsList` / `RequestToolsList` / `RequestPluginsList` / `RequestShellSessions` / `RequestFederationStatus`——响应被 `managementCoreCurrent` 过滤到 activeRegion，不重拉会短暂显示上一个 core 的值。**首屏首次区域确定**（prev=null）时跳过技能/工具目录（`includeCatalogs:false`）——二者合计约 36KB 且首屏无消费方（设置页/Agent 配置弹窗打开时自拉），切换区域保留全量
  - 实例管理数据**不预取**（`InstancePanel` 打开时按实例自行刷新描述符与状态；当前 Agent 未启用插件实例、或多实例尚未选定实例时预取必被拒，只堆出无意义的「插件实例未找到」提示）

- **重连保留时间线**：只清空运行期状态（`branchTabs`、`tasks`、`activities`、工作区 git/文件浏览器缓存）；时间线（`trunk`、`teamTimelines`、`trunkTeamId`）**不清空**——按缓存游标带 `since_seq` 增量补齐，响应 `full` 标志（首载/窗口滚出/游标超前）时整体替换（`connection.ts::startConnection` 的 `onopen` 段）
- **退避**：500ms 起、×2 递增，**上限按页面可见性分档**（`connection.ts::RETRY_CAP_VISIBLE_MS` = 3s / `RETRY_CAP_HIDDEN_MS` = 30s——前台几秒内即恢复，后台省电）；成功连接后复位 500ms
- **前台自愈（heal）**：`visibilitychange` 回到前台或 `online` 事件时主动评估连接——已断开则立即重连（复位退避，不等可能被浏览器冻结的退避定时器）；显示 OPEN 也可能半死（设备休眠期间对端已消失而本端未察觉），发 `RequestTeamsList` 探测帧，**5s** 内无任何下行帧则判死、主动关闭走标准重连（`connection.ts`）
- **上游状态轮询暂停**（`upstreams.ts`）：`/api/upstreams` 每 8s 一次；标签页隐藏时跳过轮询、回到前台立即补一次（省后台流量，回前台即最新）
- **中继半死收割**：一侧超过 90s（3 个心跳周期）无任何帧（含 Pong）→ 中继断开整条链路；另有 **10s 写超时**（`SEND_TIMEOUT`，`proxy.rs::browser_write`）——转发一帧时对端写阻塞超过 10s 即断链并 WARN 记 `sent` / `timeout_s`（冻结标签页不再挂起中继、Core 发送缓冲不再无界积压；排队帧由重连后 `since_seq` 补拉找回）。链路关闭的统计与缺口语义见 `proxy.rs::LinkStats` / `resume_gap`（见 [Panel 前端](./panel.md)「中继模型」）
- **断连期间**：边栏「连接状态」卡显示"连接中…" + 重连提示（由原顶栏状态点迁入）；输入框禁用（placeholder `未连接到 Core，暂时无法发送`）；`sendCommand` 不发送并弹 error toast；实例按钮禁用（`state.connected` 为 false 时）
- **首屏与重连的加载反馈**：聊天区消息列为空时三态区分——未连接 → 居中 spinner +「正在连接 Core…」；已连接但本次连接尚未收到 `TrunkTimeline` → 「正在加载时间线…」；收到过快照（哪怕为空）才显示「（暂无消息…）」。判定标志 `state.timelineArrivedOnce`（`onopen` 重置、任一 `TrunkTimeline` 置位）；有磁盘缓存时挂载即渲染缓存，无加载闪屏；**JS/CSS 传输期间**（挂载前）由 `index.html` 内联骨架屏给出首帧反馈（纯内联样式、主题跟随，8s 后浮出慢网提示；见 [Panel 前端](./panel.md)「前端分包与首屏」）
- **兜底对齐**：`AgentCompleted` 时若该会话连一条正式回答都没有，自动补拉 `RequestTrunkTimeline`（带当前 `team_id` 与缓存游标 `since_seq`，走增量；`state.ts` 的 `AgentCompleted` 分支）
- 协议信封 `{type: command|event, payload}`；无法解析的帧静默丢弃（`protocol.ts::parseEvent`）
- **事件流连续性**：Core 给每条下发事件附一个进程内单调递增的**扇出序号**（事件帧顶层 `seq`，见 [Panel 前端](./panel.md)「中继模型」）。`connection.ts::parseFrame` 顺带取出序号（多接入点取信封内层），dispatch 之后交 `event-seq.ts::observeEventSeq` 判定：`baseline`（本连接/本 core 首帧，中途订阅从当前进度起收，不算跳号）/ `contiguous` / `gap`（缺 N 条）/ `reset`（序号回退：Core 重启归 1 或中继补发旧帧，重取基线不报警）/ `ignored`（旧版 Core 无序号，跳过校验）——**按 core 分桶**，多接入点各有计数器。`gap` 时 `connection.ts::requestTimelineResync` 按本窗口缓存游标补拉（带 `since_seq` 走增量，不触发全量替换与 DOM 回顶；4s 节流——一次半死往往连跳数十号）；`onopen` 调 `event-seq.ts::resetEventSeq` 重建基线（重连窗口的缺口由引导 `since_seq` 负责，不该在重连首帧误判）
- **丢帧留痕**：中继链路关闭时按有无异常选 INFO/WARN，一条日志交代 `frames` / `last_seq` / `seq_gaps` / `missed` / `non_json` / `browser_stalled`；断链期间缺口由 `resume_gap` 报 `missed`；浏览器侧写超时 WARN 带已发送计数、收尾 debug 记 `sent` / `close_reason`（`proxy.rs`）。三段日志（中继链路 / 写超时 / 前端跳号）联合即可定位丢在哪一跳

## 五、侧边栏

### 5.1 临时分支卡

- 仅当当前 Agent 为**并行多会话循环模式**时显示（`loopModeOf(team) === 'parallel'`，`rail-cards.ts::branchCardVisible`）；徽标显示运行中分支数
- 分支行：脉冲 spinner（8px warn 色圆点，`branch-pulse 1s ease-in-out infinite`）+ 任务摘要（截断 24 字符）；无关闭按钮——`ReplyBranchCompleted` 或重连后自动消失
- 点击分支行 → 打开 BranchModal（分支详情）
- 卡片几何（RailStack）：头部 padding 10px/12px + 握把 + 折叠 caret（`PanelIcon chevron-down/right`，`size="xs"`），正文 padding **8px/14px**；分支行 padding 5px/8px、圆角 6px；徽标圆角 9px、10px 字、主色底（`RailStack.vue` 的样式段）

### 5.2 会话切换（入口行「会话」按钮）

会话卡已从侧边栏**删除**（避免与入口行双入口/状态分叉）——会话切换统一走
入口行「会话」按钮弹出的 `SessionSwitcher`：

- **按钮显隐**：当前智能体会话数 >1，或并行多会话模式（含「全局」入口）时显示；切换 Agent 强制关闭
- 分组规则（按会话 id `platform:scope:…` 解析）：Local / **各平台私聊** / **各平台群聊** / 其他（组键 `<platform>-dm` / `<platform>-group`，标题取 `state.adapters` 的平台名——当前只有 QQ）；并行模式顶部另有合成的**「全局」**项（跨会话合并视图，选中后发消息重定向到本地会话）
- **Local 分组含工作区通道**（会话 id 形如 `local:workspace:<ws>:local_user`，多对话后是 `local:workspace:<ws>/<conv>:local_user`——`scope_id` 第三段可含 `/` 但不含 `:`；`workspace.ts::parseWorkspaceChannelId` 先按 `:` 切段再拆 `/`。昵称 = 工作区名，标签「工作区」独立配色）：选通道 = 激活该工作区会话（写回 `echo-workspaces-{id}.json` 的 active 标记）——`active` 是所有激活来源的单一事实来源，见 [多 Agent 与会话](../core/core-agents.md)
- **排序**：组内按 `last_active` 倒序（活跃上浮）
- **归属过滤**：只显示当前 Agent 的会话（`team_id` 归一化匹配；TeamsList 未到达时他属会话不放行，避免闪现）
- **忙碌点**：会话有未完成活动时显示绿色 `●`（`--panel-ok`；`state.activities[sessionKey(core, id)].phase ≠ completed`——activities 是复合键；`SessionSwitcher.vue::isBusy`））
- 点选 = 切换会话过滤（不重拉时间线）+ 强制回会话视图
- 每个群/私聊拥有独立模型上下文（见 [多 Agent](../core/core-agents.md)「会话模型」）

## 六、Agent 切换器

锚定在会话视图入口行左侧的玻璃胶囊（头像 + 名称 + 箭头，箭头开启时旋转 180°）。几何（`AgentSwitcher.vue` 的模板与样式）：卡片高 34px、圆角 999px、`blur(20px) saturate(1.6)` 玻璃底；卡内头像 26px、菜单内 24px（圆形）；名称 120px 省略；箭头 14px、过渡 0.15s。

- 菜单**向上弹出**（`bottom: calc(100% + 8px)`、min-width 220px、最大高度 `min(60vh, 100vh-200px)`、圆角 16px、z-index 30）；点击外部或选中后关闭
- 头像 = 名称首字符，颜色按名称哈希取 7 色板；菜单项选中态 = 主色 18% 底 + 40% 描边；禁用成员灰显 + online/offline 状态点
- **切换效果**（`App.vue::onTeamSelect`）：持久化选择 → 清空分支标签 → 立即用本地缓存渲染该 Agent 时间线 → 后台 `RequestTrunkTimeline{since_seq}` 增量对齐（无缓存则全量）→ 重选本地会话（用户主动切 Agent = reason `team-select`，落该 Agent 自己的 active 投影，§3.7）
- 与"点选会话"的本质区别：切换 Agent = 切换记忆空间（重载时间线）；点选会话 = 同一记忆空间内的过滤
- 当前 Agent 被删除（或尚未选中）时：切到列表首个、重载时间线、重选本地会话（`App.vue::resetToFirstTeam` 与 teams watcher，TeamsList 为空时不校验，避免误清刚恢复的 id）；全局会话被禁用时从「全部消息」回退本地会话——两条兜底都**落在中立的默认本地会话**（`LOCAL_DEFAULT_SESSION`，reason `team-reset` / `global-fallback`），不落 active 投影（§3.7）
