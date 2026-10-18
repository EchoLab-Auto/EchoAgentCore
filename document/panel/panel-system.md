---
id: panel-system
title: "Panel 系统交互"
group: 前端
x: 3840
y: 780
---

# Panel 系统交互

> **定位**：Panel 的系统级交互：Toast 通知、键盘清单、设计边界（协议已定义但 UI 未接线的命令）与全部关键常量速查表。读者：Panel 前端开发者与排障者；相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)、[布局与导航](./panel-layout.md)。

## Toast 系统

- 三类：`info / success / error`；位置右上；单条 **6s** 自动消失；同屏上限 8 条（`ToastProvider` 的 `max-count`，溢出挤掉最旧）；队列 cap 8、文本截断 512 字符（`App.vue` 的 Toast 桥 + `state.ts` 的 `pushToast`）
- `state.toasts` 仅作转发队列：watcher 逐条泵入组件库 ToastProvider 后清空（`App.vue` 的 Toast 桥）
- 来源与类型：断连提示「与后端断开，正在重连…」（error）、断连时发送命令（error）、Core `Error` 事件（**按 info 展示**，`state.ts` 的 `BackendEvent.Error` 分支——Git 安装等异步操作的失败也经此通道呈现）

## 键盘清单

Panel 无全局快捷键系统；所有键处理局部于组件：

| 键 | 位置 | 行为 |
|---|---|---|
| Enter | 输入区 | 发送消息（IME 组合中不触发） |
| Shift+Enter | 输入区 | 换行 |
| Enter | Shell 命令行 | 执行命令 |
| Esc | Shell 命令行 | 清空输入 |
| Esc | 边栏卡片拖拽中 | 取消拖拽（`RailStack.vue` 的拖拽态 `Escape` 分支） |
| Enter | 设置视图技能触发词输入 | 添加触发词（`SettingsView.vue` 的「触发词就地添加」输入） |

## 设计边界（协议已定义但 UI 未接线）

以下 `BackendCommand` 在协议层存在，但当前 UI 无任何入口——属有意留白而非缺陷，新增入口时按本文档规范补充：
- `SwitchModel` / `SwitchProvider` / `SetSystemPrompt`（模型与提示词经 API 设置/AgentConfig 覆盖）
- `StartAllInstances` / `StopAllInstances`（逐个插件实例控制已覆盖）
- 实例管理（门控/名单/owner/登录）：统一 `InvokeInstanceOp`；owner 显示为只读

> `RequestSystemPrompt` 已接线：切 core 时随「单值管理面」重拉发送（`connection.ts::requestActiveRegionManagement`），`pending.ts` 的关联表登记 `SystemPrompt` 响应销账；**但 Panel 不落状态**——`AppState.model` / `systemPrompt` 字段已删除（事件写入后零读取方；模型名可由 api/会话归属推导，`SystemPrompt` 回推文本仅面向调试）。`InstallSkillFromGit` / `UpdateSkillFromGit` / `RemoveSkillSource` 亦已接线（见 [设置视图](./panel-settings.md)），均不再属于本清单。

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
| 滚动跟随阈值 / 让位 | 120px；「回到底部」按钮 `right = trayPadRight + 16 / bottom = entryBottom + 45`（浮层打开时隐藏）；内边距 190px | main.ts / ChatView.vue |
| 图片附件 | 最长边 1600px；JPEG q0.85 / PNG 保格式；待发缩略图 64×64；历史图最大 260×200 | `ChatView.vue` 的图片压缩工具与历史图样式 |
| 工具输出上限 / 输入摘要截断 | 4000 字符；120 字符（非 JSON 兜底 200） | `timeline.ts` 的 `bounded(…, 4000)` / `helpers.ts` 的 `truncate(oneLine(text), 120)` |
| 输入区最大高度 | `calc(8em + 20px)` | `ChatView.vue` 的输入区 `max-height` |
| 边栏列宽（chat-rail） | 324px（左右两列；容器底 = `entryBottom + 45px`） | `ChatView.vue` 的 `RAIL_WIDTH` |
| 设置视图 | 一级菜单 168px；条目列表 250px；API 概览 ≤720px（表单按需展开）；智能体编辑器 = 头部卡 + 3 折叠分区（默认展开前两个） | SettingsView.vue / ApiSettings.vue |
| 入口行弹出层 | 宽 `min(520px, 82vw)`；无全屏遮罩 | ChatView.vue |
| 入口行按钮 | ui-frame `NeumorphismButton`（glass/pill/small；`--nm-glass-bg` 45% 更透 + blur 24px）；清单徽标 `NeumorphismBadge` | ChatView.vue |
| z-index 阶梯 | 清单卡 3（`.checklist-float`，容器内 sticky）/ 边栏列 4 / 输入区与「回到底部」5 / 入口行 6 / 入口行弹出层与 Agent 菜单 30（`PanelPopover`；设置视图同段 30–31）/ 根 chip 速览 40 / 浮层遮罩 50 / 拖动幽灵 200；模态与 Toast 由库统一分配 | styles.css / ChatView.vue / PanelPopover.vue / WorkspaceFileBrowser.vue / AgentSwitcher.vue / RailDragGhost.vue |
| Agent 切换器 | 卡片高 34px；菜单 min-width 220px、max-height `min(60vh, 100vh-200px)` | `AgentSwitcher.vue` 的卡片与菜单样式 |
| 轮询：日志 / 任务耗时 | 5s / 1s | LogView:48 / TasksPanel:70-84（QQ 登录状态为 WS 事件驱动，无轮询） |
| HTTP 超时 | 10s | `api.ts` 的 `FETCH_TIMEOUT_MS` |
| 二维码有效期 / 定时器摘要截断 | 约 2 分钟；160 字符 | 插件实例描述符 `image` 节点（`login_qrcode`，`adapter-ui` 渲染器呈现）/ `helpers.ts` 的定时器摘要截断 |
