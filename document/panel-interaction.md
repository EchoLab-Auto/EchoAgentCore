---
group: 前端模块
x: 890
y: 727
---

# Panel 交互定义

Panel 前端全部交互行为的完备规范：视图、导航、输入、反馈、模态、异常与恢复。实现位置以 `web/src/` 相对路径标注；渲染基元来自 `@echolab-auto/ui-frame`（新拟态组件库），本文档只定义 Panel 自身的交互契约，库组件的通用行为（按钮、折叠卡、开关等）不展开。

**核心原则**：

1. **Core 是状态的唯一事实来源**——前端不持有业务真相，一切展示由 WS 事件流驱动，断连重连后全量重建而非增量对齐
2. **实时优先**——推理/工具/回答按事件到达顺序即时渲染，不缓冲等待（规范见 [Panel 前端](./panel.md)「时序与动画规范」）
3. **危险操作显式确认**——删除/清理走确认（两步确认或原生 confirm）；sudo 授权不可绕过
4. **编辑即生效**——QQ 门控、启停开关等操作无草稿态，点击立即下发命令

## 一、视图与导航

Panel 有五个主视图，仅经顶栏导航按钮切换；地址栏不承载视图状态（无路由）。

```prodoc-flow
graph LR
  Chat[会话视图] -->|顶栏: 任务| Tasks[任务视图]
  Chat -->|顶栏: 资源| Caps[资源视图]
  Chat -->|顶栏: 日志| Logs[日志视图]
  Chat -->|顶栏: Shell| Shell[Shell 视图]
  Tasks -->|顶栏: 会话| Chat
  Caps -->|顶栏: 会话| Chat
  Logs -->|顶栏: 会话| Chat
  Shell -->|顶栏: 会话| Chat
  Side[侧边栏点选会话] -->|强制回到| Chat
```

- 当前视图持久化于 `localStorage: echo-panel-view`（非法值回退 `chat`），刷新后恢复（`App.vue:37-42, 94-101`）
- 主区同一时间只渲染一个视图组件（`App.vue:330-343`）
- 点选侧边栏会话项**强制切回会话视图**（`App.vue:154-157`）
- 任务按钮在有运行中任务时显示计数徽标 `任务 (N)`（`App.vue:278`）

## 二、应用外壳

### 2.1 布局区域

`NeumorphismLayout`（`App.vue:231-241`）：

| 区域 | 内容 |
|---|---|
| 顶栏左 | 品牌 "EchoAgent Panel" |
| 顶栏中 | 连接状态点 + 模型名 + 忙碌徽标 |
| 顶栏右 | 五视图导航、设置下拉、主题开关 |
| 侧边栏（264px，可折叠为 0） | 临时分支卡 + 会话卡 |
| 主区 | 当前视图组件 |

### 2.2 顶栏交互

- **连接指示**：`已连接`（online 绿点）/ `连接中…`（connecting 呼吸点），驱动源 `state.connected`
- **模型显示**：`state.model`（无数据时 `—`）；任一会话处于 thinking/tool/subagent 相位时追加 `忙碌 ×N` 警告标签（`App.vue:143-148`）
- **设置下拉**：点击触发，单菜单项「API 设置」→ 打开 SettingsModal（`App.vue:304-314`）
- **主题开关**：三态循环（浅色 → 跟随系统 → 深色），持久化 `localStorage: echo-panel-theme`；外壳默认 `auto`，index.html 内联脚本防闪烁

### 2.3 持久化的界面状态

| localStorage 键 | 内容 | 缺省 |
|---|---|---|
| `echo-panel-view` | 当前视图 | `chat` |
| `echo-panel-active-team` | 当前 Agent（刷新/重连后停留原 Agent） | 主 Agent |
| `echo-panel-sidebar-collapsed` | 侧边栏折叠 `'1'/'0'` | 展开 |
| `echo-panel-theme` | 主题三态 | `auto` |

所有读写包 try/catch（隐私模式降级为不持久化）。侧边栏卡片自身的展开/折叠**不持久化**（组件内状态，默认双卡展开）。

## 三、连接生命周期

```prodoc-flow
graph LR
  Closed[未连接] -->|ws onopen| Open[已连接: Bootstrap]
  Open -->|onclose| Toast[toast: 与后端断开…]
  Toast -->|500ms| Retry[重连尝试]
  Retry -->|失败: 退避×2 上限30s| Retry
  Retry -->|成功| Rebuild[清空运行期+时间线状态]
  Rebuild --> Open
```

- WS 地址 `ws(s)://{host}/ws`（随页面协议）；单例连接（`connection.ts`）
- **Bootstrap 命令组**（每次 onopen 按序发送）：`RequestState` → `RequestTrunkTimeline{team_id: 上次Agent}` → `RequestAdapterStatus` → `RequestQqFilterConfig` → `RequestTeamsList` → `RequestShellSessions`
- **重连清空**：`branchTabs`、`tasks`、`activities` 以及时间线状态（`trunk`、`teamTimelines`、`trunkTeamId`）全部清空后全量重建——断连期间错过的实时事件与就地更新无法对齐，全量是唯一安全恢复路径
- **退避**：500ms 起、×2 递增、上限 30s；成功连接后复位 500ms
- **断连期间**：顶栏显示"连接中…"；输入框禁用（placeholder `未连接到 Core，暂时无法发送`）；`sendCommand` 不发送并弹 error toast；QQ 面板按钮禁用
- **兜底对齐**：`AgentCompleted` 时若该会话最后一条不是正式回答，自动补拉 `RequestTrunkTimeline`（`state.ts:594-608`）
- 协议信封 `{type: command|event|sudo_password, payload}`；无法解析的帧静默丢弃

## 四、侧边栏

### 4.1 临时分支卡

- 仅当当前 Agent 启用回执分支能力时显示（`reply_branches_enabled !== false`，`PanelSidebar.vue:18-30`）；徽标显示运行中分支数
- 分支行：脉冲 spinner（`branch-pulse 1s`）+ 任务摘要（截断 24 字符）；无关闭按钮——`ReplyBranchCompleted` 或重连后自动消失
- 点击分支行 → 打开 BranchModal（分支详情）

### 4.2 会话卡与分组

仅当当前 Agent 启用会话能力时显示。分组规则（按会话 id `platform:scope:…` 解析）：

| 分组 | 内容 | 备注 |
|---|---|---|
| 全局 | 合成的「全部消息」项（预览"共享同一 trunk 上下文"） | 仅当 Agent 启用全局会话；选中后发消息会重定向到本地会话 |
| Local | 本地会话（`local:tui::local_user`） | 启动默认选中 |
| QQ 私聊 / QQ 群 / 其他 | 按平台归组 | 空分组隐藏 |

- **排序**：组内按 `last_active` 倒序（活跃上浮）
- **归属过滤**：只显示当前 Agent 的会话（`team_id` 归一化匹配；TeamsList 未到达时他属会话不放行，避免闪现）
- **忙碌点**：会话有未完成活动时显示主色 `●`（`state.activities[id].phase ≠ completed`）
- **预览行**：`session.last_message`
- 点选 = 切换会话过滤（不重拉时间线）+ 强制回会话视图

## 五、Agent 切换器

锚定在会话视图入口行左侧的玻璃胶囊（头像 + 名称 + 箭头，箭头开启时旋转 180°）：

- 菜单**向上弹出**（`bottom: calc(100% + 8px)`，最大高度 `min(60vh, 100vh-200px)`）；点击外部或选中后关闭
- 头像 = 名称首字符，颜色按名称哈希取 7 色板；禁用成员灰显 + online/offline 状态点
- **切换效果**（`App.vue:onTeamSelect`）：持久化选择 → 清空分支标签 → 立即用本地缓存渲染该 Agent 时间线 → 后台 `RequestTrunkTimeline{since_seq}` 增量对齐（无缓存则全量）→ 重选本地会话
- 与"点选会话"的本质区别：切换 Agent = 切换记忆空间（重载时间线）；点选会话 = 同一记忆空间内的过滤
- 当前 Agent 被删除时：回退主 Agent、重载时间线、重选本地会话（`App.vue:191-212`）；全局会话被禁用时从「全部消息」回退本地会话（`App.vue:180-188`）

## 六、会话视图（聊天）

### 6.1 消息列表

- **自动滚动**：仅当用户处于底部 120px 阈值内时跟随新内容；上翻后出现「回到底部 ↓」按钮（平滑滚动，500ms 后解除锁定）。滚动区内 padding-bottom 190px、按钮抬升至 bottom 180px——为悬浮玻璃输入区让位
- **入场动画**：0.28s 淡入 + 上移 6px，仅实时消息（`animate: true`）播放；历史回放不播
- **容量**：主时间线上限 1024 条，溢出裁最旧
- **复制**：消息气泡与工具输出均有复制按钮（库内置）；无任何右键菜单
- **Markdown**：仅 Agent 消息渲染 Markdown
- **时间格式**：zh-CN 24 小时制

### 6.2 消息角色与渲染

| 角色 | 渲染 |
|---|---|
| `user` | 用户气泡（含来源元数据：平台/用户/群） |
| `backend` | Agent 气泡（Markdown） |
| `reasoning` | **Panel 扩展角色**：`#message` slot 拦截渲染 ReasoningBlock（库的 ChatRole 无此角色，不拦截会被误渲染为 Agent 气泡） |
| `tool` | ChatToolCallBlock 折叠卡 |
| `system` | 系统提示行 |
| `branch` | ChatBranchMergeBlock 分支合并卡（当前 reducer 已不产生——分支内容实时进主时间线，此角色保留适配） |

### 6.3 工具卡生命周期

```prodoc-flow
graph LR
  Call[ToolCall 事件] -->|按 tool_call_id 建档| Running[running: spinner]
  Running -->|ToolResult 同 id 配对| Done{succeeded / failed}
  Running -->|旧 core 无 id: 按名回填最新 running| Done
  Done -->|timed_out 或 error: 前缀| Failed[failed: error 标签]
  Done -->|否则| Ok[succeeded: success 标签]
```

- 输入显示为 `key=value` 摘要（单值截断 120 字符）；输出上限 4000 字符；输入+输出非空才可展开
- 重连/恢复后仍 `output==null` 的条目标记为「已中断」（core 侧重启清理），不再永远 running

### 6.4 推理块（ReasoningBlock）

- **实时**：打字机逐字——24ms/tick，每 tick ≥2 字符，总时长约 6s 封顶（按文本长度自适应提速）；紫色左边框 + 「思考」标签 + 输入中提示「正在推演…」+ 闪烁光标 + 呼吸点
- **历史**：一次性全量显示，无动画
- 动画纯视觉层（`animate` 元数据不参与逻辑）

### 6.5 活动浮条

输入区上方的胶囊浮条（仅当前会话忙碌时可见）：旋转 spinner（0.8s）+ 相位文案——`正在思考：{detail}…` / `正在调用工具 {detail}…` / `子代理工作中…`（320px 省略截断）。

### 6.6 输入区

- **发送**：Enter（IME 组合输入守卫，`isComposing`/229 不触发）；Shift+Enter 换行；空文本（trim 后）不发送；发送后清空
- **文本域**：1 行起自适应撑高，上限 `calc(8em + 20px)` 后内部滚动
- **断连禁用**：`disabled` + placeholder `未连接到 Core，暂时无法发送`
- **图片**：粘贴板拦截（仅 `image/*`）或附件按钮多选；最长边 1600px、JPEG q0.85、PNG 直通转 data URL；待发附件 64×64 缩略图 + `×` 移除；随 `SendMessage.images` 发送，发后清空
- **取消任务**：仅当前会话忙碌时出现在操作区 → `CancelRequestedWork{session_id, all:false}`
- 发送按钮为库 `#actions` slot 自绘（替换默认按钮，保持与输入区新拟态风格一致）

### 6.7 入口行按钮与弹出层

入口行位于输入区上方（`bottom = 输入区高度 + 24px`，ResizeObserver 跟踪）：

| 按钮 | 行为 |
|---|---|
| Agent 切换 | 见 §五 |
| 配置 | 打开 AgentConfigModal（当前 Agent 的能力配置） |
| 上下文 | 打开 ContextView 覆盖层（trunk 上下文明细） |
| 清单 | 弹出清单卡（徽标 = 清单数）：各清单进度条 + ☑/☐ 项，只读 |
| 适配器 | 弹出 QQ 管理面板（§九）；仅当前 Agent 启用适配器插件时显示；切换 Agent 强制关闭 |

弹出层宽 `min(520px, 82vw)`，锚定按钮上方 8px、相对输入区水平居中；点遮罩空白关闭。

## 七、模态与覆盖层

### 7.1 SudoModal（最高优先级）

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

### 7.2 SettingsModal（API 设置）

顶栏设置下拉进入，大尺寸、不可点遮罩关闭。内容仅限 API 配置（系统提示词编辑在 AgentConfigModal/资源页）：

- **快捷模板**（5 个：DeepSeek-Anthropic / DeepSeek-OpenAI / OpenAI / Anthropic / Ollama）：点击填充 provider/base_url/model/思考/推理
- **表单**：Provider（未知值保留为额外选项）、Model、Base URL、API Key（密码框；标注 已设置/未设置，留空保持不变）、思考模式、推理强度
- **测试连接**：`TestApi`（默认或指定 profile）；加载态互斥；结果显示 ✓/✗ + 消息 + 延迟；每次打开清空上次结果
- **Profile 管理**：每行显示 激活/编辑中 标签与 `provider / model · key` 摘要；操作 测试 / 编辑 / 切换（`SwitchApi`）/ 删除（`DeleteApi`，**无确认，立即生效**）；新建 = 命名 + 保存为新 profile
- 打开或 `state.api` 更新时表单重置为当前配置

### 7.3 BranchModal（分支详情）

侧边栏分支行点击进入：头部 `临时分支 · {id前8位}` + 状态标签（运行中/已完成/已取消/失败）；正文 = 任务/目标/会话元信息 + 分支内消息流（工具卡/推理块/内容，新消息自动滚底）；分支已结束时显示「分支已结束并合并到主会话」。

### 7.4 ContextView（上下文覆盖层）

入口行「上下文」进入，会话视图内磨砂全覆盖；点遮罩或「返回聊天」关闭。打开时 `RequestContext`：

- token 仪表盘：`NeumorphismProgress`（≥90% error、≥70% warning），总量/提示词/历史/上限四项统计
- 上下文块分组折叠（系统提示词各块 + 历史块）：kind 色签 + token 数 + 相对占比条
- 逐条消息明细（角色 + token + 内容）
- 操作：**清理历史**（两步确认：首击变为「确认清理？不可恢复」，再击发送 `ClearHistory`）；**归档**（confirm → `ArchiveHistory`）；**压缩**（confirm → `CompactHistory{keep_recent: 40}`）；**重载技能**；**刷新**

### 7.5 AgentConfigModal（当前 Agent 配置）

锚定会话区上方的磨砂覆盖层（底边 = 入口行高 + 42px）；点遮罩/✕/取消/保存后关闭。打开时拉取技能/工具/插件清单：

- 字段：名称、描述、系统提示词（留空继承全局）、启用开关（**禁用即卸载记忆**，重启用重新挂载）
- 三类能力复选表（插件按 kind 分组、工具/技能按包分组，组级全选）：**白名单语义——空表 = 全部启用**；首次取消勾选时先把全量写入列表再移除该项
- 保存 → `SaveTeam`

### 7.6 原生确认与受保护操作

| 操作 | 确认形式 |
|---|---|
| 删除技能 | `window.confirm`（提示会删除 SKILL.md 文件） |
| 删除 Team | 主 Agent `window.alert` 禁止；其余 `window.confirm`（不可恢复） |
| 归档/压缩历史 | `window.confirm` |
| 清理历史 | 两步按钮确认（见 §7.4） |

## 八、资源视图（CapabilitiesPanel）

**双栏布局**：左侧分组菜单（默认展开 技能/工具 两组），右侧详情/编辑器；底部 新建技能 / 新建 Team / 刷新。打开与刷新时拉取四类清单（Skills/Tools/Plugins/Teams）；条目消失时选中态自动置空。

| 对象 | 详情内容 | 操作 |
|---|---|---|
| 技能 | 分类/常驻标签、描述、触发词、SKILL.md 原文（≤420px 滚动） | 启停开关（`ToggleSkill`）、编辑（名称锁定）、删除（confirm） |
| 工具 | 分类、描述、JSON 参数 schema | 启停开关（`ToggleTool`，禁用后模型不可见） |
| 插件 | kind/版本/外部标签、描述、ID/Entry/Author | 启停开关（`TogglePlugin`，禁用即卸载注册；全局生效） |
| Team | 主 Agent/N 会话标签、提示词、能力白名单 | 选中即进编辑态；启停（`ToggleTeam`）；非主 Agent 可删除 |

- **技能编辑器**：名称（编辑时锁定）、描述、触发词（逗号分隔）、分类、常驻开关、Markdown 正文（12 行自适应）；保存 `SaveSkill`
- **Team 编辑器**：ID 必填 + `/^[A-Za-z0-9_-]+$/`（编辑时锁定）、名称必填；启用包（Package 级主控，一次切换整包工具+技能）、启用插件/工具/技能复选组；粘性页脚 删除/放弃更改/保存（`SaveTeam`）
- 禁用项在菜单中显示 `（已禁用）` 后缀；`builtin` 类工具显示为「内置工具」

## 九、QQ 管理（适配器弹出层）

入口行「适配器」弹出的 QQ 面板。**所有编辑即点即生效**（无草稿、无保存按钮，页脚明示"点击名单条目即生效"）：

- **适配器行**：状态点、display name、self_id、运行中/已停止 胶囊；启动/停止/重启按钮（`StartAdapter`/`StopAdapter`/`RestartAdapter`），断连禁用
- **刷新列表**：拉取群/好友/门控配置/管理员（挂载时与连接恢复时自动触发）
- **扫码登录区**：
  - 每 **5s** 轮询 `/api/qq/login-status`；在线即清除二维码
  - 「获取登录二维码」→ `/api/qq/qrcode`（blob 转 object URL，旧的 revoke）；适配器未连接 + 离线 + 无二维码时**自动获取一次**
  - 二维码约 2 分钟有效；状态行区分 已连接/在线/离线（含适配器状态提示）；显示 `QQ 管理员：{owner}`（只读——`SetQqOwner` 协议命令未接 UI）与 NapCat WebUI 链接 `:6099`
- **门控模式**：分段选择 无约束/白名单/黑名单（`SetQqGateMode` 即选即生效），每模式附说明
- **2×2 名单卡**（白名单用户/黑名单用户/白名单群/黑名单群）：计数胶囊 + 已选 id 筹码（点击移除，黑名单染 error 色）+ 候选筹码（好友/群列表，点击切换归属，`UpdateQqAllowlist`/`UpdateQqDenylist` 两类 id 一并提交）；**黑名单群无候选列表**（提示"群列表仅对白名单开放，黑名单群请在 Core 侧配置"）

## 十、任务 / 日志 / Shell 视图

- **任务视图**：任务卡片（kind/状态标签 + 目标 + 创建时间 + 耗时）；有运行中任务时耗时**每 1s** 跳动；分支状态与结果逐条列出；运行中任务可「取消」（`CancelRequestedWork`）
- **日志视图**：Core/Panel 分段切换（默认 Core），拉取 `/api/logs/{core,panel}?lines=100` 原文显示；自动刷新默认开（**5s** 间隔），可切手动；HTTP 错误显示状态码
- **Shell 视图**：新建会话（工作目录 + Enter）；每会话一张终端卡（运行中 spinner，完成/失败/超时 + 耗时 + 输出）；命令输入 Enter 执行（`ShellExec`）、Esc 清空；新输出自动滚底；可停止会话
- **HTTP 通道**：WS 之外的仅有接口（QQ 状态/二维码、日志）走 `fetchWithTimeout`，超时 10s（`api.ts`）

## 十一、Toast 系统

- 三类：`info / success / error`；位置右上；单条 **6s** 自动消失；同屏上限 8 条（溢出挤掉最旧）；队列 cap 8、文本截断 512 字符
- `state.toasts` 仅作转发队列：watcher 逐条泵入组件库 ToastProvider 后清空
- 来源：断连重连提示、断连时发送命令、SudoResolved、Core `Error` 事件

## 十二、键盘清单

Panel 无全局快捷键系统；所有键处理局部于组件：

| 键 | 位置 | 行为 |
|---|---|---|
| Enter | 输入区 | 发送消息（IME 组合中不触发） |
| Shift+Enter | 输入区 | 换行 |
| Enter | SudoModal | 授权（提交密码） |
| Esc | SudoModal | 拒绝（提交 null） |
| Enter | Shell 命令行 | 执行命令 |
| Esc | Shell 命令行 | 清空输入 |

## 十三、设计边界（协议已定义但 UI 未接线）

以下 `BackendCommand` 在协议层存在，但当前 UI 无任何入口——属有意留白而非缺陷，新增入口时按本文档规范补充：

- `SwitchModel` / `SwitchProvider` / `SetSystemPrompt`（模型与提示词经 API 设置/AgentConfig 覆盖）
- `StartAllAdapters` / `StopAllAdapters`（逐个适配器控制已覆盖）
- `SetQqOwner`（管理员显示为只读）

## 十四、关键常量速查

| 常量 | 值 | 位置 |
|---|---|---|
| WS 重连退避 | 500ms ×2，上限 30s | connection.ts |
| Toast | 6s；队列/同屏 8；≤512 字符 | App.vue / state.ts |
| 主时间线容量 | 1024 条 | timeline.ts |
| 推理打字机 | 24ms/tick，约 6s 封顶，≥2 字符/tick | ReasoningBlock.vue |
| 消息入场动画 | 0.28s（淡入 + 上移 6px） | ChatView.vue |
| 滚动跟随阈值 / 让位 | 120px；按钮 180px / 内边距 190px | main.ts / ChatView.vue |
| 图片附件 | 最长边 1600px，JPEG q0.85 | ChatView.vue |
| 工具输出上限 | 4000 字符 | state.ts |
| 输入区最大高度 | `calc(8em + 20px)` | ChatView.vue |
| 轮询：QQ 状态 / 日志 / 任务耗时 | 5s / 5s / 1s | QqLoginSection / LogView / TasksPanel |
| HTTP 超时 | 10s | api.ts |
