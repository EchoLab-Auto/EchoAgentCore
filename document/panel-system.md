---
id: panel-system
title: "Panel 系统交互"
group: 前端模块
link: ["panel-interaction | Panel 布局 & 交互定义"]
x: 332
y: 216
---

# Panel 系统交互

Panel 的系统级交互：Toast 通知、键盘清单、设计边界（协议已定义但 UI 未接线的命令）与全部关键常量速查表。

## 十二、Toast 系统

- 三类：`info / success / error`；位置右上；单条 **6s** 自动消失；同屏上限 8 条（`ToastProvider :max-count="8"`，溢出挤掉最旧）；队列 cap 8、文本截断 512 字符（`App.vue:233, 367`，`state.ts:912-914`）
- `state.toasts` 仅作转发队列：watcher 逐条泵入组件库 ToastProvider 后清空（`App.vue:226-235`）
- 来源与类型：断连/重连提示（info）、断连时发送命令（error）、`SudoResolved`（授权 success / 拒绝或中断 error）、Core `Error` 事件（**按 info 展示**，`state.ts:762-765`——Git 安装等异步操作的失败也经此通道呈现）

## 十三、键盘清单

Panel 无全局快捷键系统；所有键处理局部于组件：

| 键 | 位置 | 行为 |
|---|---|---|
| Enter | 输入区 | 发送消息（IME 组合中不触发） |
| Shift+Enter | 输入区 | 换行 |
| Enter | SudoModal | 授权（提交密码） |
| Esc | SudoModal | 拒绝（提交 null） |
| Enter | Shell 命令行 | 执行命令 |
| Esc | Shell 命令行 | 清空输入 |

## 十四、设计边界（协议已定义但 UI 未接线）

以下 `BackendCommand` 在协议层存在，但当前 UI 无任何入口——属有意留白而非缺陷，新增入口时按本文档规范补充：

- `SwitchModel` / `SwitchProvider` / `SetSystemPrompt` / `RequestSystemPrompt`（模型与提示词经 API 设置/AgentConfig 覆盖）
- `StartAllAdapters` / `StopAllAdapters`（逐个适配器控制已覆盖）
- `SetQqOwner`（管理员显示为只读）

> 2026-09-03 起 `InstallSkillFromGit` / `UpdateSkillFromGit` / `RemoveSkillSource` 已接线（§9.3），不再属于本清单。

## 十五、关键常量速查

| 常量 | 值 | 位置 |
|---|---|---|
| WS 重连退避 | 500ms ×2，上限 30s | connection.ts |
| 前台自愈 / 中继收割 | 探测帧 5s 判死；一侧 90s 无帧断链 | connection.ts / proxy.rs |
| Toast | 6s；队列/同屏 8；≤512 字符；右上 | main.ts:11 / App.vue:222, 344 / state.ts:912-914 |
| 主时间线容量 | 1024 条 | timeline.ts:6-11 |
| 推理打字机 | 24ms/tick，约 6s 封顶，≥2 字符/tick | ReasoningBlock.vue:18-53 |
| 消息入场动画 | 0.28s（淡入 + 上移 6px） | ChatView.vue:383, 864-870 |
| 滚动跟随阈值 / 让位 | 120px；按钮 180px / 内边距 190px | main.ts:10 / ChatView.vue:533-535, 566-568 |
| 图片附件 | 最长边 1600px；JPEG q0.85 / PNG 保格式；待发缩略图 64×64；历史图最大 260×200 | ChatView.vue:202-214, 634-642 |
| 工具输出上限 / 输入摘要截断 | 4000 字符；120 字符（非 JSON 兜底 200） | state.ts:497 / helpers.ts:11-24 |
| 输入区最大高度 | `calc(8em + 20px)` | ChatView.vue:739-742 |
| 侧边栏宽度 | 264px（折叠 0） | App.vue:236-238 |
| 设置视图 | 一级菜单 168px；条目列表 250px；API 页 ≤720px | SettingsView.vue / ApiSettings.vue:428 |
| 入口行弹出层 | 宽 `min(520px, 82vw)`；无全屏遮罩 | ChatView.vue:44-81, 848-861 |
| z-index | 清单浮层 3；Agent 菜单 30；模态/Toast 库管理 | styles.css:352 / AgentSwitcher.vue:188 |
| Agent 切换器 | 卡片高 34px；菜单 min-width 220px、max-height `min(60vh, 100vh-200px)` | AgentSwitcher.vue:120-199 |
| 轮询：QQ 状态 / 日志 / 任务耗时 | 5s / 5s / 1s | QqLoginSection:42 / LogView / TasksPanel:92-94 |
| HTTP 超时 | 10s | api.ts:4 |
| 二维码有效期 / 定时器摘要截断 | 约 2 分钟；160 字符 | QqLoginSection:131 / state.ts:802-805 |

