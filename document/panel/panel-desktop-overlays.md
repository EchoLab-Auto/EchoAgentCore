---
id: panel-desktop-overlays
title: "桌面端模态与覆盖层"
group: 前端
x: 1920
y: 780
---

# 桌面端模态与覆盖层

> **定位**：**≥768px** 下 Panel 的模态与覆盖层——有哪些、各在哪一层、怎么关、哪些操作必须先问一句。包含与视口无关的内容契约（上下文占用的口径、Agent 配置的字段与白名单语义、确认形式矩阵、分支查看器的行模型、目录选择器的两级浏览）与本视口特有的几何（会话区覆盖层贴边 12px、入口行弹层 520px）。读者：维护 Panel 交互的开发者；上游：[桌面端布局](./panel-desktop-layout.md)；窄屏形态见[移动端模态与覆盖层](./panel-mobile-overlays.md)；相关：[交互定义](./panel-interaction.md)、[设置 · 技能](./panel-settings-skills.md)。

> **为什么单独成篇**：这些层散落在 `ChatView` / `App.vue` / `SettingsView` / 工作区面板里，最容易出的问题是「多弹一层压住按钮」「Esc 关错原子」「z-index 差 10 被遮」「遮罩点不到」。所以「谁在几层、谁来关、哪种操作要问」集中在这里，改一处就知道牵连谁。

## 覆盖层清单与分层

Panel 的浮层按**阻断强度**分三级——轻的不拦人，重的必须先处理完：

| 级别 | 行为 | 关闭方式 | 成员 |
| --- | --- | --- | --- |
| **轻：内容浮层** | 无遮罩，不拦下层的点击 | 点外部 / `Esc` | 入口行六项（清单 / 会话 / 工作区 / 实例 / 任务 / 模型）、Agent 菜单 |
| **中：会话区覆盖层** | 遮罩罩住聊天区，顶栏与侧栏仍可用 | 点遮罩空白 | 上下文占用、Agent 配置（`z-index: 50`） |
| **重：应用级模态** | 库模态遮罩铺满视口，焦点锁在框内 | 库模态 Esc / ✕ / 明确按钮 | 分支详情、全局确认框、目录选择器、Git 安装技能 |

| 覆盖层 | 组件 | 载体与层位 | 入口 | 契约 |
| --- | --- | --- | --- | --- |
| 上下文占用 | `ContextView` + `.ctx-overlay` 外壳 | 会话区内绝对定位 + 遮罩，`z-index: 50` | 入口行「上下文」（受 `echo-agent.management.panel` 插件门控） | 本文「会话区覆盖层」 |
| Agent 配置 | `AgentConfigModal` | 同上（`z-index: 50`） | 入口行「配置」 | 本文「会话区覆盖层」 |
| 分支详情 | `BranchModal` | 库 `NeumorphismModal`（App 根，`size=large`、无 footer） | 侧栏「临时分支」卡的分支行（仅并行多会话模式） | 本文「分支查看器」 |
| 全局确认 | `ConfirmDialog` | 库 `NeumorphismModal`（App 根挂载一次，`size=small`） | 任意组件调 `confirmDialog()` | 本文「全局确认与确认形式」 |
| 目录选择器 | `DirectoryPicker` | 自绘遮罩 + `Teleport to="body"`，`z-index: 60` | 工作区面板「浏览…」 | 本文「目录选择器」 |
| Git 安装技能 | `SettingsView` 内联 | 库 `NeumorphismModal`（`size=small`、无 footer） | 设置 → 技能 → 工具条「Git 安装」 | [设置 · 技能](./panel-settings-skills.md) |
| 入口行弹层（六项） | `PanelPopover` | `teleport` 到 body + fixed，`z-index: 30` | 清单 / 会话 / 工作区 / 实例 / 任务 / 模型 | [入口行](./panel-chat-entry.md)「入口行按钮」 |
| Agent 菜单 | `AgentSwitcher` + `PanelPopover` | 与入口行弹层同一容器（`teleport` 到 body + fixed，宽 360px、内容 `max-height: 52vh`、`z-index: 30`） | 入口行最左的 Agent 卡片 | [桌面端布局](./panel-desktop-layout.md)「Agent 切换器」 |

**层叠秩序**（完整阶梯见[桌面端布局](./panel-desktop-layout.md)「层叠秩序」）：边栏列 4 → 输入区与「回到底部」5 → 入口行 6 → 入口行弹层与 Agent 菜单 30 → 根 chip 速览 40 → 会话区覆盖层 50 → 目录选择器 60 → 拖动幽灵 200 → 库模态与 Toast（库统一分配，恒在应用层之上）。

## 会话区覆盖层

上下文占用与 Agent 配置共用同一套外壳配方（`ChatView.vue` 的 `.ctx-overlay` 与 `AgentConfigModal.vue` 的 `.agent-config-overlay`）：

- **遮罩在会话区之内**：`position: absolute; inset: 0`（相对 `.chat-view`），不是全屏——顶栏与侧栏保持可见，读作「仍在同一页里换个焦点」
- **几何**：卡片距会话框上/左/右 **12px**，底边 = 入口行底偏移 + 30px（按钮高）+ 12px 间隙（`configBottomOffset`；上下文用 `ctxBottomOffsetCss` 复用同一数值）；磨砂玻璃配方——20px 圆角 + `blur(24px) saturate(1.6)` + 描边、投影与内高光
- **关闭**：点遮罩空白处（`@click.self`）即关；两者都**没有返回按钮**，也**不监听 Esc**（Esc 只属于入口行弹层与库模态）。打开期间「回到底部」按钮隐藏（`.chat-view--overlay-open`）
- **层位**：`z-index: 50`，高于入口行（6）与入口弹层（30），低于目录选择器（60）

### 上下文占用（ContextView）

外壳头部是「上下文占用」+ 当前人格 id（无激活人格时显示 `default`），正文为 `ContextView` 组件（只做内容，几何由外壳给）。打开即按当前（Agent, 会话）发 `RequestContext{team_id, session_id}`，切换会话自动重拉：

- **token 仪表盘**：`NeumorphismProgress`（≥90% error、≥70% warning）+ 四项统计——总占用 / 提示词 / 历史 / 上限。**总占用按全部块求和**（Core 的 `total_tokens` 只计历史，单看会出现「提示词 1091t 而总占用 0%」的错位）；比例不足 1% 时显式显示 `<1%`，不取整成 0%
- **两组折叠块**：「系统提示词与注入块（N）」与「对话历史（N）」，`ChatFold` **默认全展开**，每块内容区 `max-height: 220px` 内滚动；块头 = kind 徽标（覆盖 Core 全部种类，含「工作区会话」「系统提示词」）+ 名称（自动剥掉与徽标重复的前缀）+ token 数 + **占当前上下文总量的百分比** + 占比条（非零块给 0.8% 最小可见宽度，避免「有 token 却画不出条」）
- **逐条消息明细**（角色 + token + 内容）：**默认折叠**，点小节标题展开/收起
- **加载三态**：`sendPending('context')` 登记——未连接 spinner / 加载中骨架（仪表盘 + 2 块卡片，150ms 内到达不闪）/ 超时 `LoadHint` + 重试；已显示过快照时重开先亮旧快照再覆盖；「↻」按钮加载中转圈
- **五个操作的作用范围**（清理与归档是**会话级**，压缩是**人格级**）：

| 操作 | 下发命令 | 作用范围 |
| --- | --- | --- |
| 清理历史记忆 | `ClearHistory{team_id, session_id}` | **只清当前会话**；会话未知时 fail-closed 不发命令（缺 `session_id` 在 Core 语义是清空该人格全部会话） |
| 归档 | `ArchiveHistory{team_id, session_id}` | **只归档并清空当前会话**（完整快照写 `archives/`）；同样 fail-closed |
| 压缩 | `CompactHistory{team_id, keep_recent: 40}` | **该人格全部会话**（各会话独立分组摘要，LLM 生成可能数分钟；执行前自动写 `archives/*-precompact.json`，摘要失败回退统计文案） |
| 重载技能 | `ReloadSkills` | 全部运行中智能体 + 管理代理的技能注册表，不只是当前查看的那个；回执三态——「技能已重新加载（N/M 个智能体更新）」/「技能无变化」/「技能重载部分失败（N/M 个更新）：<逐个人格原因>」 |
| 刷新 | `RequestContext` | 重新拉取快照 |

### Agent 配置（AgentConfigModal）

锚定会话区的磨砂覆盖层，头部标题「配置 Agent：{名}」+ 人格 id；点遮罩、右上 ✕、取消、保存后关闭。打开时并行拉取技能 / 工具 / 插件三份清单（各自登记 `sendPending`）。字段：

| 字段 | 语义 |
| --- | --- |
| 名称 / 描述 / 系统提示词 | 提示词留空 = 继承全局 |
| 启用开关 | **禁用即卸载记忆**（正在运行的该 Agent 会被回收），重启用时重新挂载 |
| 循环模式 | 分段单选「单会话（默认）/ 并行多会话」——写入 `enabled_plugins` 白名单的 `echo-agent.loop.{single,parallel}` 插件 id（`loop-mode.ts::applyMode` / `deriveMode`） |
| API 供应商 | 下拉 = 「跟随全局默认配置」+ 全局供应商池各 profile，标签 `name（provider / model）`，**缺 key 时追加「· 无 key」**；保存写入 `SaveTeam.api_profile`，运行期重建该 persona 的 provider（见[设置 · API](./panel-settings-api.md)「Persona 级引用」） |

三类能力复选表（**白名单语义**，空表 = 全部启用；首次取消勾选时先把全量写入列表再移除该项）：

- **启用插件**：成员 = `kind ∈ {instance, management, interaction}` ∪ 包维度门控插件固定清单（单一来源 `capabilities.ts::isPluginCheckboxVisible`，与设置视图同一常量，见[插件化设计](../plugins/core-plugins.md)「新增包维度门控插件时的同步清单」）；组内按 kind 分「平台实例 / 交互 / 管理面」（`PLUGIN_GROUP_LABELS`，`adapter → 平台实例` 为旧 Core 的 wire 名兼容）
- **启用工具 / 启用技能**：按包（pkg）分组，组级全选

加载与保存：分区头计数在清单未到时显示「…」，分区体用 `LoadHint` 呈现等待 / 慢提示 / 超时重试，空态「（无）」只在确认无数据时出现；**保存即热生效**——勾选变化立刻影响该 Agent 的模型可见能力（全局禁用优先，不受勾选覆盖）。

## 分支查看器

`BranchModal` 挂在 App 根（与全局确认框同级，是仅有的两个应用级库模态之一；`NeumorphismModal size=large`、无 footer）：侧栏「临时分支」卡的分支行点击后，由 `App.vue` 的 `activeBranchId` 驱动打开——与会话视图没有父子关系，也不随视图切换关闭。

- **头部**：`临时分支 · {id 前 8 位}` + 状态标签（运行中 primary / 已完成 success / 已取消 warning / 失败 error）
- **元信息**：任务；目标 · 会话 id
- **正文**：分支内活动流，**行模型与主时间线同构**——连续的 `tool` / `reasoning` 合并成 `WorkRunGroup` 的 chip 行（同一组件、同一开合语义；回放路径不播打字机、`animate` 恒 false），`content` 走纯文本气泡、其余 kind 走 muted 文本；消息区 `max-height: 55vh`，新消息自动滚到底；空态「（暂无活动，等待分支执行…）」
- **运行中**：底部提示「分支执行中…」
- **分支消失后弹窗不关**：分支 tab 被移除（已合并）时正文换成「（分支已结束并合并到主会话）」

## 全局确认与确认形式

### ConfirmDialog（全局确认框）

`confirm.ts` 是**模块级服务**（不依赖组件树/teleport），`ConfirmDialog.vue` 在 App 根挂载一次；所有调用点保持同步式读法、异步实现：`if (!(await confirmDialog(message, options))) return;`

| 项 | 行为 |
| --- | --- |
| 选项 | `title`（缺省「确认操作」）、`confirmText`（缺省「确定」）、`cancelText`（缺省「取消」）、`danger`（确认按钮用错误色） |
| 结算 | 确定 → `true`；取消 / 遮罩 / 右上 ✕ / **Esc** → `false`（都经库模态的 `update:modelValue(false)` 走 `settleConfirm(false)`） |
| 并发 | 同一时间只展示一个：旧框未结算又发起新框时，旧框按**取消**结算，避免调用方悬挂 |
| 外观 | 库 `NeumorphismModal`（`size=small`、无 footer）提供焦点陷阱、`aria-modal`、Esc 与主题跟随；按钮是面板自绘的胶囊对（取消玻璃 / 确定主色，`danger` 时确定染 error 色）；消息文案保留换行（`pre-line`） |
| 替代对象 | 早期散落的 `window.confirm` / `alert`（阻塞式、样式不可控、暗色下白框刺眼）——现已统一 |

### 确认形式矩阵

阻断式操作按「代价 × 可逆性」分四档：**禁止**（Core 侧拒绝）、**两步按钮**（同一按钮二次点击即执行）、**确认框**（`confirmDialog`）、**无确认**（立即生效）。

| 操作 | 确认形式 |
| --- | --- |
| 禁用「管理面」插件 | **禁止**——Core 拒绝 `TogglePlugin`；UI 在开关提示与影响文案里明示「禁用即断连自锁」（无法经界面恢复），确需禁用只能改 `core.toml` 的 `disabled_plugins` 后重启 |
| 删除智能体 | 只剩一个时**拒绝**（warning toast「至少保留一个智能体，不能删除最后一个」）；否则确认框（danger，不可恢复） |
| 迁移运行区域 | 确认框（danger，列出三件副作用：目标区创建/更新该 agent、从当前区删除、**会话历史不随迁**） |
| 删除技能 | 确认框（danger，提示会删除 `SKILL.md` 文件） |
| 设为系统提示词技能 | 确认框（提示内容注入该 Agent 的 base 身份/规则层，影响所有启用它的智能体） |
| 移除技能 Git 来源 | 确认框（danger，明示「目录保留，可手动删除」） |
| 放弃未保存的技能 / 智能体编辑 | 确认框（danger；切换分类或选中项时触发，标题「放弃未保存的编辑」） |
| 归档 / 压缩历史 | 确认框（danger；压缩文案明示「作用于该人格全部会话」，归档明示「只清当前会话」） |
| 清理历史记忆 | **两步按钮**——首击变「确认清理？不可恢复」，再击才发命令 |
| 删除工作区会话 / 删除对话 | 确认框（danger；删对话会同时发 `ClearHistory` 清该对话的历史） |
| 实例动作按钮 | 确认框（danger）**当且仅当**插件描述符给该按钮声明了 `confirm` 文案——文案与动作都由描述符决定，Panel 无平台专属逻辑（见[实例管理·任务·Shell](./panel-instance-tasks.md)） |
| 删除 API Profile | **无确认**，立即下发 `DeleteApi` |
| 更新 Git 技能 | **无确认**，立即下发 `UpdateSkillFromGit` |

## 目录选择器

`DirectoryPicker` 是工作区面板「浏览…」打开的全屏模态（`Teleport to="body"` + `position: fixed; z-index: 60`），用来挑选**尚未声明的目录**（新建/编辑工作区会话的目录行）。它与右侧栏的文件浏览器是两套语义：文件浏览器只在**会话已声明的目录**内浏览，选择器必须能进到未声明的路径（否则鸡生蛋）。

- **位置切换**：下拉两类——「本机（当前区域）」与**在线**联邦节点（`state.federation.peers` 里 `link === 'Online'` 者）；切位置会清空导航栈并重拉根列表
- **请求/响应**：发 `RequestBrowseDirectories{path}`——本机传 `null`，联邦节点传 `node://<peer>`；结果落 `state.browseDirectories[scopedKey(activeRegion, path)]`（`roots` / `entries` / `error`），缓存上限 32 条
- **两级视图**：栈空 = **浏览根列表**（点根行 = 直接选该根，「›」= 进入浏览）；栈非空 = **子目录列表**（点行 = 选该子目录，「›」= 继续下钻）；面包屑逐级回退 + 「上一级」；底部「选择此目录」仅在栈非空（有当前目录）时可用
- **回填**：选中即 `emit('select', path)` → 工作区面板把路径**追加**到目录文本框（去重，保留手写 `node://` 路径的能力）后关闭
- **为何 Teleport**：它常挂在入口行 `PanelPopover` 里——浮层的 `backdrop-filter` 会把 `fixed` 后代的包含块改成自身、`overflow-y: auto` 再行裁剪，遮罩只会罩住 520px 的浮层且 `@click.self` 大面积失效；`teleport` 后 `fixed` / `inset` 才参照视口
- **权限边界**：浏览根由 Core 限定——**家目录 `~` 在首位**，其后是各本地工作区目录（去重；远程目录由对端各自给根）；进入目录时按根做 canonical 前缀校验（字面前缀 + `/`，避免 `/a/b` 放行 `/a/bc`）。联邦节点还需对端 `allow_queries` 放行 `browse_directories`——越界与未放行都表现为列表里的 `error` 文案（见[联邦](../federation/federation.md)「只读查询」）

## 桌面端浮层几何

| 浮层 | 几何 |
| --- | --- |
| 入口行弹层 | 宽 **520px**（`width` prop 缺省），视口不足时夹到 `100vw - 24px`；锚定按钮上方 `max(8, vh - rect.top + 8)`，水平按锚点中心居中并夹在 12px 边距内；内容区 `max-height: 68vh` |
| 会话区覆盖层 | 上/左/右 12px，底边 = 入口行底 + 30 + 12；见「会话区覆盖层」 |
| 目录选择器 | `min(560px, 92vw)` × `max-height: 72vh`，居中 |
| 分支查看器 | 库 `large`（`max-width: 720px`）× `max-height: 85vh`，居中 |
| 全局确认框 | 库 `small`（`max-width: 400px`）× `max-height: 85vh`，居中 |
