---
id: panel-adapter-tasks
title: "Panel 适配器管理 · 任务 · Shell"
group: 前端
x: 3200
y: 780
---

# Panel 适配器管理 · 任务 · Shell

> **定位**：本文覆盖三块与平台无关的交互——**适配器弹出层**（描述符驱动的设备管理界面）、入口行任务弹层、Shell 列表与详情视图，以及 HTTP 旁路通道（卡片状态、取消约定 = 乐观中断 + `team_id` 命令、通道超时）。读者：维护 Panel 适配器 / 任务 / Shell 交互的开发者。
> Panel 不认识任何具体平台：弹层的布局由插件下发的描述符（`AdapterStatus.ui`）声明、动作经 `InvokeAdapterOp` 透传、状态经 `AdapterState` 回推。当前在册的适配器插件只有 `echo-agent.adapter.qq`——它是**进程外插件**：manifest 编译在 Core 里，实现却是独立进程（独立仓库 `echo-adapter-qq`），Core 侧只有门控壳（插件体系的两维度见 [插件化设计](../plugins/core-plugins.md)）。下文举的扫码 / 门控 / 名单都是**该插件描述符的内容**，不是 Panel 的能力边界，也不是 Core 的内置功能。相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)、[插件开发指南](../plugins/plugin-authoring.md)。

## 适配器管理（描述符驱动的弹出层）

入口行「适配器」按钮弹出的设备管理面板——**面板本身不含平台专属逻辑**，新增平台只需插件侧提供描述符，Panel 零改动。**所有编辑即点即生效**（无草稿、无保存按钮，页脚明示"点击名单条目即生效"）：

- **作用域 = 当前智能体**：面板只展示**归属当前智能体**（`persona` = team id）的实例——别的 agent 的实例状态、登录状况、容器名均不出现，也不会成为命令目标；当前智能体无实例时显示空态且不发任何请求
- **适配器行**：逐实例列出状态点、display name、self_id、**元数据胶囊**（`AdapterStatus.metadata[]`，如容器名 / WebUI 地址）与运行中/已停止 胶囊；启动/停止/重启按钮（`StartAdapter`/`StopAdapter`/`RestartAdapter`，断连禁用）。实例列表与状态行由通用 `AdapterPanel`（渲染器外层）提供
- **多实例**：实例列表来自 `AdapterList`（`persona` 元数据）；同一智能体多于一个实例时顶部出现实例选择，面板其余部分（登录/门控/名单/owner）都作用于所选实例；实例级命令经 `InvokeAdapterOp.adapter`（必填实例名）寻址
- **无实例零噪音**：实例列表为空（当前 Agent 未启用任何适配器插件）时面板**不发任何命令**（`AdapterPanel` 的挂载刷新守卫：无可见实例时不发任何请求）——此前打开面板会发 6 条必被 Core 拒绝的查询，堆出一串「适配器实例 … 未找到」式错误提示
- **刷新**：管理读命令经 `InvokeAdapterOp{method:get_filter_config / get_groups / get_friends / get_owner / login_status}` 逐实例拉取；等待态按 `adapter-op:<scopedKey>:<method>` 键登记；状态经 `AdapterState` 回推落定（名单/门控区显示加载与错误态）
- **扫码登录区（Core 代理）**：登录状态读状态文档（`AdapterState.state.online` / `self_id`）；「获取/刷新二维码」经 `InvokeAdapterOp{method:"login_qrcode"}`（PNG base64 经 `AdapterOpResult` 返回，只存渲染器 image 节点局部，多实例不串号）；**Panel 不直连 OneBot HTTP / docker**（旧 `/api/qq/*` 已移除）。动作按钮由插件描述符 image 节点声明（`clear_when: online` 登录后自动清图）；Core 对陈旧码（>90s）自动向 NapCat 请求重新生成后再返回
  - 连接状态经 `AdapterStateChanged` 实时推送（扫码成功后状态行自动转"已连接"）；二维码约 2 分钟有效；「管理员」与 WebUI 链接由描述符节点读状态文档（`owner` / `instance.webui_url`）呈现（多实例各不同）
- **门控模式**：选择器 无约束/白名单/黑名单（描述符 `select` 节点，动作 `InvokeAdapterOp{method:"set_gate_mode"}` 即选即生效），每模式附说明（options `hint`）
- **2×2 名单卡**（白名单用户/黑名单用户/白名单群/黑名单群）：描述符 `list_editor` 节点；增删经 `InvokeAdapterOp{method:"update_allowlist"/"update_denylist"}` 提交（部分载荷：只带变更的表，缺键 = 保留现值）；提交结果经 `AdapterState` 回推落定

## 任务弹层与 Shell

- **任务弹层**（替代独立任务视图）：入口行「任务」按钮弹出 `TasksPanel`（徽标 = 当前会话运行中/等待整合任务数），**只对应当前 agent 的当前会话**（按 `sessionId` 过滤；「全局」会话显示全部）；显隐 = `echo-agent.subagent` 插件对当前 persona 启用或当前会话已有任务记录，切换 Agent / 插件禁用后残留弹层强制关闭。任务卡片**倒序**（最新在前），kind 两类标签（Subagent/临时回复）+ 状态四态（运行中/已完成/失败/已取消）+ 目标 + 耗时（由 `createdAtMs` 现算）；`running` 状态时耗时**每 1s** 跳动；分支状态与结果逐条列出；运行中任务可「取消」——与聊天区同一约定：先本地乐观中断，再发 `CancelRequestedWork{session_id, all:false, team_id}`
- **Shell**（列表与详情两层）：边栏「Shell」卡 = 会话列表（`ShellList`：状态点 + 会话 id + 目录 + 命令数 + 「详情」按钮，附新建/刷新；不内嵌终端）；点「详情」进入 **Shell 详情视图**（`ShellPanel`，经 `openShellDetail` 事件链切换视图，无顶栏导航入口）——仅目标会话的终端卡（运行中 spinner，完成/失败/超时 + 耗时（一位小数秒）+ 输出，终端区 max-height 320px）+ 命令输入（Enter 执行 `ShellExec`、Esc 清空）+ 返回列表/停止会话；新输出自动滚底。**列表加载三态**：边栏卡空态让位于加载跟踪（`shell:sessions`）——加载转圈 / 超时重试，确认无会话才显示「（当前 agent 暂无 shell 会话）」

## HTTP 通道

- WS 之外的接口（日志 `/api/logs/{panel,core}`、上游管理 `/api/upstreams`、媒体 `/media/<id>`）走 `fetchWithTimeout`/`panelFetch`，超时由 `api.ts` 的 `FETCH_TIMEOUT_MS` 决定（10s）；适配器的登录 / 二维码等平台动作已全部迁到 WS（经 Core 代理到插件）
