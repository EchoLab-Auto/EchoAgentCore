---
id: panel-layout
title: "Panel 布局与导航"
group: 前端模块
x: 1186
y: 1164
---

# Panel 布局与导航

Panel 的布局骨架与导航形态：视图层级（四主视图 + 设置六分类）、应用外壳几何与层叠秩序、侧边栏（临时分支卡）、Agent 切换器与会话切换（入口行），以及连接生命周期（重连/自愈）。实现位置以 `web/src/` 相对路径标注。

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

- 三主视图仅经顶栏导航切换（§二）；设置视图的六个分类走内部一级菜单（§九）；任务视图已并入会话视图入口行「任务」弹层（2026-09-19，§7.7）
- 模态与覆盖层（Sudo / Branch / Agent 配置 / 上下文）浮于全部视图之上（§八）；侧边栏点选会话强制回会话视图

### 1.2 布局与组件树

```prodoc-flow
graph TD
  App[App.vue 应用壳] --> Layout[NeumorphismLayout 外壳]
  Layout --> Header[顶栏：品牌 · 连接状态 · 导航 · 主题]
  Layout --> Main[主区：当前视图组件]
  ChatView --> Rail[右侧边栏 chat-rail：文件浏览器卡 · 临时分支卡]
  Main --> ChatView[ChatView 会话]
  Main --> SettingsView[SettingsView 设置]
  Main --> ShellPanel[ShellPanel Shell]
  ChatView --> MsgList[消息列表]
  ChatView --> EntryRow[活动浮条 · 入口行]
  ChatView --> Composer[悬浮输入区]
  ChatView --> Pops[弹出层：清单 · QQ 适配器 · 任务 TasksPanel]
  ChatView --> Overs[会话区内弹层：上下文 · Agent 配置]
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
- HTTP 仅有两条日志旁路（`/api/logs/{panel,core}`），其余全部走 WS（§十一）——QQ 登录/二维码已于 2026-09-13 迁到 WS（Core 代理）

## 二、视图与导航

Panel 有三个主视图，仅经顶栏导航按钮切换；地址栏不承载视图状态（无路由）。

```prodoc-flow
graph LR
  Chat[会话视图] -->|顶栏: Shell| Shell[Shell 视图]
  Chat -->|顶栏: 设置| Sets[设置视图]
  Shell -->|顶栏: 会话| Chat
  Sets -->|顶栏: 会话| Chat
  Side[侧边栏点选会话] -->|强制回到| Chat
```

- 当前视图持久化于 `localStorage: echo-panel-view`（非法值回退 `chat`；旧值 `caps`/`logs` 自动迁移为 `settings`，旧值 `tasks` 迁移为 `chat`），刷新后恢复（`App.vue:31-40, 92-99`）
- 主区同一时间只渲染一个视图组件
- 点选会话项（入口行「会话」切换器）**强制切回会话视图**（`App.vue` selectSession）
- 原顶栏「任务」视图已移除（2026-09-19）：任务入口并入会话视图入口行「任务」按钮（徽标 = 当前会话运行中任务数），弹层复用 `TasksPanel` 且只对应当前 agent 的当前会话（§7.7）

> 2026-09-04 起原「资源」「日志」视图与 API 设置弹窗合并为「设置」视图（§九），顶栏设置下拉随之移除。

## 三、应用外壳与布局

### 3.1 布局区域与几何

外壳 = ui-frame `NeumorphismLayout`（`App.vue:243-328`）：`:sider-width="264"`、`:collapsed-width="0"`、`height="100%"`。

| 区域 | 内容 | 几何 |
|---|---|---|
| 顶栏左 | 品牌 "EchoAgent Panel"（`.brand` 字重 700） | 顶栏分左中右三段，右侧操作 `margin-left:auto` 顶齐 |
| 顶栏中 | 连接状态点 | 状态组 gap 6px、13px 次要色 |
| 顶栏右 | 三视图导航（聊天/Shell/设置）、主题开关 | 操作组 gap 6px、允许折行 |
| 右侧边栏 | 会话视图内 chat-rail 常驻卡片栈（RailStack，悬浮圆角矩形磨砂玻璃卡）：文件浏览器卡（workspace 插件 + 激活会话时，最上方）→ Shell 卡（常驻，只含会话列表 + 「详情」按钮，终端内容经详情视图查看）→ 临时分支卡（并行模式）；每张卡可折叠为横条、相邻展开卡间的分隔条可拖动调整上下空间分配（折叠态与高度比例均持久化） | 324px 透明容器层（无底色无描边），下界 = 输入框外边框上方 5px（随输入框高度动态）；左侧边栏已于 2026-09-19 移除 |
| 主区 | 当前视图组件 | `flex:1` + `min-width:0`，纵向 flex，`overflow:hidden`（滚动交给视图内部） |

页面级约束（`styles.css:43-57, 147-154, 581-606`）：

- `html/body/#app` 高 100%、禁止页面级滚动（`overflow:hidden` + `overscroll-behavior:none`）；背景 `var(--nm-bg-color)`
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
- **设置入口**：顶栏导航「设置」直达设置视图（§九）——2026-09-04 起不再有设置下拉与 API 弹窗
- **主题开关**：三态循环（浅色 → 跟随系统 → 深色），持久化 `localStorage: echo-panel-theme`；外壳默认 `auto`，index.html 内联脚本防闪烁

### 3.5 持久化的界面状态

| localStorage 键 | 内容 | 缺省 |
|---|---|---|
| `echo-panel-view` | 当前视图 | `chat` |
| `echo-panel-active-team` | 当前 Agent（刷新/重连后停留原 Agent） | 列表首个 |
| `echo-panel-sidebar-collapsed` | 侧边栏折叠 `'1'/'0'` | 展开 |
| `echo-panel-theme` | 主题三态 | `auto` |

所有读写包 try/catch（隐私模式降级为不持久化）。侧边栏卡片自身的展开/折叠**不持久化**（组件内状态，默认双卡展开）。

## 五、侧边栏

### 5.1 临时分支卡

- 仅当当前 Agent 为**并行多会话循环模式**时显示（`loopModeOf(team) === 'parallel'`，`PanelSidebar.vue:17-24`）；徽标显示运行中分支数
- 分支行：脉冲 spinner（8px warn 色圆点，`branch-pulse 1s ease-in-out infinite`）+ 任务摘要（截断 24 字符）；无关闭按钮——`ReplyBranchCompleted` 或重连后自动消失
- 点击分支行 → 打开 BranchModal（分支详情）
- 卡片几何：头部 padding 12px/14px + 11px 折叠 caret，正文 padding 4px/12px/12px；分支行 padding 5px/8px、圆角 6px；徽标圆角 9px、10px 字、主色底（`styles.css:125-145, 222-241`）

### 5.2 会话切换（入口行「会话」按钮，2026-09-14 起）

会话卡已从侧边栏**删除**（避免与入口行双入口/状态分叉）——会话切换统一走
入口行「会话」按钮弹出的 `SessionSwitcher`：

- **按钮显隐**：当前智能体会话数 >1，或并行多会话模式（含「全局」入口）时显示；切换 Agent 强制关闭
- 分组规则（按会话 id `platform:scope:…` 解析）：Local / QQ 私聊 / QQ 群 / 其他；并行模式顶部另有合成的**「全局」**项（跨会话合并视图，选中后发消息重定向到本地会话）
- **Local 分组含工作区通道**（`local:workspace:<id>:local_user`，昵称 = 工作区名，标签「工作区」独立配色）：选通道 = 激活对应工作区、选默认会话 = 取消激活；active 回推变化仅当当前停留在 Local 来源会话上时跟随切换（2026-09-14，见 [多 Agent](./core-agents.md)§工作区会话与项目通道）
- **排序**：组内按 `last_active` 倒序（活跃上浮）
- **归属过滤**：只显示当前 Agent 的会话（`team_id` 归一化匹配；TeamsList 未到达时他属会话不放行，避免闪现）
- **忙碌点**：会话有未完成活动时显示主色 `●`（`state.activities[id].phase ≠ completed`）
- 点选 = 切换会话过滤（不重拉时间线）+ 强制回会话视图
- 每个群/私聊拥有独立模型上下文（见 [多 Agent](./core-agents.md)§会话模型）

## 六、Agent 切换器

锚定在会话视图入口行左侧的玻璃胶囊（头像 + 名称 + 箭头，箭头开启时旋转 180°）。几何（`AgentSwitcher.vue:120-228`）：卡片高 34px、圆角 999px、`blur(20px) saturate(1.6)` 玻璃底；卡内头像 26px、菜单内 24px（圆形）；名称 120px 省略；箭头 14px、过渡 0.15s。

- 菜单**向上弹出**（`bottom: calc(100% + 8px)`、min-width 220px、最大高度 `min(60vh, 100vh-200px)`、圆角 16px、z-index 30）；点击外部或选中后关闭
- 头像 = 名称首字符，颜色按名称哈希取 7 色板；菜单项选中态 = 主色 18% 底 + 40% 描边；禁用成员灰显 + online/offline 状态点
- **切换效果**（`App.vue:68-93` onTeamSelect）：持久化选择 → 清空分支标签 → 立即用本地缓存渲染该 Agent 时间线 → 后台 `RequestTrunkTimeline{since_seq}` 增量对齐（无缓存则全量）→ 重选本地会话
- 与"点选会话"的本质区别：切换 Agent = 切换记忆空间（重载时间线）；点选会话 = 同一记忆空间内的过滤
- 当前 Agent 被删除（或尚未选中）时：切到列表首个、重载时间线、重选本地会话（`App.vue:203-224`，TeamsList 为空时不校验，避免误清刚恢复的 id）；全局会话被禁用时从「全部消息」回退本地会话（`App.vue:191-201`）

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
- **Bootstrap 命令组**（每次 onopen 按序发送）：`RequestState` →（有保存的 Agent 时）`RequestTrunkTimeline{team_id: 上次Agent}` → `RequestAdapterStatus` → `RequestTeamsList` → `RequestShellSessions`。
  - 去主智能体后 `team_id` 必填：没有保存过 Agent 时**跳过**时间线请求（发了必被 Core 拒绝），等 `TeamsList` 到达后由 App watcher 用列表首个发起
  - QQ 过滤配置**不预取**（QqPanel/QqLoginSection 打开时按实例自行刷新；无 QQ 或多实例未指定实例时预取必被拒，产生无意义的"QQ 适配器未找到"提示）
- **重连清空**：`branchTabs`、`tasks`、`activities` 以及时间线状态（`trunk`、`teamTimelines`、`trunkTeamId`）全部清空后全量重建——断连期间错过的实时事件与就地更新无法对齐，全量是唯一安全恢复路径
- **退避**：500ms 起、×2 递增、上限 30s；成功连接后复位 500ms
- **前台自愈（heal）**：`visibilitychange` 回到前台或 `online` 事件时主动评估连接——已断开则立即重连（复位退避，不等可能被浏览器冻结的退避定时器）；显示 OPEN 也可能半死（设备休眠期间对端已消失而本端未察觉），发 `RequestTeamsList` 探测帧，**5s** 内无任何下行帧则判死、主动关闭走标准重连（`connection.ts`）
- **中继半死收割**：一侧超过 90s（3 个心跳周期）无任何帧（含 Pong）→ 中继断开整条链路（`proxy.rs`；设备休眠留下的僵尸连接因此被清理，唤醒后重连拿到干净状态）
- **断连期间**：顶栏显示"连接中…"；输入框禁用（placeholder `未连接到 Core，暂时无法发送`）；`sendCommand` 不发送并弹 error toast；QQ 面板按钮禁用
- **兜底对齐**：`AgentCompleted` 时若该会话最后一条不是正式回答，自动补拉 `RequestTrunkTimeline`（带当前 team_id，`state.ts:595-608`）
- 协议信封 `{type: command|event|sudo_password, payload}`；无法解析的帧静默丢弃（`protocol.ts:437-459`）

