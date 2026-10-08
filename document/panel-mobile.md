---
id: panel-mobile
title: "移动端适配（竖屏）"
group: 前端
x: 3520
y: 780
---

# 移动端适配（竖屏）

> **定位**：本文覆盖 Panel 的竖屏移动端（< 768px）自适应布局：断点口径、全局壳约束（安全区/动态视口）、会话视图与设置视图的窄屏形态（工具条横滚、卡片抽屉、Master-Detail 二级导航）、软键盘适配与桌面零回归的边界纪律。读者：维护 Panel 布局与移动端的开发者；实现位置以 `web/src/` 相对路径标注。相关：[布局与导航](./panel-layout.md)、[会话视图](./panel-chat.md)、[设置视图](./panel-settings.md)。

## 一、断点与纪律

**唯一断点：768px**——与 ui-frame 库内 `useTouchDevice().isMobile`（`window.innerWidth < 768`）**严格同源**：库判「移动」与应用判「移动」永不打架（JS 分支用 `isMobile`，CSS 用 `@media (max-width: 767.98px)`，两者覆盖同一区间）。

**零回归纪律**：桌面（≥768px）样式即"默认值"，全部移动规则限定在媒体查询 / `isMobile` 条件内——本轮改造未修改任何一条桌面规则（`isMobile` 全 false 时 JS 各 computed 与原逻辑逐字等价）。

实现分层：**跨组件的壳层规则**放 `styles.css` 尾段（单一移动段，按 ① 全局壳 ② 触控 ③ 各视图分区）；**视图自有规则**就近写在各组件 `<style scoped>` 内。样式令牌仍受 `npm run lint:styles` 守卫。

## 二、全局壳

| 项 | 桌面 | 移动端 |
|---|---|---|
| 视口高度 | `height: 100%` 链 | 三层兜底：`100dvh` → `--app-h`（内联脚本实测 `innerHeight`）→ `100%`；并压掉库的 `min-height: 100vh`（见下「工具条遮挡修复」） |
| 安全区 | — | `index.html` 的 `viewport-fit=cover` + 顶栏 `padding-top: env(safe-area-inset-top)`、输入区 `env(safe-area-inset-bottom)` |
| 顶栏 | 品牌 + 聊天/设置 + 全屏 + 主题 | 品牌字号收敛且**可收缩为省略号**；**全屏按钮隐藏**（触摸端意义低，类随组件根按钮——见「顶栏可达性修复」）；新增「卡片」入口（仅会话视图，见 §四） |
| 触控目标 | — | `@media (pointer: coarse)`：顶栏与入口行按钮 `min-height: 44px` |
| Toast | `top-right` | `top-center`（窄屏右上角贴近拇指区，易遮挡） |
| 库 Modal | — | mask 补 `padding: 8px`（库 mask 无内边距，375px 屏上贴边） |

**布局契约：固定视口 vs 库的"整页滚动"模式（2026-10 回归修复）**

- **冲突**：ui-frame 在 `<768px` 给布局挂 `nm-layout--mobile` 并把它切成"整页滚动"模式（`.nm-layout--mobile` `height: auto` / `.nm-layout__body` `min-height: auto` / `.nm-layout__content` `overflow-y: visible`）——期望由**document 滚动**消费溢出。但 Panel 是固定视口 SPA（`html, body, #app` 均 `overflow: hidden`，滚动在各视图内部），溢出被直接裁掉：**小屏（375×667 / 360×640，可用高 < 内容固有高 733px）时输入区被裁出视口，且页面上不存在任何可滚动容器**（用户症状：无法下滑、看不到输入框；宽度 ≥768 或高度足够 844 时恰好掩盖）。
- **修复**：`styles.css` 移动段以 `#app` 前缀（特异性 1,1,0 > 库 scoped 的 `[data-v-*]`（0,3,0）——不能只写两段类名，会输给 scoped 属性选择器）把布局拉回定高 + 恢复收缩：`#app .nm-layout--mobile { height: 100dvh; overflow: hidden }`、`…__body { min-height: 0; overflow: hidden }`、`…__content { overflow-y: hidden }`。
- **验证口径**：320×568 / 360×640 / 375×667 / 390×844 输入区均在视口内且消息区可滚动（触摸下滑看历史 / 上滑回底）；设置页 master-detail 的 `caps-detail-pane` 内滚动可达底部；桌面 ≥768px 无 `--mobile` 类、样式零变化。守护测试：`web/src/__tests__/mobile-shell-layout.test.ts`（源码契约，4 断言）。

**工具条遮挡修复（2026-10）**

- **症状**：手机浏览器上输入框被浏览器底部工具条遮住——固定视口 SPA 的底部内容落在可见区之下。
- **两个叠加根因**：① ui-frame 的 `.nm-layout--mobile` 自带 `min-height: 100vh`（库移动模式为「整页滚动」站点设计）——移动浏览器里 `vh` 是「大视口」（按工具条隐藏时算），比可见区高 ~工具条高；`min-height` 会顶穿 Panel 的 `height` 覆盖，把底部输入区推到工具条之下。② 个别浏览器 `dvh` 不可靠（iOS 非滚动页不动态更新 / 老 WebView 不支持）——高度偏大产生同样偏移。
- **修复**：`#app .nm-layout--mobile { min-height: 0 }` 压掉库地板；高度链改为 `var(--app-h, 100dvh)`，`--app-h` 由 `index.html` 内联脚本实测 `window.innerHeight`（px），随 `resize` / `orientationchange` / `pageshow` 更新——`innerHeight` 在主流移动浏览器均为「排除工具条的可见高度」，键盘弹出不改变它（键盘偏移仍由 visualViewport 逻辑处理，互不干扰）。
- **验证口径**：模拟「工具条遮挡 60px」（覆盖 `innerHeight` 报告值）在 320/360/375/390/430 五档下输入框完整可见；正常场景无回归。

**顶栏可达性修复（2026-10）**

- **症状**：375px 屏上右侧「主题切换」被推出屏外、不可达；「全屏」按钮本应按设计隐藏却仍在占位。
- **根因**：① 隐藏规则 `.topbar-actions .fullscreen-toggle` 的类从未落在 `FullscreenToggleButton` 根按钮上（规则空转）；② 库 `header-left` 为 `flex: 0 0 auto` 不可收缩 + 品牌固定宽，右组固定 ~354px 必然溢出。
- **修复**：`FullscreenToggleButton.vue` 根按钮补 `fullscreen-toggle` 类；移动段品牌 `min-width: 0` + 省略号、`#app .nm-layout__header-left { flex: 0 1 auto }`、头部左右内边距 `max(12px, env(safe-area-inset-*))`。效果：320–430px 全尺寸右组按钮全可达，品牌按余宽自适应（超窄屏收为 0）。

## 三、会话视图（ChatView）

| 区域 | 桌面 | 移动端 |
|---|---|---|
| 双侧边栏 chat-rail | 左右各 324px 常驻卡片列 | **不渲染**（两列在 375px 屏上压死正文）；卡片改经顶栏「卡片」抽屉访问（§四） |
| 消息区留白 | 有卡片列时按列宽 324px 留白 | 归零（`trayPadLeft/Right` 含 `!isMobile` 条件） |
| 入口行（7 按钮 + Agent 切换器 + 活动条） | 单行铺开 | **横向滚动工具条**：左右 8px 双边界 + `overflow-x: auto` + 隐藏滚动条一一此前无右界，右侧按钮超出屏外不可达 |
| 输入区 | `left/right/bottom: 12px` | 8px + 底部安全区 + 软键盘偏移（§五）；输入面字号 **16px**（iOS 聚焦防缩放） |
| 弹出层 `.checklist-pop`（会话/工作区/适配器/任务/清单） | 跟随按钮 `left = centerX - 260` | **全宽贴底**：`left/right: 8px`（锚点法在窄屏会把面板推出屏外）——定位由 `popoverStyle()` 统一分发 |
| 会话区覆盖层（上下文/Agent 配置） | 贴会话框 12px | 6px 近全屏 |
| 消息气泡 | `max-width: 78%` | 88% |
| 输入框占位文本 | 库 locale 默认长文案（`输入消息，Enter 发送（Shift+Enter 换行）`，宽框单行完整显示） | **短占位** `输入消息…` / 断连 `未连接到 Core…`——窄框（内容宽 ~166px）放不下 303px 的默认文案，折行会被单行框拦腰截断，且 Enter/Shift+Enter 在触屏键盘上不成立（2026-10） |

## 四、卡片抽屉（移动端双侧栏替代）

```
顶栏「卡片」按钮（仅 isMobile && 会话视图）
   └─ NeumorphismDrawer position="bottom"（标题「信息卡片」，高度 78%）
        └─ PanelSidebar ×2（有可见卡片的列才渲染；沿用门控/排序/工作区绑定）
```

- **跨组件开关**：`mobile-cards.ts` 的模块级 `mobileCardsDrawerOpen`——入口在 App 顶栏、抽屉本体在 ChatView（复用其工作区/Shell/分支处理），比往 App 塞 ChatView 内部状态干净。
- **复用面**：连接状态 / 文件浏览器 / Shell / 临时分支四类卡片及其门控、排序、交互全部继承 `PanelSidebar`；移动端不渲染 RailStack 的拖动场景（列被合并为上下排列）。
- 桌面完全无消费者（按钮与抽屉均 `v-if` 门控），开关值恒 false。

## 五、设置视图（SettingsView）

**一级菜单**（9 分类）：168px 竖排 → **顶部横滑 tab 条**（`overflow-x: auto`、隐藏滚动条、条目 `min-height: 40px`）。

**工作台 Master-Detail 二级导航**（技能/工具/插件/智能体）：

```
列表态（默认）                详情态（选中条目后）
┌──────────────┐            ┌──────────────┐
│ [分类tab条]   │            │ [← 返回] 标题 │
│  条目列表     │  点条目 →   │  详情（全宽）  │
│  （全宽）     │  ← 返回    │              │
└──────────────┘            └──────────────┘
```

- 判据 `mobileDetailOpen`：当前分类已选中条目（或编辑态）为 true——**复用现有 `selected*` 状态，零新增状态机**。
- 类切换：工作台根节点挂 `caps-workspace--mobile-detail` / `--mobile-list`，CSS 隐藏对侧（列表 `inline` 宽度在移动端不绑定、`caps-resizer` 隐藏）。
- **返回按钮**（`.caps-mobile-back`，工具条内、仅详情态渲染）：先走与各选择逻辑同口径的脏确认（技能编辑中经 `confirmDialog`），再清空选中/编辑态。
- 其它适配：工具条/表单动作行允许换行；Git 安装弹层 420px 最小宽重置；详情内表格横向滚动兜底。

## 六、软键盘适配（visualViewport）

iOS/Android 弹键盘不改变 layout viewport，`overflow: hidden` 的 SPA 里输入区会被键盘盖住：

- ChatView 监听 `window.visualViewport` 的 `resize`/`scroll`，`keyboardInset = innerHeight - vv.height - vv.offsetTop`（**> 80px 才计入**，滤地址栏伸缩抖动）；
- 注入输入区与入口行 `bottom`（`v-bind(keyboardInsetPx)`）——键盘弹出时两者同步抬升，入口行不悬空；
- 监听仅移动端挂载、随组件卸载清理；桌面（无 visualViewport 或 isMobile=false）零开销。

## 七、验证口径

- **尺寸**：320×568（SE1）/ 375×667 / 390×844 / 430×932（浏览器 + PWA standalone，含安全区）。
- **回归**：≥768px 桌面关键视图像素不变（改动全在媒体查询内）；桌面无 `nm-layout--mobile` 类。
- **单测**：
  - `ChatViewMobile.test.ts`（4）：rail 移动不渲染/桌面渲染、抽屉开关路径、移动样式契约（入口行横滚/安全区/键盘偏移/16px）、visualViewport 挂载清理；
  - `SettingsViewMobile.test.ts`（3）：master-detail 类切换、返回按钮行为、桌面不挂类；
  - `mobile-shell-layout.test.ts`（6）：外壳布局契约（§二 修复）——`#app` 前缀覆盖库"整页滚动"模式的定高/收缩/滚动归属断言 + 固定视口地基守护 + `--app-h` 视口高度兜底契约 + 顶栏可达性契约（全屏类匹配/品牌可缩/header-left 放开收缩）。
- 交互走查清单：入口行 7 按钮全部可达（横滚到底）；5 个弹出层开/关；设置 9 分类可达、工作台可进详情可返回；输入框聚焦不缩放；键盘弹出输入区可见；**消息区触摸下滑可看历史、上滑回底、顶部/底部可达**（§二 修复的验收项）。
