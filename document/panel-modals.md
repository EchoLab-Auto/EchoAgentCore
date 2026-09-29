---
id: panel-modals
title: "Panel 模态与覆盖层"
group: 前端模块
x: 1186
y: 766
---

# Panel 模态与覆盖层

全部模态与覆盖层行为：分支详情、上下文弹层、Agent 配置弹层，以及各危险操作的确认形式（原生 confirm / 两步确认）。（原 Sudo 授权弹窗与内联选单卡片随 `run_sudo` / `present_menu` 工具于 2026-09 废弃移除。）

## 八、模态与覆盖层

### 8.2 BranchModal（分支详情）

侧边栏分支行点击进入：头部 `临时分支 · {id前8位}` + 状态标签（运行中 primary/已完成 success/已取消 warning/失败 error）；正文 = 任务/目标/会话元信息 + 分支内消息流（按 kind 分别用紧凑工具行组（ToolRunGroup，2026-09-30 起）/推理块/纯文本渲染，新消息自动滚底；消息区 max-height 55vh）；运行中时底部提示「分支执行中…」；分支 tab 被移除后弹窗保持打开，显示「（分支已结束并合并到主会话）」。

### 8.3 ContextView（上下文弹层）

入口行「上下文」进入，弹层几何与 AgentConfigModal 一致（上/左/右 12px、底边 = 入口行高 + 42px，磨砂玻璃卡片，`ChatView.vue` `ctxBottomOffsetCss`）；**无独立返回按钮，点弹层外遮罩即关闭**。打开时按当前（Agent, 会话）发送 `RequestContext{team_id, session_id}`（按人格+会话定向取上下文快照，切换会话自动重拉、忽略他属陈旧快照；无 active agent 时为 `null`）：

- token 仪表盘：`NeumorphismProgress`（≥90% error、≥70% warning），总量/提示词/历史/上限四项统计
- 上下文块分「系统提示词与注入块」「对话历史」两组折叠（**全部块默认展开**，每块内容区 220px 内滚动；2026-09 修正——此前仅 base 展开，用户反馈「看不到内容」）：kind 徽标（覆盖 Core 全部种类含「工作区会话」「系统提示词」；名称自动剥掉与徽标重复的前缀）+ token 数 + **占当前上下文总量的百分比**（<1% 显式标注；顶部「总占用」同口径，按全部块求和——Core 的 `total_tokens` 只计历史）
- 逐条消息明细（角色 + token + 内容），默认折叠
- 操作：**清理历史**（两步确认：首击变为「确认清理？不可恢复」，再击发送 `ClearHistory`）；**归档**（confirm → `ArchiveHistory`）；**压缩**（confirm → `CompactHistory{keep_recent: 40}`，执行前**自动把快照写进 `archives/*-precompact.json`**，摘要由 LLM 生成、失败回退统计文案；结果消息含压缩条数/组数/归档路径）；**重载技能**（`ReloadSkills`；重载**所有运行中智能体+管理代理**的技能注册表，不只是当前查看的——回执「技能已重新加载（N/M 个智能体更新）」、无变化时「技能无变化」、部分失败逐个人格报错）；**刷新**

### 8.4 AgentConfigModal（当前 Agent 配置）

锚定会话区上方的磨砂覆盖层（底边 = 入口行高 + 42px，`ChatView.vue:395-402, 874-880`）；点遮罩/✕/取消/保存后关闭。打开时拉取技能/工具/插件清单：

- 字段：名称、描述、系统提示词（留空继承全局）、启用开关（**禁用即卸载记忆**，重启用重新挂载）、**循环模式分段单选**（单会话（默认）/ 并行多会话——写入 `enabled_plugins` 白名单的 `echo-agent.loop.{single,parallel}` 插件 id，二选一）
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

