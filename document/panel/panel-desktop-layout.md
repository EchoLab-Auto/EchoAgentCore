---
id: panel-desktop-layout
title: "电脑端布局"
group: 前端
x: 3520
y: 780
---

# 电脑端布局

> **定位**：Panel 在**电脑端**（视口 ≥ 768px）的空间安排——外壳几何、层叠秩序、双侧边栏、卡片、顶栏、Agent 切换器、连接生命周期。设备无关的功能契约见[交互定义](./panel-interaction.md)；窄屏形态见[移动端布局](./panel-mobile-layout.md)。读者：要在宽屏上改几何/层叠/侧栏的开发者。
>
> **断点**：`min-width: 768px`。与 ui-frame 的 `useTouchDevice().isMobile`（`innerWidth < 768`）严格互补——库判「移动」与应用判「移动」永不打架。所有桌面样式是**默认值**，移动端规则全部收在 `@media (max-width: 767.98px)` 内，因此「桌面零回归」是结构性保证而非纪律。

## 一、外壳几何

```prodoc-flow
graph TB
  App[App.vue 应用壳] --> Layout[NeumorphismLayout show-header]
  Layout --> Header[顶栏 64px<br>品牌 · 视图按钮 · 全屏 · 主题]
  Layout --> Main[主区 flex:1]
  Main --> ChatView[ChatView 会话]
  Main --> SettingsView[SettingsView 设置]
  Main --> ShellLazy[ShellPanel Shell 详情]
  ChatView --> RailL[左侧栏 324px]
  ChatView --> Msg[消息时间线]
  ChatView --> RailR[右侧栏 324px]
  ChatView --> Entry[入口行 z6]
  ChatView --> Composer[悬浮输入区 z5]
  ChatView --> Pops[PanelPopover z30]
  App --> Branch[BranchModal 库模态]
  App --> Toast[Toast z 库管理]
```

| 区域 | 内容 | 几何 |
| --- | --- | --- |
| 顶栏左 | 品牌 `EchoAgent Panel`（`.brand`，字重 700） | 三段式：左 `flex: 0 0 auto` / 中空 / 右 `margin-left: auto` |
| 顶栏中 | （空——连接状态在侧栏「连接状态」卡，面板级信息与业务导航分离） | — |
| 顶栏右 | 视图按钮「聊天」「设置」+ 全屏 + 主题三态开关 | `display: flex; gap: 6px` |
| 双侧边栏 | 会话视图内左右两列 chat-rail 常驻卡片栈（`RailStack`） | 每列 **324px**；`railRegistered()` 判显隐（登记过卡片即渲染，避免单卡门控失效导致整列抖动） |
| 主区 | 当前视图组件 | `flex: 1; min-width: 0`，纵向 flex，`overflow: hidden`（滚动在各视图内部） |
| 消息区 | `.chat-view__list` | `flex: 1; min-height: 0; overflow-y: auto`；`padding-bottom: 190px` 预留输入区（5 行 ≈156px + 边距） |
| 输入区 | `.chat-composer-float` | `position: absolute`；`left/right: 12px; bottom: 12px`；毛玻璃透出下层消息 |
| 入口行 | `.entry-row` | `position: absolute; left: 20px; bottom: <输入区上方 8px>`，`z-index: 6` |

> **顶栏不承载 Shell 入口**（桌面）：Shell 详情从右侧栏 Shell 卡的「详情」按钮进入，返回时回会话视图。移动端因侧栏不渲染，Shell 改由视图菜单进入（见[移动端布局](./panel-mobile-layout.md)）。

### 层叠秩序（z-index）

| 层 | z-index | 内容 |
| --- | --- | --- |
| 基础视图 | 0 | 当前视图组件 |
| 边栏列 | 4 | 左右侧栏卡片列（`RailStack`） |
| 输入区与「回到底部」 | 5 | 会话视图底部输入区、回到底部按钮 |
| 入口行 | 6 | 配置 / 上下文 / 清单 / 会话 / 工作区 / 任务 / 实例 |
| 入口行弹出层 | 30 | `PanelPopover`（清单 / 会话 / 工作区 / 实例 / 任务共用容器，`teleport` 到 body + fixed） |
| Agent 菜单 | 30 | `AgentSwitcher` 上弹菜单（设置视图同段 30–31） |
| 根 chip 速览 | 40 | 文件浏览器根路径 chip 悬停卡（仅文本、`pointer-events: none`） |
| 浮层遮罩 | 50 | 会话区覆盖层（上下文 / Agent 配置） |
| 拖动幽灵 | 200 | 边栏卡片拖拽幽灵（`RailDragGhost`） |
| 模态 / Toast | 库管理 | BranchModal、ToastProvider 由 ui-frame 统一分配，始终高于应用层 |

清单卡已并入入口行弹层——`.checklist-float` 只是弹层内的 sticky 容器（`z-index: 3`），**不再**在会话视图里独立悬浮。

## 二、主题与设计令牌

| 令牌 | 浅色 | 深色 | 用途 |
| --- | --- | --- | --- |
| `--nm-primary-color` | `#0969da` | `#4c9aff` | 主色（库默认 lime 改为 panel 蓝） |
| `--panel-accent` / `-soft` | `#0969da` / 12% 透明 | `#4c9aff` / 16% 透明 | 选中态、忙碌点、强调 |
| `--panel-ok/warn/err` | `#1a7f37` / `#9a6700` / `#cf222e` | `#3fb950` / `#d29922` / `#f85149` | 状态语义色 |
| `--panel-hover` | 文本色 6% 混合（`color-mix`） | 同 | 悬停底 |

新增语义色**成对**定义（`:root` 与 `[data-theme='dark']`），组件内不写字面色（`web/scripts/lint-styles.mjs` 守令牌对照）。

## 三、双侧边栏与卡片

- **两列**：左 / 右各一列卡片栈（`RailStack`）；卡片可在列间拖动（`drag-enabled`），落点由 `rail-layout.ts` 持久化
- **显隐判据**：`railRegistered('left'|'right')`——「该列登记过卡片」而非「当前有可见卡片」。单卡列在卡片门控失效（无激活工作区、插件缺失）时**列不消失**，改为列内空态占位，消息区留白稳定不抖动（`trayPadLeft/Right` 随之）
- **卡片清单**：连接状态 / 文件浏览器 / Shell / 临时分支 / 任务等，具体见[会话视图](./panel-chat.md)「侧栏与多接入点」
- **拖动**：卡片头 `cursor: grab`；拖动时渲染跟随指针的幽灵（z200）；换列后只在新列渲染（`rail-layout.ts` 决定归属）

### 顶栏交互

| 元素 | 行为 |
| --- | --- |
| 品牌 | 纯展示（无点击） |
| 聊天 / 设置 | 切视图，`aria-pressed` 标当前项；当前视图按标签页持久化 |
| 全屏 | 切换浏览器全屏（`FullscreenToggleButton`）；触摸端由 CSS 隐藏（iOS Safari 不支持元素全屏） |
| 主题 | 三态（浅色 / 自动跟随系统 / 深色），切换即持久化到 `echo-panel-theme` |

### Agent 切换器

入口行左侧的玻璃胶囊（头像 + 名称 + 区域 + 模型 + 箭头），点开上弹菜单（z30）列出全部 Agent：选中即切换（含归属 core 解析），箭头在展开时旋转 180°。多接入点下同名 Agent 按 `(core, team_id)` 区分。

### 媒体文件路由

`GET /media/{id}` 由 Panel 后端同源提供（浏览器懒加载 + 强缓存）。除此之外前端不直连 Core 的 HTTP 面——日志（`/api/logs/{panel,core}`）与接入点管理（`/api/upstreams`）是仅有的两个面板级旁路，其余全部走 WS。

## 四、持久化与多窗口

| 键 | 内容 | 归属与存储 | 缺省 |
| --- | --- | --- | --- |
| `echo-panel-view` | 当前视图 | 窗口（`sessionStorage` 权威 + `localStorage` 作新标签默认） | `chat` |
| `echo-panel-session` | 当前会话（含工作区通道） | 窗口（**仅** `sessionStorage`，避免多窗口串台） | `local:tui::local_user` |
| `echo-panel-active-team` / `-core` | 上次使用的 Agent 与其归属 core | 窗口（同 `tab-prefs`） | 列表首个 |
| `echo-panel-theme` | 主题 | 全局（`localStorage`） | `auto` |
| `echo-panel-rail-layout` | 卡片列归属与折叠态 | 全局 | 组件默认 |

### 多窗口语义

多个标签页/窗口同时连 Panel 是**正常用法**：每个窗口一条 WS 连接、一份内存 `state`（`store.ts` 的响应式对象），事件广播给每条连接。

| 类别 | 例子 | 归属 |
| --- | --- | --- |
| 窗口本地 | 当前视图、当前会话、滚动位置、弹层开合、输入草稿 | 各窗口独立（`sessionStorage` + 内存） |
| 全局状态 | Agent 列表、会话时间线、实例状态、任务 | Core（广播给所有窗口） |
| 服务端动作 | 启停实例、改配置、自更新、重启 | 全局生效——**多窗口无从隔离**（属预期） |

## 五、连接生命周期

```prodoc-flow
graph LR
  Closed[未连接] -->|onopen| Boot[Bootstrap 命令组]
  Boot --> Live[已连接]
  Live -->|断线| Backoff[退避重连]
  Backoff -->|成功| Boot
  Live -->|可见性变化| Live
```

1. **Bootstrap 命令组**（每次 onopen 按序发送）：`RequestState` → `RequestTeamsList`（只读发现命令，**裸广播全部在线接入点**）→ `RequestInstanceStatus` → `RequestShellSessions` →（有保存的 Agent 时）`RequestTrunkTimeline`
2. **首屏优先级**：`RequestInstanceStatus` / `RequestShellSessions` 两条单值管理面命令**仅当目标区域已解析**（刷新/重连场景下 `activeRegion` 自 localStorage 还原）时发送；首访区域未定时跳过——多接入点下会因解析不出目标被拒并误弹「尚未选择 agent」
3. **退避**：`500ms × 2^n`，上限按页面可见性取（可见 3s / 隐藏 30s）；重连期间命令进队列（上限 128 条），解析不出目标则拒发并提示
4. **断线保留时间线**：只清空运行期状态（`branchTabs`、`tasks`、`activities`、工作区 git/文件浏览器缓存）；时间线按缓存游标增量补齐

### 桌面零回归的口径

- 改移动端只允许碰 `@media (max-width: 767.98px)` 内的规则，以及 `isMobile` 分支的模板
- 桌面几何的常量（324px 侧栏、64px 顶栏、190px 消息底部留白、z-index 表）都在本文件「外壳几何」与「层叠秩序」两节，改动需同步本节
- 回归判据：≥768px 下侧栏渲染、顶栏两视图按钮可见、入口行不出现横向滚动提示（`entryScrollable` 恒 false）
