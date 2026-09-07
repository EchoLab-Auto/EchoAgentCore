---
id: panel-layout
title: "Panel 布局与导航"
group: 前端模块
link: ["panel-interaction | Panel 布局 & 交互定义"]
x: 48
y: 48
---

# Panel 布局与导航

Panel 的布局骨架与导航形态：视图层级（四主视图 + 设置六分类）、应用外壳几何与层叠秩序、侧边栏（临时分支卡/会话卡）、Agent 切换器，以及连接生命周期（重连/自愈）。实现位置以 `web/src/` 相对路径标注。

## 一、结构总览（层级图）

本节以图表达 Panel 的层级逻辑——视图组成、布局组件树、数据流分层；各项的具体定义（行为、常量、协议命令）见后续对应小节。

### 1.1 视图层级

```prodoc-flow
graph TD
  Root[Panel 主视图] --> Chat[会话]
  Root --> Tasks[任务]
  Root --> ShellV[Shell]
  Root --> Sets[设置]
  Sets --> Api[API 设置]
  Sets --> Skills[技能]
  Sets --> Tools[工具]
  Sets --> Plugins[插件]
  Sets --> Teams[智能体]
  Sets --> Logs[日志]
```

- 四主视图仅经顶栏导航切换（§二）；设置视图的六个分类走内部一级菜单（§九）
- 模态与覆盖层（Sudo / Branch / Agent 配置 / 上下文）浮于全部视图之上（§八）；侧边栏点选会话强制回会话视图

### 1.2 布局与组件树

```prodoc-flow
graph TD
  App[App.vue 应用壳] --> Layout[NeumorphismLayout 外壳]
  Layout --> Header[顶栏：品牌 · 连接状态 · 导航 · 主题]
  Layout --> Sider[侧边栏 PanelSidebar]
  Layout --> Main[主区：当前视图组件]
  Sider --> BranchCard[临时分支卡]
  Sider --> SessionCard[会话卡 SessionGroups]
  Main --> ChatView[ChatView 会话]
  Main --> SettingsView[SettingsView 设置]
  Main --> TasksPanel[TasksPanel 任务]
  Main --> ShellPanel[ShellPanel Shell]
  ChatView --> MsgList[消息列表]
  ChatView --> EntryRow[活动浮条 · 入口行]
  ChatView --> Composer[悬浮输入区]
  ChatView --> Pops[弹出层：清单 · QQ 适配器]
  ChatView --> Overs[覆盖层：上下文 · Agent 配置]
  App --> Modals[模态：Sudo · Branch]
  App --> Toasts[ToastProvider 右上]
```

几何与层叠常量见 §三；设置视图内部为 一级菜单 → 工具条 → 列表/详情 三段（§九）。

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

- Core 是状态唯一事实来源；断连重连后全量重建（§四）；唯一乐观例外 = 取消任务（§7.6、§十一）
- HTTP 仅有三条旁路（QQ 状态/二维码、日志），其余全部走 WS（§十一）

## 二、视图与导航

Panel 有四个主视图，仅经顶栏导航按钮切换；地址栏不承载视图状态（无路由）。

```prodoc-flow
graph LR
  Chat[会话视图] -->|顶栏: 任务| Tasks[任务视图]
  Chat -->|顶栏: Shell| Shell[Shell 视图]
  Chat -->|顶栏: 设置| Sets[设置视图]
  Tasks -->|顶栏: 会话| Chat
  Shell -->|顶栏: 会话| Chat
  Sets -->|顶栏: 会话| Chat
  Side[侧边栏点选会话] -->|强制回到| Chat
```

- 当前视图持久化于 `localStorage: echo-panel-view`（非法值回退 `chat`；旧值 `caps`/`logs` 自动迁移为 `settings`），刷新后恢复（`App.vue:31-40, 92-99`）
- 主区同一时间只渲染一个视图组件（`App.vue:311-323`）
- 点选侧边栏会话项**强制切回会话视图**（`App.vue:154-156`）
- 任务按钮在有运行中任务时显示计数徽标 `任务(N)`（`App.vue:150-152, 278`）

> 2026-09-04 起原「资源」「日志」视图与 API 设置弹窗合并为「设置」视图（§九），顶栏设置下拉随之移除。

## 三、应用外壳与布局

### 3.1 布局区域与几何

外壳 = ui-frame `NeumorphismLayout`（`App.vue:243-328`）：`:sider-width="264"`、`:collapsed-width="0"`、`height="100%"`。

| 区域 | 内容 | 几何 |
|---|---|---|
| 顶栏左 | 品牌 "EchoAgent Panel"（`.brand` 字重 700） | 顶栏分左中右三段，右侧操作 `margin-left:auto` 顶齐 |
| 顶栏中 | 连接状态点 + 模型名 + 忙碌徽标 | 状态组 gap 6px、13px 次要色 |
| 顶栏右 | 四视图导航（聊天/任务/Shell/设置）、主题开关 | 操作组 gap 6px、允许折行 |
| 侧边栏 | 临时分支卡 + 会话卡 | 固定 264px，可折叠为 0（`.nm-layout--sider-collapsed` 兜底 `width:0!important` + `overflow:hidden`） |
| 主区 | 当前视图组件 | `flex:1` + `min-width:0`，纵向 flex，`overflow:hidden`（滚动交给视图内部） |

页面级约束（`styles.css:43-57, 147-154, 581-606`）：

- `html/body/#app` 高 100%、禁止页面级滚动（`overflow:hidden` + `overscroll-behavior:none`）；背景 `var(--nm-bg-color)`
- sider 槽位去背景/边框/阴影（由卡片自带新拟态）；内容栈 `.panel-sider` 纵向 flex、gap 14px、padding 12px、自身 `overflow-y:auto`
- 自定义样式层只写类选择器，**禁止裸元素选择器**（`button/input/…` 会污染库组件）——`styles.css` 头注

### 3.2 层叠秩序（z-index）

| 层 | z-index | 内容 |
|---|---|---|
| 基础视图 | 0 | 当前视图组件 |
| 清单浮层 | 3 | 会话视图内清单卡（`.checklist-float`） |
| Agent 菜单 | 30 | AgentSwitcher 上弹菜单 |
| 模态 / Toast | 库管理 | SudoModal、BranchModal、ToastProvider 等由 ui-frame 统一分配，始终高于应用层 |

会话视图内的悬浮元素（输入区、入口行、弹出层）不参与全局 z-index 竞争，靠 DOM 顺序与定位叠放，详见 §7.6/§7.7。

### 3.3 主题与设计令牌

视觉体系来自 `@echolab-auto/ui-frame`（`--nm-*` 令牌 + `[data-theme='dark']` 机制），Panel 在其上覆盖（`styles.css:13-39`）：

| 令牌 | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--nm-primary-color` | `#0969da` | `#4c9aff` | 主色（库默认 lime 改为 panel 蓝） |
| `--panel-accent` / `-soft` | `#0969da` / 12% 透明 | `#4c9aff` / 16% 透明 | 选中态、忙碌点、徽标底 |
| `--panel-ok/warn/err` | `#1a7f37/#9a6700/#cf222e` | `#3fb950/#d29922/#f85149` | 状态语义色 |
| `--panel-hover` | 文本色 6% 混合（`color-mix`） | 同 | 悬停底 |

### 3.4 顶栏交互

- **连接指示**：`已连接`（online 绿点）/ `连接中…`（connecting 呼吸点），驱动源 `state.connected`（`App.vue:249-252`）
- **模型显示**：`state.model`（无数据时 `—`）；任一会话处于 thinking/tool/subagent 相位时追加 `忙碌 ×N` 警告标签（`App.vue:143-148, 254-257`）
- **设置入口**：顶栏导航「设置」直达设置视图（§九）——2026-09-04 起不再有设置下拉与 API 弹窗
- **主题开关**：三态循环（浅色 → 跟随系统 → 深色），持久化 `localStorage: echo-panel-theme`；外壳默认 `auto`，index.html 内联脚本防闪烁

### 3.5 持久化的界面状态

| localStorage 键 | 内容 | 缺省 |
|---|---|---|
| `echo-panel-view` | 当前视图 | `chat` |
| `echo-panel-active-team` | 当前 Agent（刷新/重连后停留原 Agent） | 主 Agent |
| `echo-panel-sidebar-collapsed` | 侧边栏折叠 `'1'/'0'` | 展开 |
| `echo-panel-theme` | 主题三态 | `auto` |

所有读写包 try/catch（隐私模式降级为不持久化）。侧边栏卡片自身的展开/折叠**不持久化**（组件内状态，默认双卡展开）。

## 五、侧边栏

### 5.1 临时分支卡

- 仅当当前 Agent 为 **chatbot 编排模式**时显示（`orchestrationModeOf(team) === 'chatbot'`，`PanelSidebar.vue:17-24`）；徽标显示运行中分支数
- 分支行：脉冲 spinner（8px warn 色圆点，`branch-pulse 1s ease-in-out infinite`）+ 任务摘要（截断 24 字符）；无关闭按钮——`ReplyBranchCompleted` 或重连后自动消失
- 点击分支行 → 打开 BranchModal（分支详情）
- 卡片几何：头部 padding 12px/14px + 11px 折叠 caret，正文 padding 4px/12px/12px；分支行 padding 5px/8px、圆角 6px；徽标圆角 9px、10px 字、主色底（`styles.css:125-145, 222-241`）

### 5.2 会话卡与分组

仅当当前 Agent 为 **chatbot 编排模式**时显示（single 模式会话卡与分支卡整体隐藏）。分组规则（按会话 id `platform:scope:…` 解析）：

| 分组 | 内容 | 备注 |
|---|---|---|
| 全局 | 合成的「全部消息」项（预览"共享同一 trunk 上下文"） | 仅 chatbot 模式；选中后发消息会重定向到本地会话 |
| Local | 本地会话（`local:tui::local_user`） | 启动默认选中 |
| QQ 私聊 / QQ 群 / 其他 | 按平台归组 | 空分组隐藏 |

- **排序**：组内按 `last_active` 倒序（活跃上浮）
- **归属过滤**：只显示当前 Agent 的会话（`team_id` 归一化匹配；TeamsList 未到达时他属会话不放行，避免闪现）
- **忙碌点**：会话有未完成活动时显示主色 `●`（`state.activities[id].phase ≠ completed`）
- **预览行**：`session.last_message`
- 点选 = 切换会话过滤（不重拉时间线）+ 强制回会话视图
- 会话项几何：padding 6px/8px、圆角 6px、3px 透明左边框；选中 = 左边框 `--panel-accent` + `--panel-accent-soft` 底；分组 tag 10px 字、圆角 8px，配色 全局 `#64748b` / Local `#0ea5e9` / QQ 私聊 `#2ea46e` / QQ 群 `#f59e0b` / 其他 `#8b5cf6`（`styles.css:180-219`）

## 六、Agent 切换器

锚定在会话视图入口行左侧的玻璃胶囊（头像 + 名称 + 箭头，箭头开启时旋转 180°）。几何（`AgentSwitcher.vue:120-228`）：卡片高 34px、圆角 999px、`blur(20px) saturate(1.6)` 玻璃底；卡内头像 26px、菜单内 24px（圆形）；名称 120px 省略；箭头 14px、过渡 0.15s。

- 菜单**向上弹出**（`bottom: calc(100% + 8px)`、min-width 220px、最大高度 `min(60vh, 100vh-200px)`、圆角 16px、z-index 30）；点击外部或选中后关闭
- 头像 = 名称首字符，颜色按名称哈希取 7 色板；菜单项选中态 = 主色 18% 底 + 40% 描边；禁用成员灰显 + online/offline 状态点
- **切换效果**（`App.vue:68-93` onTeamSelect）：持久化选择 → 清空分支标签 → 立即用本地缓存渲染该 Agent 时间线 → 后台 `RequestTrunkTimeline{since_seq}` 增量对齐（无缓存则全量）→ 重选本地会话
- 与"点选会话"的本质区别：切换 Agent = 切换记忆空间（重载时间线）；点选会话 = 同一记忆空间内的过滤
- 当前 Agent 被删除时：回退主 Agent、重载时间线、重选本地会话（`App.vue:203-224`，TeamsList 为空时不校验，避免误清刚恢复的 id）；全局会话被禁用时从「全部消息」回退本地会话（`App.vue:191-201`）

## 四、连接生命周期

```prodoc-flow
graph LR
  Closed[未连接] -->|ws onopen| Open[已连接: Bootstrap]
  Open -->|onclose| Toast[toast: 与后端断开…]
  Toast -->|500ms| Retry[重连尝试]
  Retry -->|失败: 退避×2 上限30s| Retry
  Retry -->|成功| Rebuild[清空运行期+时间线状态]
  Rebuild --> Open
```

- WS 地址 `ws(s)://{host}/ws`（随页面协议）；单例连接（`connection.ts`）
- **Bootstrap 命令组**（每次 onopen 按序发送）：`RequestState` → `RequestTrunkTimeline{team_id: 上次Agent}` → `RequestAdapterStatus` → `RequestQqFilterConfig` → `RequestTeamsList` → `RequestShellSessions`
- **重连清空**：`branchTabs`、`tasks`、`activities` 以及时间线状态（`trunk`、`teamTimelines`、`trunkTeamId`）全部清空后全量重建——断连期间错过的实时事件与就地更新无法对齐，全量是唯一安全恢复路径
- **退避**：500ms 起、×2 递增、上限 30s；成功连接后复位 500ms
- **前台自愈（heal）**：`visibilitychange` 回到前台或 `online` 事件时主动评估连接——已断开则立即重连（复位退避，不等可能被浏览器冻结的退避定时器）；显示 OPEN 也可能半死（设备休眠期间对端已消失而本端未察觉），发 `RequestTeamsList` 探测帧，**5s** 内无任何下行帧则判死、主动关闭走标准重连（`connection.ts`）
- **中继半死收割**：一侧超过 90s（3 个心跳周期）无任何帧（含 Pong）→ 中继断开整条链路（`proxy.rs`；设备休眠留下的僵尸连接因此被清理，唤醒后重连拿到干净状态）
- **断连期间**：顶栏显示"连接中…"；输入框禁用（placeholder `未连接到 Core，暂时无法发送`）；`sendCommand` 不发送并弹 error toast；QQ 面板按钮禁用
- **兜底对齐**：`AgentCompleted` 时若该会话最后一条不是正式回答，自动补拉 `RequestTrunkTimeline`（带当前 team_id，`state.ts:595-608`）
- 协议信封 `{type: command|event|sudo_password, payload}`；无法解析的帧静默丢弃（`protocol.ts:437-459`）

