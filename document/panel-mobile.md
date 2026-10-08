---
id: panel-mobile
title: "移动端适配（竖屏）"
group: 前端
x: 1920
y: 560
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
| 视口高度 | `height: 100%` 链 | `100dvh`（iOS 地址栏伸缩不溢出/抖动） |
| 安全区 | — | `index.html` 的 `viewport-fit=cover` + 顶栏 `padding-top: env(safe-area-inset-top)`、输入区 `env(safe-area-inset-bottom)` |
| 顶栏 | 品牌 + 聊天/设置 + 全屏 + 主题 | 品牌字号收敛；**全屏按钮隐藏**（触摸端意义低）；新增「卡片」入口（仅会话视图，见 §四） |
| 触控目标 | — | `@media (pointer: coarse)`：顶栏与入口行按钮 `min-height: 44px` |
| Toast | `top-right` | `top-center`（窄屏右上角贴近拇指区，易遮挡） |
| 库 Modal | — | mask 补 `padding: 8px`（库 mask 无内边距，375px 屏上贴边） |

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

- **尺寸**：375×667 / 390×844 / 430×932（浏览器 + PWA standalone，含安全区）。
- **回归**：≥768px 桌面关键视图像素不变（改动全在媒体查询内）。
- **单测**：
  - `ChatViewMobile.test.ts`（4）：rail 移动不渲染/桌面渲染、抽屉开关路径、移动样式契约（入口行横滚/安全区/键盘偏移/16px）、visualViewport 挂载清理；
  - `SettingsViewMobile.test.ts`（3）：master-detail 类切换、返回按钮行为、桌面不挂类。
- 交互走查清单：入口行 7 按钮全部可达（横滚到底）；5 个弹出层开/关；设置 9 分类可达、工作台可进详情可返回；输入框聚焦不缩放；键盘弹出输入区可见。
