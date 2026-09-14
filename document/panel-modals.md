---
id: panel-modals
title: "Panel 模态与覆盖层"
group: 前端模块
x: 1186
y: 766
---

# Panel 模态与覆盖层

全部模态与覆盖层行为：Sudo 授权（不可绕过）、分支详情、上下文弹层、Agent 配置弹层，以及各危险操作的确认形式（原生 confirm / 两步确认）。
（选单 menu 不在此列：2026-09-14 起为**会话区内联卡片**，见 §8.1b。）

## 八、模态与覆盖层

### 8.1 SudoModal（最高优先级）

由 Core `SudoRequest` 事件触发，**不可绕过**：`closable=false`、`mask-closable=false`、无关闭按钮、无页脚。

```prodoc-flow
graph LR
  Req[SudoRequest 事件] --> Modal[密码框自动聚焦]
  Modal -->|Enter / 授权| Auth[sendSudoPassword: 密码]
  Modal -->|Esc / 拒绝| Deny[sendSudoPassword: null]
  Auth --> Wait[等待 Core]
  Deny --> Wait
  Wait --> Resolved[SudoResolved: 卸载弹窗 + toast]
```

- 密码经专用 `sudo_password` 帧（不进命令队列/日志/LLM 上下文）；提交后清空输入框
- `SudoResolved` 到达即关闭弹窗并弹 toast（授权 success / 拒绝或中断 error）；Core 侧保证所有退出路径（含超时、turn 取消）恰好发一次——弹窗不会挂在死请求上

### 8.1b MenuCard（选单，menu 插件）——**内联于会话区**

由 Core `MenuRequest` 事件触发（模型调用 `present_menu` 工具）：标题 + 可选说明 + 选项列表。
**不是弹窗**（2026-09-14 改造）：选单作为合成条目追加到会话区消息流尾部，与消息同列滚动，
不遮挡面板、不抢输入焦点。

```prodoc-flow
graph LR
  Req[MenuRequest 事件] --> Card[会话区尾部的选单卡片]
  Card -->|点击选项 / 数字键 1-9（聚焦时）| Pick[sendMenuAnswer: option_id]
  Card -->|Esc / 取消按钮| Cancel[sendMenuAnswer: null]
  Pick --> Wait[等待 Core]
  Cancel --> Wait
  Wait --> Resolved[MenuResolved: 卡片消失]
```

- **三态分明**：选定回传 `option_id`；取消回传 `null`（模型收到"用户取消"，**不会**被当成某个选项）；超时/中断由 Core 兜底发 `MenuResolved`——卡片不会挂在死请求上
- **归属过滤与消息同语义**：合成条目 id = `menu:{request_id}`，仅在该选单归属当前会话（或全局视图）时追加到列表；`MenuResolved` 到达即随 store 清空自动消失
- 不做自动聚焦：会话区属输入流，抢焦点会打断用户正在输入的内容；键盘可选（卡片聚焦时数字键 1-9 直选、Esc 取消），鼠标点击始终可用
- 交互：选项行显示序号 + label + 可选说明；`timeout_secs > 0` 时页脚提示等待时长
- 应答经专用 `menu_answer` 帧（不经命令队列——应答不是新输入，不开启新 turn）；选定静默消失，取消/超时才弹 toast
- 门控：`echo-agent.menu` 插件未启用时模型侧没有该工具（见 [插件化设计](./core-plugins.md)）；QQ 会话里用户看不到面板内的选单卡片（工具描述要求改用文字询问）

### 8.2 BranchModal（分支详情）

侧边栏分支行点击进入：头部 `临时分支 · {id前8位}` + 状态标签（运行中 primary/已完成 success/已取消 warning/失败 error）；正文 = 任务/目标/会话元信息 + 分支内消息流（按 kind 分别用工具卡/推理块/纯文本渲染，新消息自动滚底；消息区 max-height 55vh）；运行中时底部提示「分支执行中…」；分支 tab 被移除后弹窗保持打开，显示「（分支已结束并合并到主会话）」。

### 8.3 ContextView（上下文弹层）

入口行「上下文」进入，弹层几何与 AgentConfigModal 一致（上/左/右 12px、底边 = 入口行高 + 42px，磨砂玻璃卡片，`ChatView.vue` `ctxBottomOffsetCss`）；**无独立返回按钮，点弹层外遮罩即关闭**。打开时按当前 Agent 发送 `RequestContext{team_id: 当前 Agent id}`（按人格定向取上下文快照；无 active agent 时为 `null`）：

- token 仪表盘：`NeumorphismProgress`（≥90% error、≥70% warning），总量/提示词/历史/上限四项统计
- 上下文块分「系统提示词与注入块」「对话历史」两组折叠（仅 base 块默认展开）：kind 色签 + token 数 + 占比条（按最大块 token 归一）
- 逐条消息明细（角色 + token + 内容），默认折叠
- 操作：**清理历史**（两步确认：首击变为「确认清理？不可恢复」，再击发送 `ClearHistory`）；**归档**（confirm → `ArchiveHistory`）；**压缩**（confirm → `CompactHistory{keep_recent: 40}`）；**重载技能**（`ReloadSkills`）；**刷新**

### 8.4 AgentConfigModal（当前 Agent 配置）

锚定会话区上方的磨砂覆盖层（底边 = 入口行高 + 42px，`ChatView.vue:152-157, 399-405`）；点遮罩/✕/取消/保存后关闭。打开时拉取技能/工具/插件清单：

- 字段：名称、描述、系统提示词（留空继承全局）、启用开关（**禁用即卸载记忆**，重启用重新挂载）
- **API 供应商下拉**（2026-09 新增）：选项 = 「跟随全局默认配置」+ 全局供应商池各 profile（`name（provider / model · key 状态）`）；保存写入 `SaveTeam.api_profile`，运行期立即重建该 persona 的 provider（见 [设置视图 §9.1.1](./panel-settings.md)）
- 三类能力复选表（插件按 kind 分组、工具/技能按包分组，组级全选）：**白名单语义——空表 = 全部启用**；首次取消勾选时先把全量写入列表再移除该项
  - 「启用插件」区成员 = `kind ∈ {adapter, management, interaction}` ∪ 包维度门控插件固定清单（单一来源 `capabilities.ts::isPluginCheckboxVisible`，与设置视图同一常量；详见 [插件化设计](./core-plugins.md)「新增包维度门控插件时的同步清单」）。分组按 kind：适配器 / 交互独立成组，其余整类能力项归入「管理面」
- **保存即热生效**：勾选/取消立即影响该 Agent 的模型可见能力（取消 = 禁用，重新勾选 = 恢复，无需重启；全局禁用优先，不受勾选覆盖）
- 保存 → `SaveTeam`

### 8.5 原生确认与受保护操作

| 操作 | 确认形式 |
|---|---|
| 禁用「管理面」插件 | **禁止**（core 拒绝命令 + UI alert 明示，防自锁；只能 core.toml + 重启） |
| 删除技能 | `window.confirm`（提示会删除 SKILL.md 文件） |
| 删除 Team | 仅当**只剩一个智能体**时 `window.alert` 拒绝（去主智能体：无受保护成员）；否则 `window.confirm`（不可恢复） |
| 移除技能 Git 来源 | `window.confirm`（明示"目录保留，可手动删除"） |
| 归档/压缩历史 | `window.confirm` |
| 清理历史 | 两步按钮确认（见 §8.3） |
| 删除 API Profile / 更新 Git 技能 | **无确认**，立即生效 |

