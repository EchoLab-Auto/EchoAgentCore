---
id: mobile-layout
title: "移动端布局"
group: 前端
x: 2240
y: 500
link: ["mobile-overlays | 模态与覆盖层"]
---

# 移动端布局

> **定位**：Panel 在**移动端**（视口 < 768px，竖屏为主）的完整空间设计与交互重排。这不是「把桌面样式收窄」，而是**按拇指可达性重新排布信息架构**：视图切换进底部 Sheet、卡片进抽屉、入口行只保留一条可横滚的带。设备无关的功能契约见[交互定义](./interaction.md)，宽屏形态见[桌面端布局](./desktop-layout.md)。
>
> **读者**：要改窄屏形态的开发者。每个结论都对应 `web/src` 里的具体规则与一条**可度量断言**（§六），关键数值来自 headless Chrome + CDP 实测（320×640 / 375×812，安全区 47/34）。浮层（入口行覆盖面板 / 模态 / Sheet）在窄屏的形态见[移动端模态与覆盖层](./mobile-overlays.md)。

## 一、版面总览与三条口径

| 目标 | 约束来源 | 落地判据 |
| --- | --- | --- |
| 单列、单一滚动 | 375px 放不下双 324px 侧栏 | 侧栏不渲染；唯一滚动容器是消息区 / 视图自身 |
| 拇指可达 | 单手竖握，可达区在屏幕下 1/3 | 高频操作（输入、卡片、视图切换）在下部或顶栏两侧 |
| 触控目标 ≥ 44px | iOS HIG / Material 一致口径 | 顶栏按钮 44×44；Sheet 项 52px；视图内按钮 44px |
| 不横向溢出 | 320px 是实测下限 | `document.scrollWidth === innerWidth`（§六 断言） |
| 安全区 | 刘海 / Home 指示条 / 圆角 | 贴边元素带 `env(safe-area-inset-*)`，且**高度与内边距一起加** |
| 桌面零回归 | 同一套组件服务两种视口 | 窄屏规则全部收在 `@media (max-width: 767.98px)` 内 |

**口径 1 · 断点同源**：`(max-width: 767.98px)` 与 `useTouchDevice().isMobile`（`innerWidth < 768`）严格互补——前者给 CSS，后者给模板 `v-if`，两者判定永不打架。

**口径 2 · 三把环境量**：窄屏的一切「为什么会错位」都出在这三个量没被算进去（桌面它们恒为 0，所以桌面看不出来）：

| 量 | 取值 | 消费方 |
| --- | --- | --- |
| `env(safe-area-inset-top/bottom)` | iPhone 实测 47 / 34 | 顶栏**高度与**内边距；输入区、入口行、覆盖层底部；消息区底部留白 |
| `keyboardInset`（`visualViewport` 算得，<80px 视为抖动归零） | 键盘高度 ≈ 300 | 输入区 / 入口行的 `bottom`；消息区 `padding-bottom` |
| `isMobile` | `true` | 侧栏是否渲染、模板分支、默认占位文案 |

**口径 3 · 底部是一条链**：`entryBottom = 输入区高 + 24`（`ResizeObserver` 实测写入），入口行、侧栏下界、覆盖层底边、「回到底部」按钮**全部以它表达**。窄屏改任何底部元素，先看它挂没挂这条链、有没有补安全区与键盘位移。

### 版面总览

```prodoc-flow
graph TB
  H["顶栏 56px + 安全区：☰ · 标题 · 卡片 · 主题"] --> C[内容区：唯一滚动]
  C --> B["底部：入口行 + 输入区（chat）"]
  H -.->|☰| VS[视图 Sheet：聊天/设置/Shell]
  H -.->|卡片| CS[卡片 Sheet：信息卡片]
```

| 区段 | 桌面 | 移动端 v2 |
| --- | --- | --- |
| 顶栏左 | 品牌 `EchoAgent Panel` | **`☰`（视图菜单）+ 当前视图标题** |
| 顶栏右 | 聊天 / 设置 / 全屏 / 主题 | 全屏（图标）+ 卡片（仅 chat）+ 主题——全屏在卡片**左侧**，2026-10 起不再隐藏 |
| 主区 | 双列侧栏 + 消息 | 消息（单列，无侧栏留白） |
| 底部 | 入口行 + 悬浮输入区 | 入口行（横滚）+ 输入区（贴底、随键盘抬升） |
| 侧栏卡片 | 常驻双列 | 底部 Sheet（卡片抽屉） |
| 视图切换 | 顶栏常驻按钮 | `☰` → 底部 Sheet |

### 顶栏全屏按钮（2026-10 改为移动端可用）

此前 `.topbar-actions .fullscreen-toggle { display: none }` 把移动端整段隐藏，理由写的是「iOS Safari 不支持元素全屏」。**这是把一家限制当成所有移动端的限制**：Android Chrome、微信内置浏览器、各家 WebView 都支持 `requestFullscreen`，手机上隐藏地址栏能给会话区多出 ~50px 真实可见高度。

| 决策 | 做法 |
| --- | --- |
| 显示 | 移动端照常显示，位置在「卡片」**左侧**（`App.vue` 里 DOM 顺序即视觉顺序；桌面端卡片按钮 `v-if` 为假，故桌面仍是「聊天/设置 → 全屏 → 主题」） |
| 收窄 | 窄屏走 `compact` 图标态：16px 四角图标 + 左右 8px 内边距 ≈ **34px**（原来文本按钮 60px），375/320 顶栏仍放得下三个按钮；高度由 `pointer: coarse` 段的 `min-height: 44px` 兜住 |
| 能力检测 | `typeof document.documentElement.requestFullscreen === 'function'` 为假（iOS Safari）时：按钮仍显示，`title` 与点按 toast 都说明「可『添加到主屏幕』后全屏运行」——不假装能全屏，也不静默失败 |
| 图标 | 进入=四角向外、退出=四角向内；同时 `aria-pressed` 反映状态，`aria-label` 在图标态承担文案（`全屏` / `退出全屏` / `全屏（当前浏览器不支持）`） |

实测（375×812，CDP 真实鼠标手势）：点按 → `document.fullscreenElement = HTML`、图标与 `aria-label` 同步切到「退出全屏」；再点 → 退出、还原。`element.click()` 之类的合成事件会被浏览器以 NotAllowedError 拒绝（非用户手势）——这是 API 的正确行为，不是 bug。

### 视图切换 Sheet

底部 Sheet（`NeumorphismDrawer`，`position="bottom"`），三项固定顺序 `聊天 → 设置 → Shell`（与桌面顶栏一致，避免肌肉记忆分裂）：

| 项 | 图标 | 副标题 |
| --- | --- | --- |
| 聊天 | `message` | 消息、输入与入口行 |
| 设置 | `gear` | API、技能、插件与 Core 连接 |
| Shell | `clipboard` | Shell 会话列表与详情 |

- 当前项高亮 + `check` 图标 + `aria-current="page"`；选中即切换并关闭，点遮罩 / 关闭按钮只关闭、不切换
- 项高 **52px**（> 44px 触控下限），图标 + 双行文字（标题 15px / 副标题 12px 省略号）
- **为什么不像桌面那样直接摆视图按钮**：桌面是「品牌 + 聊天 + 设置 + 全屏 + 主题」，375px 上必然溢出——v1 压品牌字号 + 省略号（320px 时品牌宽被压到 0）。v2 移进 Sheet 后顶栏只剩四项，320px 仍有余量，且 **Shell 有了常驻入口**（v1 只能走「卡片 Sheet → Shell 卡 → 详情」三级路径）

## 二、底部两带：入口行、输入区与消息留白

**内容区**是唯一滚动容器（`overflow-y: auto`），滚动交给视图自身——库在 <768px 给布局挂 `nm-layout--mobile` 切成「整页滚动」模式，而本应用是固定视口 SPA（`html/body/#app` 均 `overflow: hidden`），必须把布局拉回定高并恢复收缩能力，否则小屏上输入区会被裁出视口且无任何可滚动容器可达（§五 详述）。

三条底部公式（**都随键盘抬升；左右两侧与刘海做 `max()` 让位**）：

| 元素 | 公式 |
| --- | --- |
| 输入区 | `left/right: max(--composer-inset, 安全区)`；`bottom: --composer-inset + keyboardInset`；`--composer-inset = 3vw`（**左右底三者相等**）；轮廓 = `clip-path` 超椭圆（见下节）；输入字号 **16px**（iOS 焦点防缩放，硬约束） |
| 入口行 | `bottom: entryBottom + keyboardInset`（`entryBottom` = 输入区实测高度 + 24，由 `ResizeObserver` 写回）；横滚 |
| 消息区留白 | `padding-bottom: 150px + keyboardInset` |

> **输入区不再单列 `safe-bottom`**：底距就是「屏宽的 3%」（iPhone 上 11.3~13.2px，见
> 下节）。底部留白必须等于左右留白，否则任何轮廓都无法让角部间隙恒定 —— 这是 2026-09
> 实测出来的（当时底部垫 34px 安全区，间隙只能压到 9.9px）。Home 指示条的可视范围距底
> 8~13px，与小屏段的指示条上缘基本齐平（指示条由系统绘制在内容之上）。
>
> **入口行的 `entryBottom` 不含安全区**：它读的是输入区实测 `rect`，安全区已经体现在
> 输入区的 `bottom` 里，再叠一次会把入口行顶到输入区脸上。

### 输入区圆角：3% 间距 + 四角等大 + 下角贴屏幕角（2026-10）

三条需求按优先级排：**① 间距 = 屏宽的 3%**（用户定）；② 四个角必须等大；③ 在
前两条前提下，下角与屏幕圆角**等间隙平行**。

浏览器读不到屏幕圆角（CSS Round Display 的 `device-radius` 没实现、也没有 `env()`），
所以值来自近似 token `--panel-display-radius`（`styles.css`；窄屏按逻辑视口宽度分档：
390 → 57、393 → 65.5、402 → 74.5、420 → 74.3、430 → 66.5、440 → 74.5，兜底 55 / 70；
桌面 0）。

**轮廓不是 `border-radius`，是 `clip-path: path()`**：`border-radius` 只能画圆弧，贴
iPhone 的超椭圆屏角时无论取什么半径，45° 附近都会错位（最优仍有 2.3px 波动）。轮廓由
`src/utils/squircle.ts` 的 `composerOutline` 算出折线（24 段/角）：下角 = 屏幕角超椭圆
（n=2.5）沿法线内缩留白 `d` 的**偏移曲线**，上角 = 下角关于水平中轴的**镜像**。

#### 四个量互相约束，公式必须闭环

| 量 | 取值 | 说明 |
| --- | --- | --- |
| 留白 `d` | `0.03 × 100vw` | 左右底三者相等（等间隙的几何前提） |
| 跨度（每角纵向尺寸） | `R − d`，上下两角共用 | 留白变小 → 跨度变大 |
| 卡片静止高 `H` | `3P + 输入行 + 状态行` | 必须 `≥ 2×跨度`，否则上下角放不下 |
| 外壳内边距 `P` | `max(10px, (2×跨度 − 输入行 − 状态行) / 3)` | **多出来的高度给 P，不给输入行** |

**为什么高度落在 padding 上**：留白从 22px 降到 12px 后，跨度从 52.5 涨到 62.4、卡片
需要 125px。若按「输入行吃掉差额」的算法，输入条会变成 **78px 高的单行框 + 78px 的圆形
按钮** —— 远超触控基准（iOS 44 / 我们的 45.6）。所以把差额三等分进 `P`：输入行与按钮
保持 45.6，卡片靠内边距长到 125。三处间距仍恒等于 `P`（输入框上 = 输入框下→徽标 = 徽标→
外壳下），所以视觉上是「一张更厚的胶囊」，不是「空出来的白」。

**留白不能依赖输入行高度**：一旦 `--composer-inset` 去读 `--chat-control-h`，就与上面
第三条形成 CSS 变量循环，浏览器会把两边都判为无效（`--chat-control-h` 整条失效、输入行
高度归零）。所以链条只能是 `视口宽 → d → 跨度 → P`，单向。

#### 结果（生产构建实测）

| 视口 | 屏角 R | 留白 d | 跨度 | 卡片静止高 | 输入行 | 内边距 P | 下角间隙 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 320×640 | 55 | 9.59 | 45.4 | 93 | 45.6 | 10 | 9.6 ±0.04px |
| 390×844 | 57 | 11.69 | 45.3 | 93 | 45.6 | 10 | 11.7 ±0.05px |
| 393×852（15/16） | 65.5 | 11.78 | 53.7 | 107.8 | 45.6 | 14.9 | 11.8 ±0.06px |
| 375×812 @3.2（Zoom） | 70 | 11.25 | 58.7 | 117.9 | 45.6 | 18.3 | 11.3 ±0.04px |
| 402×874（16 Pro/17） | 74.5 | 12.05 | 62.4 | 125.3 | 45.6 | 20.8 | 12.1 ±0.06px |
| 440×956（Pro Max） | 74.5 | 13.19 | 61.3 | 123 | 45.6 | 20 | 13.2 ±0.05px |
| 桌面 1280 | 0 | 12 | — | 95.4 | 42.4 | 12 | 无 clip，`border-radius: 24px` |

四角镜像偏差 **0.00~0.01px**（严格同形同大）；下角间隙波动 ≤ 0.06px（= 离散化误差，
等于「完全贴合」）；多行输入把卡片撑高时两角尺寸不变，多出来的成为竖直直边。

> **Home 指示条**：可视范围距底 8~13px。3% 间距在 iPhone 上是 11.3~13.2px，与小屏段的
> 指示条上缘基本齐平；指示条由系统绘制在内容之上，不会被卡片遮住。

#### 哪部分是「静态编码」，哪部分是动态

| 层 | 内容 | 何时变 |
| --- | --- | --- |
| 静态 token | `--panel-display-radius` 屏角表（上面那串分档） | 换档位（视口宽度 / DPR） |
| CSS 计算链 | `3vw` → 留白 → 跨度 → `P` → 内层圆角，全是 `calc()` | 视口宽或字号变 |
| 运行时 JS | `updateComposerClip()`：实测 `rect.left`（留白，含安全区）、实测卡片高反推静止半高、按 n=2.5 采样 24 段算折线 → 写 `--composer-clip` 与 `--composer-corner-r`（**实际跨度**） | 挂载 + 每次 `ResizeObserver` |

所以移动端的圆角没有硬编码像素值：**是每次测量后现算的曲线**。唯一的静态假设是「这台
设备的屏角跨度是表里那一档」，且取「宁松勿紧」（误差方向安全）。

> 跨档补充：`P` 现在吃屏角 token，于是跨档（402 → 393）时 `P` 会从 20.8 变回 14.9，
> 卡片 125 → 108。ui-frame 的 auto-resize 把文本域高度写成**内联 px**、只在收到
> `input` 时重算 —— 变高没事，变矮会卡住。补丁：`updateComposerClip()` 记住上次的
> 输入行高，变化超过 0.5px 就补发一次 `input`。

断言（`npm run layout:mobile`，160 项）：四角等大（镜像偏差 < 0.3px，多行后仍成立）、
下角间隙恒正且波动 ≤ 4px、左右两侧分别成立、多行不改角的尺寸、三处间距都等于 `P`、
发送/附件与输入框同高、圆角真的切出来了（角尖点击穿透 + 底边命中）。

### 入口行：横滚 + 「还有更多」提示

入口行最多可挂 8 个控件（Agent 切换器 + 配置 / 上下文 / 清单 / 会话 / 工作区 / 任务 / 实例 + 模型，后几个按能力 `v-if`）。375px 上实测总宽 **608px** vs 可用 **359px**——必然横向滚动。

| 做法 | 说明 |
| --- | --- |
| 容器 | `left/right: 8px`（右界保证可滑到底，此前无右界时右侧按钮滑不到）、`overflow-x: auto`、`flex-wrap: nowrap`、隐藏滚动条 |
| 子项 | `flex: 0 0 auto`（不压缩，保持各自完整宽度） |
| 纵向手势 | `touch-action: pan-x`——纵向滚动交给消息区，避免斜滑时两个容器抢手势 |
| **滚动提示** | 可滚且未滚到底时，右端叠一个 `›`（36px，`position: sticky` 贴右缘、`pointer-events: none`、左缘 mask 渐隐）；滚到底立即消失 |

提示的实现要点（`ChatView.vue`）：`entryRowRef` + `syncEntryScroll()` 算 `entryScrollable / entryAtEnd`；在入口行 `scroll`、窗口 `resize`、以及四个能力开关变化时重算。**桌面**：入口行不溢出 → `entryScrollable` 恒 false → 提示不渲染（零回归）。

### Agent 卡片：窄屏必须收窄（2026-10 修复）

Agent 切换器是入口行第一项，卡片里塞了**头像 + 名字 + 区域徽标 + 模型名 + 箭头**五段信息。375px 上实测宽 **213.9px**，占掉可用宽（359px）的 **60%**——后面「配置 / 上下文 / 清单 / 会话 / 任务 / 模型」全被推到横滚里，一屏只露出 1.4 个按钮。

| 收敛手段 | 效果 |
| --- | --- |
| 隐藏 `.agent-card__region`（区域徽标） | 信息冗余：菜单里每条 Agent 仍带 core 徽标；单接入点场景本来就没有这条 |
| 名字 `max-width: 84px`、模型 `max-width: 64px` | 超长名与模型名省略号截断，卡片宽度确定 |
| 卡片 `gap: 6px`、`padding: 0 8px 0 3px` | 与入口行其它 pill 的视觉密度对齐 |

收窄后卡片 **108.6px**，320px 屏上一屏可见「Agent + 配置 + 上下文」三个按钮。桌面（≥768px）不受影响，仍显示区域徽标（213.9px，入口行不溢出）。

### Agent 菜单：被入口行裁掉的坑（2026-10 修复）

入口行在窄屏是 `overflow-x: auto` 的横滚容器。CSS 规定：**当一个方向是 `auto`/`scroll`、另一个方向是 `visible` 时，`visible` 会被计算成 `auto`**——于是入口行成了双向裁剪容器。原来的 Agent 菜单是入口行内的 `position: absolute; bottom: calc(100% + 8px)` 上弹层，**整个落在裁剪区外**：

| 现象 | 实测（375×812） |
| --- | --- |
| 菜单 DOM 存在、`getBoundingClientRect()` 正常 | 菜单 y = 466..632，宽 321 |
| 但屏幕上什么都看不到 | 入口行 top = 640，裁剪区从 640 才开始 |
| 命中测试揭示真相 | `elementFromPoint(菜单中心)` → `.chat-view__item--msg`（消息区），不是菜单 |

修复：菜单改由 `PanelPopover` 承载（`teleport` 到 body + `position: fixed`），与入口行其它六项弹层同一套容器与关闭语义；窄屏自动变成左右各 8px、贴底上弹（实测 375 上 x=8、宽 359，320 上宽 304），桌面为 360px 锚定卡片居中。`AgentSwitcher` 自挂的 `document` 点击监听随之删除——两套外部点击监听会互相抢先。

> **同类风险**：任何在 `.entry-row`（或其它横滚容器）内部就地 `absolute` 定位的浮层，纵向都会被裁。做入口行新控件时，内容面板一律用 `SessionOverlay`（渲染在会话视图内、边界取消息区矩形，天然不受入口行的裁剪与横滚影响）。

> 为什么不改成「常驻 2 项 + 更多 Sheet」：这 8 个入口在聊天时都有即时用途（清单、工作区、会话切换尤甚），收进二级菜单会让高频动作多一次点击。横滚的学习成本低，缺的只是「右边还有」这条视觉信号——补信号比改造交互结构更划算。

## 三、视图内的窄屏重排（设置 / Shell）

顶栏与底部两带之外，各视图有自己的窄屏重排。设置视图是最重的一处（`SettingsView.vue` 的 `@media`）：

| 桌面 | 移动端 | 实现 |
| --- | --- | --- |
| 一级菜单 168px 竖排 | 顶部**横滑 tab 条**（`scrollbar: none`、`flex: 0 0 auto`） | `.caps-nav` 改 `flex-direction: row` |
| 列表 + 详情并排（可拖分隔条） | **Master-Detail 单栏**：列表态隐藏详情、详情态隐藏列表，详情头部出「返回」 | `.caps-workspace--mobile-list/detail` + `mobileWorkspaceClass` |
| 筛选条：搜索框 + chips + 分组段选，`flex-wrap` | **两行固定形态**：搜索独占一行、chips 单行横滚、段选靠右 | `.caps-filters` / `.caps-filters__chips` |
| 工具条 `flex-wrap` | 允许换行（按钮 44px 高） | `.caps-toolbar` |

> **筛选条为什么要收成两行**（2026-10 实测修复）：桌面三者各自换行，窄屏上是**三段各自成行**——技能分类实测吃掉 **359px**（可视区 686px 的 **52%**），列表被压到 230px（320×640 时只剩 58px，等于看不见）。收成两行后筛选条 111px，列表升到 431px（375）/ 259px（320）。

Shell 视图在两个视口共用同一份结构，只有两处窄屏差异：命令输入行补底部安全区、输入字号 16px；空态（无 Shell 会话）是**显式契约**——给出说明与「回到聊天」出口，不留白（移动端可从视图 Sheet 直接进入 Shell，首次进入必然无目标会话）。

## 四、卡片 Sheet（双侧栏的窄屏替代）

桌面双列侧栏在 375px 上会压死正文（两列各 324px），因此**不渲染侧栏**，卡片改由顶部「卡片」按钮打开的底部 Sheet 承载：

- 容器：`NeumorphismDrawer position="bottom"`，标题「信息卡片」，高度 78%
- 内容：复用 `PanelSidebar`（左列 + 右列），门控 / 排序 / 工作区绑定全部继承——**不是另写一套卡片列表**
- 排列：`RailStack` 在 Sheet 内改为纵向 `block`（桌面「常驻列」语义里的 `height:100%` + 槽位 `flex:1` 会让每列各占满一屏、要滚两次才看全）；滚动交给 Sheet 本身
- 拖动：`drag-enabled=false`（窄屏没有「另一列」）
- 底部：卡片栈容器补 `12px + env(safe-area-inset-bottom)`，末卡不被 Home 指示条压住
- 空态：「（当前无可见卡片）」
- **与视图 Sheet 互斥**：两者共用一个 `mobileSheet` 单值（`mobile-sheets.ts`），不会出现「打开卡片时视图菜单还开着」的叠层事故

## 五、软键盘、安全区与库样式覆盖

### 视口高度三层兜底

移动浏览器最脏的一块，缺一层就会出现「输入框被工具条遮住」：

1. `height: 100%`（全局兜底）
2. `height: 100dvh`（iOS 15.4+ / Chrome 108+ 地址栏收展动态跟随）
3. `height: var(--app-h, 100dvh)`——`index.html` 内联脚本实测 `window.innerHeight` 写入的 px 值（个别浏览器 dvh 不可靠：iOS 非滚动页不动态更新、老 WebView 不支持）

另外必须压掉库给的 `min-height: 100vh`：移动浏览器的 `vh` 是「大视口」（按工具条隐藏时算），比可见区高约一个工具条，会顶穿 `height` 覆盖、把输入区推到工具条之下——这是「输入框被浏览器底部遮住」的直接元凶。这条覆盖同样要 `#app` 前缀（见下）。

### 安全区：高度与内边距必须一起加

```css
#app .nm-layout__header {
  height: calc(56px + env(safe-area-inset-top, 0px));
  padding-top: env(safe-area-inset-top);
  padding-left/right: max(12px, env(safe-area-inset-left/right, 0px));
}
```

只加 `padding-top` 不够：库把顶栏定高 `56px`，内边距吃掉 47px 后内容盒只剩 9px，44px 的 `☰` 反而**溢出到会话区之上**（实测按钮 29.5..73.5 vs 顶栏 0..56）。高度与内边距同步加，顶栏整体下移。

### 库样式覆盖：必须赢特异性

库规则带 scoped 属性选择器，比裸类多一级：`.nm-layout__header[data-v-006e9573]` 是 `(0,2,0)`，应用侧的 `.nm-layout__header` 只有 `(0,1,0)` —— **整段静默失效**（不报错，只是没生效）。实测后果：`padding-top` 仍是 `0`、左右仍是库的 `16px`，刘海屏上顶栏按钮有 41px 埋在状态栏里。修法是加 `#app` 前缀提到 `(1,1,0)+`；`styles.css` 里所有覆盖库根节点的窄屏规则都遵循这一条。

### 触控目标

| 元素 | 尺寸 | 依据 |
| --- | --- | --- |
| 顶栏按钮（`☰` / 卡片 / 主题） | ≥ 44×44 | iOS HIG |
| Sheet 项 | 52px 高 | 同上 + 双行文字 |
| 视图内主按钮（刷新 / 查询 / 编辑 / 删除） | 44px 高 | `@media (pointer: coarse)` 统一抬升（与视口无关：平板触摸同样受益） |
| 入口行按钮 | 原有 pill 尺寸（≥32px）+ 横滚容器额外 2px 下内边距 | 横滚时手指不易误触 |

## 六、验证口径与实测记录

### 可度量断言

| 断言 | 判据 |
| --- | --- |
| 无横向溢出 | `document.documentElement.scrollWidth === innerWidth`（320 / 375 / 390 三档） |
| 顶栏让开刘海 | 顶栏高 = `56 + safe-top`；顶栏内每个按钮的 `top ≥ safe-top` 且 `bottom ≤ 顶栏高` |
| 顶栏全部可达 | 顶栏内所有 `display !== none` 的按钮，最右一个 `rect.right < innerWidth` |
| 全屏按钮可用 | 移动端 `.fullscreen-toggle` 的 `display !== none` 且只含 SVG（`compact` 图标态） |
| 全屏在卡片左侧 | `.fullscreen-toggle` 的 `right ≤ .mobile-cards-btn` 的 `left` |
| 全屏触控目标 | 宽 **32..48px**、高 **≥44px**（实测 34×44） |
| 底部不漏 | 入口行 `bottom ≤ 输入区 top − 8`（无重叠，实测间距 16） |
| 输入区圆角同心 | 外框圆角 = `min(max(20, 屏幕圆角−8), 34)`；内框/附件/发送 = 外框 − 10（±0.5） |
| 圆角真切出来 | `elementFromPoint(外接矩形角尖)` 不命中输入区，底边中点命中 |
| 留白跟键盘 | 消息区 `padding-bottom ≥ 输入区占位（bottom + 高）`，键盘弹出后仍成立 |
| 视图切换可达 | `☰` 可见（44×44），Sheet 三项可点，切换后标题随之变化 |
| Shell 可达 | 视图 Sheet 第三项进入后主区非空（有会话 → 详情；无会话 → 空态） |
| 入口行提示正确 | 未滚到底 → `.entry-row__more` 存在；滚到底 → 不存在；桌面 → 始终不存在 |
| Agent 卡片不吃掉入口行 | `.agent-card` 宽 ≤ 140px（实测 108.6，修复前 213.9）；窄屏 `.agent-card__region` 不可见 |
| Agent 菜单点得开 | 点卡片后 `elementFromPoint(菜单中心)` 命中菜单内元素（**被裁时命中消息区**——这条是唯一能抓住该 bug 的判据） |
| Agent 菜单在视口内 | `.panel-popover` 的 `parentElement === body`，且四边不越界（375：x=8、宽 359；320：宽 304） |
| Agent 菜单可关 | Escape 后 `.agent-menu` 消失 |
| 设置可用宽度 | 技能/工具分类下 `.caps-workspace` 高度 ≥ 可视区一半（实测 431 / 709 = 61%） |
| 固定视口 | `documentElement.scrollHeight === innerHeight`（不允许整页滚动） |
| 桌面零回归 | ≥768px：侧栏渲染、两视图按钮可见、无 `.mobile-view-btn`、消息区 `padding-bottom` 恒 190px |

视口档位（实测覆盖）：**320×640**（最小）、**375×812**（主参考）、**390×844**（现代 iPhone）。安全区按 iPhone 口径注入 `top: 47 / bottom: 34`。

### 怎么测（可复现，已入库）

```bash
cd web && npm run layout:mobile        # 375×812 + 320×640，46 条断言
BASE=http://127.0.0.1:8080 npm run layout:mobile   # 打生产构建
```

脚本 `web/scripts/mobile-layout-check.mjs`：无头 Chrome + CDP 直连（**不引 playwright**，Node ≥21 的内置 `WebSocket` 即可），关键三步是

```js
Emulation.setDeviceMetricsOverride({ width, height, deviceScaleFactor: 2, mobile: true })
Emulation.setTouchEmulationEnabled({ enabled: true, maxTouchPoints: 5 })
Emulation.setSafeAreaInsetsOverride({ insets: { top: 47, bottom: 34, left: 0, right: 0 } })
// 再 Runtime.evaluate 取 getBoundingClientRect / getComputedStyle 断言
```

找不到浏览器时**跳过并退出 0**（CI 无浏览器不该红），`--strict` 则视为失败。

> `env(safe-area-inset-*)` 取到非零值取决于两件事：`index.html` 的 `viewport-fit=cover`（已配）与上面的 override——**没有 override 时它恒为 0**。这正是这类 bug 在桌面浏览器和大多数「手机模式」里永远看不见的原因。

### 本轮实测（2026-10 修复前后）

| 项 | 修复前 | 修复后 |
| --- | --- | --- |
| 顶栏 `padding-top`（安全区 47） | `0px`（规则被库压掉） | `47px`，顶栏高 56 → **103** |
| `☰` 按钮纵向位置 | 6..50（41px 埋在状态栏里） | 53..97（完全在顶栏内） |
| 顶栏左右内边距 | 16px（库值） | 12px（+ 安全区 `max()`） |
| 入口行 vs 输入区 | **+18px 重叠** | **−16px 间隙** |
| 消息区底部留白 | 190px（键盘一开就遮 220px） | 224px（=190+34），随键盘继续增长 |
| 设置·技能 筛选条高 | **359px**（可视区 52%） | **111px** |
| 设置·技能 列表高 | 230px（375）/ 58px（320） | **431px / 259px** |
| 桌面（1440×900） | 顶栏 64、入口行间隙 12、留白 190 | 完全一致（未回归） |

### 第二轮（2026-10-20：入口行 Agent 切换）

| 项 | 修复前 | 修复后 |
| --- | --- | --- |
| Agent 卡片宽（375） | **213.9px**（入口行 359px 的 60%） | **108.6px** |
| 入口行一屏可见按钮（320） | 1.4 个 | 3 个（Agent / 配置 / 上下文） |
| 点 Agent 卡片 | 菜单**不可见**（DOM 在、被入口行裁掉，命中消息区） | 可见可点，`elementFromPoint` 落在菜单内 |
| 菜单容器 | 入口行内 `absolute`（宽 321、y 466..632，整块被裁） | `PanelPopover` → body，375 上 x=8/宽 359、320 上宽 304 |
| 关闭方式 | 仅"再点一次卡片"（另挂 document 监听） | `PanelPopover`：外部点击 + Escape（并回焦卡片） |
| 桌面（1280×800） | 卡片 213.9、菜单可见 | 卡片 213.9、菜单改由 PanelPopover 承载（360px，视觉一致） |

七条修复分别落在：`styles.css` 的顶栏安全区（高度 + 内边距 + `#app` 前缀）、`ChatView.vue` 的入口行 `bottom`、`ChatView.vue` 的消息区 `padding-bottom`（含「回到底部」按钮与上下文档覆盖层的底部同步补安全区）、`SettingsView.vue` 的筛选条两行化、`AgentSwitcher.vue` 的窄屏卡片收敛与菜单容器迁移（`PanelPopover` + 删除自挂 document 监听）、`web/scripts/mobile-layout-check.mjs` 新增 6 条 Agent 断言。

### 第三轮（2026-10-20：顶栏全屏按钮）

| 项 | 修复前 | 修复后 |
| --- | --- | --- |
| 全屏按钮（375） | `display: none`（移动端整段隐藏） | 可见 **34×44** 图标态，x=163..197 |
| 与「卡片」相对位置 | —（不存在） | 全屏 right 197 ≤ 卡片 left 201，**在其左侧** |
| 顶栏右侧组宽 | 162px（卡片 + 主题） | **200px**（全屏 34 + 卡片 60 + 主题 94 + 间距） |
| 品牌可用宽（375） | 153px | **67px 余量**（品牌实测 35.9px，未截断） |
| 品牌可用宽（320） | 99px | 12px 余量（仍不截断） |
| 桌面（1280） | 文本按钮「全屏」60px | 不变（`compact` 仅窄屏传入） |
| 点击行为 | —（无按钮） | CDP 真实手势 → `fullscreenElement=HTML`，图标/aria 双向同步 |

两条修复落在：`App.vue`（全屏按钮移到「卡片」之前 + 传 `:compact="isMobile"`）、`styles.css`（删 `display:none`，改 34px 图标态收窄）、`FullscreenToggleButton.vue`（图标态 + 能力检测 + 明确提示）。断言：`mobile-shell-layout.test.ts` 改为「收窄不隐藏」，`fullscreen-toggle.test.ts` 增 2 条（图标态 / 无 API），`layout:mobile` 增 3 条。

### 第四轮（2026-10-20：输入区圆角对齐屏幕圆角）

| 项 | 修复前 | 修复后 |
| --- | --- | --- |
| 输入区外框圆角 | 20px（与屏幕 55px 弧线交叉、看着"没对齐"） | **34px**（= 半高胶囊，且距屏幕弧线余量 3.5px） |
| 输入框内框 / 附件 / 发送 | 10px | **24px**（外框 − padding，同心） |
| 桌面（1280） | 24 / 12 | 不变（token 桌面为 0） |
| 多行输入（4 行，盒高 144） | 圆角固定 20 | 仍 34（上界封顶，不被半高压缩） |

改动落在：`styles.css`（新增 `--panel-display-radius` 设备圆角 token，桌面 0 / 窄屏 55）、`ChatView.vue`（`--composer-radius` 单一事实来源；桌面 24、窄屏 `clamp(...)`；内框/按钮改为由它派生）。断言：`layout:mobile` 增 4 条，`ChatViewMobile.test.ts` 的同心断言改为校验 `clamp` 公式与派生关系。
