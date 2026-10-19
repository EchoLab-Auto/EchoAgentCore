---
id: panel-mobile-layout
title: "移动端布局"
group: 前端
x: 3840
y: 780
---

# 移动端布局

> **定位**：Panel 在**移动端**（视口 < 768px，竖屏为主）的完整空间设计与交互重排。这不是「把桌面样式收窄」，而是**按拇指可达性重新排布信息架构**：视图切换进底部 Sheet、卡片进抽屉、入口行只保留一条可横滚的带。设备无关的功能契约见[交互定义](./panel-interaction.md)，宽屏形态见[电脑端布局](./panel-desktop-layout.md)。
>
> **读者**：要改窄屏形态的开发者。本文的每个结论都对应 `web/src` 里的具体规则与一条可度量断言（§九）。

## 一、设计目标与版面总览

| 目标 | 约束来源 | 落地判据 |
| --- | --- | --- |
| 单列、单一滚动 | 375px 放不下双 324px 侧栏 | 侧栏不渲染；唯一滚动容器是消息区 / 视图自身 |
| 拇指可达 | 单手竖握，可达区在屏幕下 1/3 | 高频操作（输入、卡片、视图切换）在下部或顶栏两侧 |
| 触控目标 ≥ 44px | iOS HIG / Material 一致口径 | 顶栏按钮 44×44；Sheet 项 52px；视图内按钮 44px |
| 不横向溢出 | 320px 是实测下限 | `document.scrollWidth === innerWidth`（§九 断言） |
| 安全区 | 刘海 / Home 指示条 / 圆角 | 所有贴边元素带 `env(safe-area-inset-*)` |
| 桌面零回归 | 同一套组件要服务两种视口 | 移动规则全部收在 `@media (max-width: 767.98px)` 内 |

**断点**：`(max-width: 767.98px)` 与 `useTouchDevice().isMobile`（`innerWidth < 768`）严格同源——前者给 CSS，后者给模板 `v-if`，两者判定永不打架。

### 版面总览

```prodoc-flow
graph TB
  H["顶栏 56px：☰ · 标题 · 卡片 · 主题"] --> C[内容区：唯一滚动]
  C --> B["底部：入口行 + 输入区（chat）"]
  H -.->|☰| VS[视图 Sheet：聊天/设置/Shell]
  H -.->|卡片| CS[卡片 Sheet：信息卡片]
```

三段结构，与桌面一致（顶栏 / 主区），但每段的**内容不同**：

| 区段 | 桌面 | 移动端 v2 |
| --- | --- | --- |
| 顶栏左 | 品牌 `EchoAgent Panel` | **`☰`（视图菜单）+ 当前视图标题** |
| 顶栏右 | 聊天 / 设置 / 全屏 / 主题 | 卡片（仅 chat）+ 主题 |
| 主区 | 双列侧栏 + 消息 | 消息（单列） |
| 底部 | 入口行浮层 + 悬浮输入区 | 入口行（横滚）+ 输入区（贴底安全区） |
| 侧栏卡片 | 常驻双列 | 底部 Sheet |
| 视图切换 | 顶栏常驻按钮 | `☰` → 底部 Sheet |

### 顶栏（v2 重设计）

```prodoc-flow
graph LR
  M["☰"] --> T["标题"] --> S["（弹性空）"] --> K["卡片"] --> TH["主题"]
```

- **`☰` 视图菜单**（44×44，`aria-expanded` 反映 Sheet 开合）：打开视图 Sheet；`aria-label="切换视图"`
- **标题**：随视图变化——会话视图显示**当前 Agent 名**（人格即身份），设置显示「设置」，Shell 显示「Shell」。仍保留省略号（多节点聚合时 Agent 名可很长），但不再是「品牌压成省略号」那种无信息量的退化
- **卡片**：仅会话视图渲染；打开卡片 Sheet
- **主题**：ui-frame 三态开关（浅 / 自动 / 深），90px

**为什么不像桌面那样直接摆视图按钮**：桌面是「品牌 + 聊天 + 设置 + 全屏 + 主题」，375px 上必然溢出——v1 的解法是压品牌字号 + 加省略号（实测 320px 时品牌宽被压到 0）。v2 把视图切换移进 Sheet 后，顶栏只剩「菜单 + 标题 + 卡片 + 主题」，320px 仍有 18px 余量，且 **Shell 视图终于有了常驻入口**（v1 只能走「卡片 Sheet → Shell 卡 → 详情」三级路径）。

### 视图切换 Sheet

底部 Sheet（`NeumorphismDrawer`，`position="bottom"`），三项固定顺序 `聊天 → 设置 → Shell`（与桌面顶栏一致，避免肌肉记忆分裂）：

| 项 | 图标 | 副标题 |
| --- | --- | --- |
| 聊天 | `message` | 消息、输入与入口行 |
| 设置 | `gear` | API、技能、插件与 Core 连接 |
| Shell | `clipboard` | Shell 会话列表与详情 |

- 当前项高亮 + `check` 图标 + `aria-current="page"`
- 选中即切换并关闭；点遮罩 / 关闭按钮只关闭、不切换
- 项高 **52px**（> 44px 触控下限），图标 + 双行文字（标题 15px / 副标题 12px 省略号）

## 二、内容区与入口行

**内容区**是唯一滚动容器（`overflow-y: auto`），滚动交给视图自身——库在 <768px 会给布局挂 `nm-layout--mobile` 切成「整页滚动」模式，而本应用是固定视口 SPA（`html/body/#app` 均 `overflow: hidden`），必须把布局拉回定高并恢复收缩能力，否则小屏上输入区会被裁出视口且无任何可滚动容器可达（§七 详述）。

### 入口行：横滚 + 「还有更多」提示

入口行最多可挂 8 个控件（Agent 切换器 + 配置 / 上下文 / 清单 / 会话 / 工作区 / 任务 / 实例，后四个按能力 `v-if`）。375px 上实测总宽 **608px** vs 可用 **359px**——必然横向滚动。

| 做法 | 说明 |
| --- | --- |
| 容器 | `left/right: 8px`（右界保证可滑到底，此前无右界时右侧按钮滑不到）、`overflow-x: auto`、`flex-wrap: nowrap`、隐藏滚动条 |
| 子项 | `flex: 0 0 auto`（不压缩，保持各自完整宽度） |
| 纵向手势 | `touch-action: pan-x`——纵向滚动完全交给消息区，避免斜滑时两个容器抢手势 |
| **滚动提示** | 可滚且未滚到底时，右端叠一个 `›`（36px，`position: sticky` 贴右缘、`pointer-events: none`、左缘 mask 渐隐）；滚到底立即消失 |

提示的实现要点（`ChatView.vue`）：`entryRowRef` + `syncEntryScroll()` 计算 `entryScrollable / entryAtEnd`；在入口行 `scroll` 事件、窗口 `resize`、以及四个能力开关（`sessionSwitcherAvailable` / `workspaceAvailable` / `tasksAvailable` / `instanceAvailable`）变化时重算。**桌面**：入口行不溢出 → `entryScrollable` 恒 false → 提示不渲染（零回归）。

> 为什么不改成「常驻 2 项 + 更多 Sheet」：这 8 个入口在聊天时都有即时用途（清单、工作区、会话切换尤甚），收进二级菜单会让高频动作多一次点击。横滚的学习成本低，缺的只是「右边还有」这一条视觉信号——补信号比改造交互结构更划算。

### 输入区

- `left/right: 8px`，`bottom: calc(8px + env(safe-area-inset-bottom) + 键盘偏移)`
- 圆角 20px、内边距 10px（与桌面同族，只收窄）
- **输入字号 16px**：iOS Safari 在 <16px 的输入框聚焦时会自动放大整页，这是硬约束，不是审美选择
- 键盘弹出时随 `visualViewport` 抬升（§七）

### 消息区

- 气泡 `max-width: 88%`（桌面 100%，窄屏减少两侧浪费）
- 底部 `padding-bottom` 预留浮层高度（入口行 + 输入区 + 安全区），保证最后一条消息可滚到浮层之上
- 会话区覆盖层（上下文 / Agent 配置）收为 `inset: 6px` 近全屏，最大化可读宽度

## 三、卡片 Sheet（双侧栏的窄屏替代）

桌面双列侧栏在 375px 上会压死正文（两列各 324px），因此**不渲染侧栏**，卡片改由顶部「卡片」按钮打开的底部 Sheet 承载：

- 容器：`NeumorphismDrawer position="bottom"`，标题「信息卡片」，高度 78%
- 内容：复用 `PanelSidebar`（左列 + 右列），门控 / 排序 / 工作区绑定全部继承——**不是另写一套卡片列表**
- 排列：`RailStack` 在 Sheet 内改为纵向 `block`（桌面「常驻列」语义中的 `height:100%` + 槽位 `flex:1` 会让每列各占满一屏、要滚两次才看全）；滚动交给 Sheet 本身
- 拖动：`drag-enabled=false`（无跨列拖动，窄屏没有「另一列」）
- 空态：「（当前无可见卡片）」
- **与视图 Sheet 互斥**：两者共用一个 `mobileSheet` 单值（`mobile-sheets.ts`），不会出现「打开卡片时视图菜单还开着」的叠层事故

## 四、软键盘、触控与安全区

这是移动浏览器最脏的一块，三层兜底（缺一层就会出现「输入框被工具条遮住」）：

1. `height: 100%`（最上方全局段，兜底中的兜底）
2. `height: 100dvh`（iOS 15.4+ / Chrome 108+ 地址栏收展动态跟随）
3. `height: var(--app-h, 100dvh)`——`index.html` 内联脚本实测 `window.innerHeight` 写入的 px 值（个别浏览器 dvh 不可靠：iOS 非滚动页不动态更新、老 WebView 不支持）

另外必须压掉库给的 `min-height: 100vh`：移动浏览器里 `vh` 是「大视口」（按工具条隐藏时算），比可见区高约一个工具条高度，会顶穿 `height` 覆盖、把底部输入区推到工具条之下。这是「输入框被浏览器底部遮住」的直接元凶。

**键盘偏移**：`visualViewport` 的 `resize` / `scroll` 事件算出键盘高度，写进 `keyboardInsetPx`，输入区与入口行同步 `bottom: calc(... + v-bind(keyboardInsetPx))`。

### 触控目标与安全区

| 元素 | 尺寸 | 依据 |
| --- | --- | --- |
| 顶栏按钮（`☰` / 卡片 / 主题） | ≥ 44×44 | iOS HIG |
| Sheet 项 | 52px 高 | 同上 + 双行文字 |
| 视图内主按钮（刷新 / 查询 / 编辑 / 删除） | 44px 高 | `@media (pointer: coarse)` 统一抬升 |
| 入口行按钮 | 原有 pill 尺寸（≥32px）+ 横滚容器额外 2px 下内边距 | 横滚时手指不易误触 |

安全区：顶栏 `padding-top: env(safe-area-inset-top)`、左右 `max(12px, env(safe-area-inset-*))`；Sheet 与输入区底部 `env(safe-area-inset-bottom)`。

## 五、验证口径（可度量断言）

| 断言 | 判据 |
| --- | --- |
| 无横向溢出 | `document.documentElement.scrollWidth === innerWidth`（320 / 375 / 390 三档） |
| 顶栏全部可达 | 顶栏内所有 `display !== none` 的按钮，最右一个 `getBoundingClientRect().right < innerWidth` |
| 视图切换可达 | `☰` 可见（44×44），Sheet 三项可点，切换后标题随之变化 |
| Shell 可达 | 视图 Sheet 第三项进入后主区非空（有会话 → 详情；无会话 → 空态） |
| 入口行提示正确 | 未滚到底 → `.entry-row__more` 存在；滚到底 → 不存在；桌面 → 始终不存在 |
| 固定视口 | `documentElement.scrollHeight === innerHeight`（不允许整页滚动） |
| 单一滚动容器 | 页面内 `overflow-y: auto/scroll` 且内容溢出的元素只有消息区 / 视图自身 |
| 桌面零回归 | ≥768px：侧栏渲染、顶栏两视图按钮可见、无 `.mobile-view-btn`、无卡片按钮 |

视口档位（实测覆盖）：**320×568**（最小）、**360×640**、**375×667**（主参考）、**390×844**（现代 iPhone）。

## 六、v1 → v2 变更摘要

| 项 | v1（2026-10 初） | v2（本次重设计） |
| --- | --- | --- |
| 视图切换 | 顶栏「聊天 / 设置」两个按钮 | `☰` → 底部 Sheet（含 **Shell**，v1 无入口） |
| 顶栏标题 | 品牌，被挤成省略号 | 当前视图标题（聊天 = Agent 名） |
| 顶栏宽度 | 375px 时品牌被压到几近不可见 | 320px 仍余 18px |
| 入口行 | 横滚，无任何提示 | 横滚 + `›` 提示（滚到底消失） |
| 卡片抽屉 | 独立布尔，可与视图菜单叠加 | 与视图 Sheet 共用单值，互斥 |
| Shell 视图 | 进入无目标会话时**空白** | 自动落到第一个会话；无会话时显式空态 |
| 断点口径 | CSS 与 JS 各自硬编码 | 统一 `767.98px` / `innerHeight < 768`，并写入测试 |
