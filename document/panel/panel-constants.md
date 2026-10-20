---
id: panel-constants
title: "关键常量与设计边界"
group: 前端
x: 2880
y: 500
---

# 关键常量与设计边界

> **定位**：Panel 前端的**参考手册**——一张跨视图的关键常量速查表（路由、退避、阈值、容量、尺寸、层级），以及「协议已定义但 UI 未接线」的设计边界清单。这里**不写交互语义**（谁在什么状态做什么见[交互定义](./panel-interaction.md)），也不写空间安排（见[桌面端布局](./panel-desktop-layout.md)）——本文只回答「这个数是几」「这条命令有没有入口」。读者：改样式/阈值前查数、判断某命令是否接线的前端与排障者。

## 设计边界（协议已定义但 UI 未接线）

以下 `BackendCommand` 在协议层存在，但当前 UI 无任何入口——属有意留白而非缺陷，新增入口时按本文档规范补充：
- `SwitchModel` / `SwitchProvider` / `SetSystemPrompt`（模型与提示词经 API 设置/AgentConfig 覆盖）
- `StartAllInstances` / `StopAllInstances`（逐个插件实例控制已覆盖）
- 实例管理（门控/名单/owner/登录）：统一 `InvokeInstanceOp`；owner 显示为只读

> `RequestSystemPrompt` 已接线：切 core 时随「单值管理面」重拉发送（`connection.ts::requestActiveRegionManagement`），`pending.ts` 的关联表登记 `SystemPrompt` 响应销账；**但 Panel 不落状态**——`AppState.model` / `systemPrompt` 字段已删除（事件写入后零读取方；模型名可由 api/会话归属推导，`SystemPrompt` 回推文本仅面向调试）。`InstallSkillFromGit` / `UpdateSkillFromGit` / `RemoveSkillSource` 亦已接线（见 [设置 · 技能](./panel-settings-skills.md)），均不再属于本清单。

## 关键常量速查

| 常量 | 值 | 位置 |
|---|---|---|
| 多接入点复合键 | `(core, id)`，`<core>\u0000<id>`；core 为空退化为裸 id（单 core 行为与磁盘缓存零变化） | keys.ts |
| 命令目标路由 | 显式 targetCore > 会话归属 > 人格归属 > activeRegion；解析不出且 knownCores>1 时拒发提示 | `connection.ts` 的 `resolveTargetCore` |
| WS 重连退避 | 500ms ×2，上限 30s | connection.ts |
| 加载态阈值 | 显示延迟 150ms；最短可见 400ms；慢 6s；超时 20s | pending.ts |
| 前台自愈 / 中继收割 | 探测帧 5s 判死；一侧 90s 无帧断链；转发写阻塞 10s 断链 | connection.ts / proxy.rs |
| Toast | 6s；队列/同屏 8；≤512 字符；右上 | `state.ts` 的 `pushToast` / `App.vue` 的 Toast 桥 / `ToastProvider` 的 `max-count` |
| 主时间线容量 | 1024 条 | `timeline.ts` 的 trunk 裁剪 |
| 时间线磁盘缓存 | 挂载前 hydrate + 10s 周期落盘；总预算 2.2M 字符、单 team 1.8M 字符 | trunk-cache.ts |
| TrunkTimeline 快照瘦身 | 近 40 条推理全文、更早截 240 字；静态资源 gzip 协商（≥1KB 文本）+ 弱 ETag/304（HTML no-store、`assets/` 一年 immutable） | timeline.rs / static_files.rs |
| 推理打字机 | 24ms/tick，约 6s 封顶，≥2 字符/tick；默认折叠 + 推演中限高（180px）滚动钉底 | ReasoningText.vue |
| 消息入场动画 | 0.28s（淡入 + 上移 6px） | ChatView.vue |
| 消息列表行距 | 容器 gap 6px；消息气泡另加 3px 边距（气泡间约 12px）；工作行（推理/工具/子代理）紧排 6px | ChatView.vue |
| 工作行尺寸 | 连续工具/推理/子代理统一为一个工作行组（chip 一排）：工作图标 chip 30×26、chip 间距 4px、组内行间距 5px（一般化后三类共用同一几何，不再各自定义高度） | WorkRunGroup.vue |
| 滚动跟随阈值 / 让位 | 120px；「回到底部」按钮 `right = trayPadRight + 16 / bottom = entryBottom + 45`（浮层打开时隐藏）；内边距 190px | `useChatScroll` 的 `threshold` / `ChatView.vue` 的 `.chat-view__jump` 与 `.chat-view__list` |
| 图片附件 | 最长边 1600px；JPEG q0.85 / PNG 保格式；待发缩略图 64×64；历史图最大 260×200 | `ChatView.vue` 的图片压缩工具与历史图样式 |
| 工具输出上限 / 输入摘要截断 | 4000 字符；120 字符（非 JSON 兜底 200） | `timeline.ts` 的 `bounded(…, 4000)` / `helpers.ts` 的 `truncate(oneLine(text), 120)` |
| 输入区最大高度 | `calc(8em + 20px)` | `ChatView.vue` 的输入区 `max-height` |
| 边栏列宽（chat-rail） | 324px（左右两列；容器底 = `entryBottom + 45px`） | `ChatView.vue` 的 `RAIL_WIDTH` |
| 安全区 / 键盘（仅窄屏） | iPhone 实测 `safe-top 47` / `safe-bottom 34`；`keyboardInset` <80px 视为抖动归零 | `index.html` 的 `viewport-fit=cover` / `ChatView.vue` 的 `updateKeyboardInset` |
| 窄屏底部三公式 | 输入区 `bottom: 8 + safe + kb`；入口行 `bottom: entryBottom + safe + kb`；消息留白 `190 + safe + kb` | `ChatView.vue` 的 `@media` 段 |
| 设备圆角 token | `--panel-display-radius`：桌面 0 / 窄屏 55（iPhone 12–16 实测口径；SE 与平边 Android 近似值偏大，但对使用方安全） | `styles.css` 的「设备圆角」段 |
| 输入区圆角 | 桌面 `--composer-radius: 24px`（内框/按钮 24−12=12）；窄屏 `clamp(20px, 屏圆角−8px, 34px)` = 34（内框/按钮 24） | `ChatView.vue` 的 `.chat-composer-float` |
| 设置视图 | 一级菜单 168px；条目列表 250px；API 概览 ≤720px（表单按需展开）；智能体编辑器 = 头部卡 + 3 折叠分区（默认展开前两个） | SettingsView.vue / ApiSettings.vue |
| 入口行弹出层 | 默认宽 520px，视口不足按 `100vw - 24px` 收窄；无全屏遮罩（点外部即关） | PanelPopover.vue |
| 入口行按钮 | ui-frame `NeumorphismButton`（glass/pill/small；`--nm-glass-bg` 45% 更透 + blur 24px）；清单徽标 `NeumorphismBadge` | ChatView.vue |
| z-index 阶梯 | 清单卡 3（`.checklist-float`，容器内 sticky）/ 边栏列 4 / 输入区与「回到底部」5 / 入口行 6 / 入口行弹出层与 Agent 菜单 30（`PanelPopover`；设置视图同段 30–31）/ 根 chip 速览 40 / 浮层遮罩 50 / 目录选择器 60 / 拖动幽灵 200；模态与 Toast 由库统一分配 | styles.css / ChatView.vue / PanelPopover.vue / WorkspaceFileBrowser.vue / AgentSwitcher.vue / RailDragGhost.vue |
| Agent 切换器 | 卡片高 34px（窄屏 ≤140px 宽、隐藏区域徽标）；菜单 = `PanelPopover` 宽 360px / 内容 `max-height: 52vh`（窄屏贴底全宽） | `AgentSwitcher.vue` 的卡片样式 + `PanelPopover.vue` |
| 轮询：日志 / 任务耗时 | 5s / 1s | `LogView.vue` 的自动刷新定时器 / `TasksPanel.vue` 的耗时刷新（QQ 登录状态为 WS 事件驱动，无轮询） |
| HTTP 超时 | 10s | `api.ts` 的 `FETCH_TIMEOUT_MS` |
| 二维码有效期 / 定时器摘要截断 | 约 2 分钟；160 字符 | 插件实例描述符 `image` 节点（`login_qrcode`，`instance-ui` 渲染器呈现）/ `helpers.ts` 的定时器摘要截断 |

## 维护约定

常量散落在各组件与模块里，本表只是**索引**（值以代码为准，改代码时同步本表）：

- 新增/修改阈值类常量（超时、退避、容量、尺寸断点）→ 补一行，并在「位置」列写**文件 + 符号**，不写行号
- 某条 `BackendCommand` 接通了 UI → 从「设计边界」删掉，并在对应视图文档补交互语义（本文不描述操作流程）
- 层级类数值的权威表在[桌面端布局](./panel-desktop-layout.md)「层叠秩序」，本表只做汇总，冲突时以布局文档为准
