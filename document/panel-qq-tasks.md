---
id: panel-qq-tasks
title: "Panel QQ 管理 · 任务 · Shell"
group: 前端
x: 3200
y: 280
---

# Panel QQ 管理 · 任务 · Shell

> **定位**：本文覆盖适配器弹出层的 QQ 管理（扫码登录 / 门控 / 名单）、入口行任务弹层、Shell 列表与详情视图，以及 HTTP 旁路通道——卡片状态、取消约定（乐观中断 + `team_id` 命令）与通道超时。读者：维护 Panel QQ / 任务 / Shell 交互的开发者；相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)、[QQ 适配器门控](./adapter-qq-gating.md)。

## QQ 管理（适配器弹出层）

入口行「适配器」弹出的 QQ 面板。**所有编辑即点即生效**（无草稿、无保存按钮，页脚明示"点击名单条目即生效"）：

- **作用域 = 当前智能体**：面板只展示**归属当前智能体**（`persona` = team id）的实例——别的 agent 的实例状态、登录状况、容器名均不出现，也不会成为命令目标；当前智能体无实例时显示空态且不发任何请求（兼容旧 Core：无 `persona` 元数据时退化为展示无归属实例）
- **适配器行**：逐实例列出状态点、display name（`QQ（<persona> / <实例>）` 等实例化格式）、self_id、**NapCat 容器名**胶囊（`AdapterStatus.container`）与运行中/已停止 胶囊；启动/停止/重启按钮（`StartAdapter`/`StopAdapter`/`RestartAdapter`，断连禁用）
- **多实例**：实例列表来自 `AdapterList`（`persona` 元数据）；同一智能体多于一个实例时顶部出现「QQ 实例」分段选择，面板其余部分（登录/门控/名单/owner）都作用于所选实例；实例级命令与事件均带 `adapter` 字段（单实例时行为与旧版一致）
- **无实例零噪音**：实例列表为空（QQ 未启用）时面板**不发任何命令**（`refreshLists` 直接返回、登录区跳过请求）——此前打开面板会发 6 条必被 Core 拒绝的查询，堆出"QQ 适配器未找到"×5 + "QQ 实例 qq 未找到"×1
- **刷新列表**：逐实例拉取群/好友/门控配置/管理员/登录状态（挂载时与连接恢复时自动触发）；**逐查询登记加载跟踪**——按实例分 key（`qq:filter:<实例>` 等），名单/门控区上方 `LoadHint` 呈现 active 实例四个查询的组合阶段（转圈 / >6s 慢提示 / >20s 错误+重试），「刷新列表」按钮加载中转圈
- **扫码登录区（Core 代理）**：登录状态与二维码都走 WS——`RequestQqLoginStatus` → `QqLoginStatus` 事件；「获取登录二维码」→ `RequestQqQrcode` → `QqQrcode`（PNG base64，前端转 data URL）；**Panel 不再直连 OneBot HTTP / docker**（旧 `/api/qq/*` 已移除）。适配器运行中 + 离线 + 无二维码时**自动获取一次**（挂载即判定，`immediate`；`qrRequested` 单次闸）；获取失败显示错误行。Core 对陈旧码（>90s）自动向 NapCat 请求重新生成后再返回，因此手动「获取登录二维码」总能拿到接近新鲜的码
  - 连接状态经 `AdapterStateChanged` 实时推送（扫码成功后状态行自动转"已连接"）；二维码约 2 分钟有效；显示 `QQ 管理员：{owner}`（只读——`SetQqOwner` 协议命令未接 UI）与实例 WebUI 链接（`AdapterStatus.webui_url`，多实例各不同）
- **门控模式**：分段选择 无约束/白名单/黑名单（`SetQqGateMode{adapter}` 即选即生效），每模式附说明
- **2×2 名单卡**（白名单用户/黑名单用户/白名单群/黑名单群）：计数胶囊 + 已选 id 筹码（点击移除，黑名单染 error 色）+ 候选筹码（好友/群列表，点击切换归属，`UpdateQqAllowlist`/`UpdateQqDenylist` 带 `adapter` 提交）；**黑名单群无候选列表**（提示"群列表仅对白名单开放，黑名单群请在 Core 侧配置"）

## 任务弹层与 Shell

- **任务弹层**（替代独立任务视图）：入口行「任务」按钮弹出 `TasksPanel`（徽标 = 当前会话运行中/等待整合任务数），**只对应当前 agent 的当前会话**（按 `sessionId` 过滤；「全局」会话显示全部）；显隐 = `echo-agent.subagent` 插件对当前 persona 启用或当前会话已有任务记录，切换 Agent / 插件禁用后残留弹层强制关闭。任务卡片**倒序**（最新在前），kind 四类标签（后台/并行/Subagent/临时回复）+ 状态六态（运行中/已完成/失败/已取消/**等待整合 awaiting**/**已整合 integrated**）+ 目标 + 耗时（由 `createdAtMs` 现算，**当前运行期**：Core 侧后台/并行编排体系已移除，实际只有 Subagent 与临时回复两类产生；后台/并行标签与 awaiting/integrated 状态为保留适配）；`running` 或 `awaiting` 状态时耗时**每 1s** 跳动；分支状态与结果逐条列出；运行中任务可「取消」——与聊天区同一约定：先本地乐观中断，再发 `CancelRequestedWork{session_id, all:false, team_id}`
- **Shell**（列表与详情两层）：边栏「Shell」卡 = 会话列表（`ShellList`：状态点 + 会话 id + 目录 + 命令数 + 「详情」按钮，附新建/刷新；不内嵌终端）；点「详情」进入 **Shell 详情视图**（`ShellPanel`，经 `openShellDetail` 事件链切换视图，无顶栏导航入口）——仅目标会话的终端卡（运行中 spinner，完成/失败/超时 + 耗时（一位小数秒）+ 输出，终端区 max-height 320px）+ 命令输入（Enter 执行 `ShellExec`、Esc 清空）+ 返回列表/停止会话；新输出自动滚底。**列表加载三态**：边栏卡空态让位于加载跟踪（`shell:sessions`）——加载转圈 / 超时重试，确认无会话才显示「（当前 agent 暂无 shell 会话）」

## HTTP 通道

- WS 之外的接口（日志 `/api/logs/{panel,core}`、上游管理 `/api/upstreams`、媒体 `/media/<id>`）走 `fetchWithTimeout`/`panelFetch`，超时 10s（`api.ts:4`）；QQ 登录/二维码已全部迁到 WS（Core 代理）
