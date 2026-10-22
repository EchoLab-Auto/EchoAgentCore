---
id: panel-desktop-layout
title: "桌面端布局"
group: 前端
x: 1920
y: 500
link: ["panel-desktop-overlays | 模态与覆盖层"]
---

# 桌面端布局

> **定位**：Panel 在**桌面端**（视口 ≥ 768px）的空间安排——外壳几何与全部常量、层叠秩序、顶栏与双侧边栏、界面状态的持久化归属、连接生命周期，以及「改一处会不会碰坏另一端」的适配口径。设备无关的功能契约见[交互定义](./panel-interaction.md)；浮层的层位、几何与关闭语义见[桌面端模态与覆盖层](./panel-desktop-overlays.md)；窄屏形态见[移动端布局](./panel-mobile-layout.md)。读者：要在宽屏上改几何/层叠/侧栏的开发者。
>
> **断点**：`(min-width: 768px)`（CSS 默认值即桌面）。与 ui-frame 的 `useTouchDevice().isMobile`（`innerWidth < 768`）严格互补——库判「移动」与应用判「移动」永不打架。所有桌面样式是**默认值**，窄屏规则全部收在 `@media (max-width: 767.98px)` 内，因此「桌面零回归」是结构性保证而非纪律。

## 一、外壳几何与常量

```prodoc-flow
graph TB
  App[App.vue 应用壳] --> Layout[NeumorphismLayout show-header]
  Layout --> Header[顶栏 64px<br>品牌 · 视图按钮 · 全屏 · 主题]
  Layout --> Main[主区 flex:1 · overflow hidden]
  Main --> ChatView[ChatView 会话]
  Main --> SettingsView[SettingsView 设置]
  Main --> ShellLazy[ShellPanel Shell 详情]
  ChatView --> RailL[左侧栏 324px]
  ChatView --> Msg[消息时间线]
  ChatView --> RailR[右侧栏 324px]
  ChatView --> Entry[入口行 z6]
  ChatView --> Composer[悬浮输入区 z5]
  ChatView --> OvlPane[会话区覆盖面板 z5]
  ChatView --> Pops[PanelPopover z30]
  App --> Branch[BranchModal 库模态]
  App --> Toast[Toast z 库管理]
  OvlPane --> Ovl[桌面端模态与覆盖层|/panel/panel-desktop-overlays.md]
```

| 区域 | 内容 | 几何来源 |
| --- | --- | --- |
| 顶栏 | 三段式：左品牌 / 中空 / 右操作组 | 库 `.nm-layout__header`：`height: 64px`、`padding: 0 24px`（`--nm-spacing-lg`） |
| 主区 | 当前视图组件 | `flex: 1; min-width: 0`、`overflow: hidden`——**滚动在各视图内部**，页面自身永不滚 |
| 双侧边栏 | 会话视图内左右两列卡片栈（`RailStack`） | 每列 `324px`（`ChatView.vue` 的 `RAIL_WIDTH`），上贴视图上缘、下让入口行 |
| 消息区 | `.chat-view__list` | `flex: 1; min-height: 0; overflow-y: auto`，`gap: 6px` |
| 输入区 | `.chat-composer-float` | `position: absolute`；`left/right: 12px; bottom: 12px`；`padding: 12px`、圆角 24px，毛玻璃透出下层消息 |
| 入口行 | `.entry-row` | `position: absolute; left: 20px; bottom:` **`entryBottom`**（见下）`; z-index: 6` |

### 常量表（改样式先查这里）

`entryBottom` 是整套底部布局的**唯一变量**，由 `ChatView` 的 `ResizeObserver` 实测输入区高度写入：`entryBottom = 输入区高 + 24px`（24 = 桌面输入区 `bottom: 12px` + 12px 间隙）。其余底部元素都以它表达，输入区因自动撑高而变高时全部跟随。

| 常量 | 值 | 出处 |
| --- | --- | --- |
| 顶栏高 | 64px（窄屏另算，见移动端） | 库 `.nm-layout__header` |
| 侧栏列宽 | 324px | `ChatView.vue` 的 `RAIL_WIDTH` |
| 消息区底部留白 | 190px | `.chat-view__list` 的 `padding-bottom`（≈ 输入区 5 行 156px + 余量） |
| 输入区 | `left/right 12px`、`bottom 12px`、`padding 12px`、圆角 24px | `.chat-composer-float` |
| 入口行 | `left 20px`、`bottom = entryBottom`、`gap 8px` | `.entry-row` |
| 侧栏列下界 | `entryBottom + 45px`（入口行实测 37px + 8px 间隙） | `.chat-rail` 的 `bottom` |
| 「回到底部」按钮 | 36×36；`right = trayPadRight + 16px`、`bottom = entryBottom + 45px` | `.chat-view__jump` |
| 会话区覆盖面板 | 整个会话视图（`top/bottom: 0`，横向扣边栏留白）；内容让开入口行（`reserve`）；`z-index 6` | `SessionOverlay` + `ChatView` 的 `overlayInset` / `overlayReserve` |
| Agent 切换器 | 卡片高 34px；菜单 = `PanelPopover`（宽 360px、内容 `max-height: 52vh`、锚点上方 8px） | `AgentSwitcher.vue` |
| 设置视图 | 一级菜单 168px、条目列表 250px、API 概览 ≤720px | `SettingsView.vue` / `ApiSettings.vue` |
| 卡片间距 | 槽位 12px（分隔条净占位 0） | `RailStack.vue` |

> **顶栏不承载 Shell 入口**（桌面）：Shell 详情从右侧栏 Shell 卡的「详情」按钮进入，返回时回会话视图。移动端因侧栏不渲染，Shell 改由视图菜单进入（见[移动端布局](./panel-mobile-layout.md)）。

### 覆盖库样式：特异性是硬约束

Panel 覆写 ui-frame 的样式时，**裸类选择器压不过库**——库规则带 scoped 属性选择器，等于多一级：`.nm-layout__header[data-v-006e9573]` 是 `(0,2,0)`，而应用侧 `.nm-layout__header` 只有 `(0,1,0)`。窄屏顶栏的安全区就踩过这个坑（见[移动端布局](./panel-mobile-layout.md)「库样式覆盖」）。口径：

| 情况 | 写法 |
| --- | --- |
| 库组件根节点上有自己的类（`.nm-layout__header` / `.nm-modal__mask` …） | 加 `#app` 前缀提到 `(1,1,0)+`，例如 `#app .nm-layout__header` |
| 库的**内部**结构（`.nm-card__body`） | 组件内 `:deep(...)`，同样补 `#app` 或提高层级 |
| 纯应用自有类（`.chat-view__list` / `.entry-row` …） | 直接写，无需前缀 |

## 二、层叠秩序（z-index）

| 层 | z-index | 内容 |
| --- | --- | --- |
| 基础视图 | 0 | 当前视图组件 |
| 边栏列 | 4 | 左右侧栏卡片列（`RailStack`） |
| 会话区覆盖面板 | 6 | `SessionOverlay`（入口行七项共用外壳，几何 = 整个会话视图：盖住输入区，露出侧栏；**低于入口行 7**，使按钮在面板打开时仍可点 → 换面板只需一次点击） |
| 输入区与「回到底部」 | 5 | 会话视图底部输入区、回到底部按钮（面板打开时被盖住） |
| 入口行 | 7 | 配置 / 上下文 / 清单 / 会话 / 工作区 / 实例 / 任务（+ 最左 Agent 切换）；**浮在覆盖面板之上**，面板打开时仍可点 |
| Agent 菜单 | 30 | `AgentSwitcher` 的上弹菜单（`PanelPopover`；设置视图同段 30–31） |
| 根 chip 速览 | 40 | 文件浏览器根路径 chip 悬停卡（仅文本、`pointer-events: none`） |
| 目录选择器 | 60 | `DirectoryPicker` 全屏模态（`Teleport` 到 body；高于覆盖面板） |
| 拖动幽灵 | 200 | 边栏卡片拖拽幽灵（`RailDragGhost`，压住会话区一切悬浮元素） |
| 模态 / Toast | 库管理 | `--nm-z-overlay: 400` / `--nm-z-toast: 500`，由 ui-frame 统一分配，始终高于应用层 |

清单卡是入口行覆盖面板（`SessionOverlay`）的内容，`.checklist-float` 只是面板内的 sticky 容器（`z-index: 3`），**不再**在会话视图里独立悬浮；面板宽度大于卡片所需，故在 `ChatView` 里额外限宽 640px。

## 三、主题与设计令牌

| 令牌 | 浅色 | 深色 | 用途 |
| --- | --- | --- | --- |
| `--nm-primary-color` | `#0969da` | `#4c9aff` | 主色（库默认 lime 改为 panel 蓝） |
| `--panel-accent` / `-soft` | `#0969da` / 12% 透明 | `#4c9aff` / 16% 透明 | 选中态、忙碌点、强调 |
| `--panel-ok/warn/err` | `#1a7f37` / `#9a6700` / `#cf222e` | `#3fb950` / `#d29922` / `#f85149` | 状态语义色 |
| `--panel-reasoning` / `--panel-subagent` | `#7c3aed` / `#0f766e` | `#a78bfa` / `#14b8a6` | 工作行思考 / 子代理（均 ≥4:1 对比度） |
| `--panel-hover` | 文本色 6% 混合（`color-mix`） | 同 | 悬停底 |

新增语义色**成对**定义（`:root` 与 `[data-theme='dark']`），组件内不写字面色（`web/scripts/lint-styles.mjs` 守令牌对照）。图标本体（载体分工、尺寸线宽档位）不属布局，见[图标体系](./panel-icons.md)。

## 四、顶栏、侧栏与 Agent 切换器

### 顶栏

| 元素 | 行为 |
| --- | --- |
| 品牌 `EchoAgent Panel` | 纯展示（无点击）；`.brand` 字重 700 |
| 聊天 / 设置 | 切视图，`aria-pressed` 标当前项；当前视图按标签页持久化 |
| 全屏 | 切换浏览器全屏（`FullscreenToggleButton`，`aria-pressed` 反映状态）。触摸端不再隐藏，改为 `compact` 图标态约 34px、位置在「卡片」左侧；浏览器不提供 `requestFullscreen`（iOS Safari）时点按给明确提示，见[移动端布局](./panel-mobile-layout.md)「顶栏全屏按钮」 |
| 主题 | 三态（浅色 / 自动跟随系统 / 深色），切换即持久化到 `echo-panel-theme` |

顶栏中段留空——连接状态在侧栏「连接状态」卡，**面板级信息与业务导航分离**。

### 双侧边栏

- **两列**：左 / 右各一列卡片栈（`RailStack`）；卡片可在列间拖动（`drag-enabled`），落点由 `rail-layout.ts` 持久化
- **显隐判据**：`railRegistered('left'|'right')`——「该列登记过卡片」而非「当前有可见卡片」。单卡列在卡片门控失效（无激活工作区、插件缺失）时**列不消失**，改为列内空态占位，消息区留白（`trayPadLeft/Right`）稳定不抖动
- **卡片清单**：连接状态 / 文件浏览器 / Shell / 临时分支，门控细节见[双侧边栏（chat-rail）](./panel-chat-rail.md)「卡片清单与门控」
- **拖动**：卡头 `cursor: grab`；拖动渲染跟随指针的幽灵（z200）；换列后只在新列渲染

### Agent 切换器

入口行左侧的玻璃胶囊（头像 + 名称 + 区域 + 模型 + 箭头），点开上弹菜单（z30）列出全部 Agent：选中即切换（含归属 core 解析），箭头展开时旋转 180°。多接入点下同名 Agent 按 `(core, team_id)` 区分。

**菜单容器**：与入口行其余六项一致走 `PanelPopover`（`teleport` 到 body + `position: fixed`，宽 360px、内容 `max-height: 52vh`，`open` 即 v-model；外部点击 / Escape 由它统一关闭）。**不要**改回就地 `position: absolute`——入口行是横滚容器，纵向 `overflow` 会被算成 `auto`，上弹菜单会整个被裁掉（窄屏尤其致命，见[移动端布局](./panel-mobile-layout.md)「入口行」）。

### 媒体文件路由

`GET /media/{id}` 由 Panel 后端同源提供（浏览器懒加载 + 强缓存）。除此之外前端不直连 Core 的 HTTP 面——日志（`/api/logs/{panel,core}`）与接入点管理（`/api/upstreams`）是仅有的两个面板级旁路，其余全部走 WS。

## 五、持久化与多窗口

| 键 | 内容 | 归属与存储 | 缺省 |
| --- | --- | --- | --- |
| `echo-panel-view` | 当前视图 | 窗口（`sessionStorage` 权威 + `localStorage` 作新标签默认） | `chat` |
| `echo-panel-session` | 当前会话（含工作区通道） | 窗口（**仅** `sessionStorage`，避免多窗口串台） | `local:tui::local_user` |
| `echo-panel-active-team` / `-core` | 上次使用的 Agent 与其归属 core | 窗口（同 `tab-prefs`） | 列表首个 |
| `echo-panel-theme` | 主题 | 全局（`localStorage`） | `auto` |
| `echo-panel-rail-layout` | 卡片列归属与顺序 | 全局 | 组件默认 |
| `echo-panel-rail-cards` | 卡片折叠态与高度比例 | 全局 | 组件默认 |

### 多窗口语义

多个标签页/窗口同时连 Panel 是**正常用法**：每个窗口一条 WS 连接、一份内存 `state`（`store.ts` 的响应式对象），事件广播给每条连接。

| 类别 | 例子 | 归属 |
| --- | --- | --- |
| 窗口本地 | 当前视图、当前会话、滚动位置、弹层开合、输入草稿 | 各窗口独立（`sessionStorage` + 内存） |
| 全局状态 | Agent 列表、会话时间线、实例状态、任务 | Core（广播给所有窗口） |
| 服务端动作 | 启停实例、改配置、自更新、重启 | 全局生效——**多窗口无从隔离**（属预期） |

## 六、连接生命周期与适配口径

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

### 两端共用的三个量（桌面恒为 0）

窄屏引入了三个「环境量」，桌面全部退化为 0，因此同一份公式服务两端（这也是为什么改它们**不影响桌面**）：

| 量 | 桌面 | 窄屏 | 消费方 |
| --- | --- | --- | --- |
| `env(safe-area-inset-*)` | 恒 0（无 `viewport-fit=cover` 的手机即 0） | 刘海 47px / Home 指示条 34px（iPhone 实测） | 顶栏高与内边距、输入区/入口行/覆盖层底部、消息区留白 |
| `keyboardInset` | 恒 0（只在 `isMobile` 时挂 `visualViewport` 监听） | 键盘高度（<80px 抖动被滤掉） | 输入区、入口行、消息区留白的 `bottom` / `padding-bottom` |
| `isMobile` | `false` | `true` | 侧栏渲染与留白、模板分支、默认占位文案 |

### 桌面零回归的口径

- 改窄屏只允许碰 `@media (max-width: 767.98px)` 内的规则，以及 `isMobile` 分支的模板
- 桌面几何的常量都在本文件「外壳几何与常量」与「层叠秩序」两节，且常量表是**唯一索引**——改一个数要同步一行，`panel-constants` 只做汇总
- 覆盖 ui-frame 的规则必须按「特异性」一节的写法（`#app` 前缀 / `:deep`），否则会静默失效（不报错、只是没生效）
- 回归判据：≥768px 下侧栏渲染、顶栏两视图按钮可见、无 `☰`、无「卡片」按钮、入口行不出现横向滚动提示（`entryScrollable` 恒 false）、消息区 `padding-bottom` 恒 190px
