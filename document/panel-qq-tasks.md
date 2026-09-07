---
id: panel-qq-tasks
title: "Panel QQ 管理 · 任务 · Shell"
group: 前端模块
link: ["panel-interaction | Panel 布局 & 交互定义"]
x: 616
y: 48
---

# Panel QQ 管理 · 任务 · Shell

适配器弹出层的 QQ 管理（扫码登录/门控/名单）与任务/Shell 视图的交互契约：卡片状态、取消约定（乐观中断 + team_id 命令）、HTTP 旁路通道。

## 十、QQ 管理（适配器弹出层）

入口行「适配器」弹出的 QQ 面板。**所有编辑即点即生效**（无草稿、无保存按钮，页脚明示"点击名单条目即生效"）：

- **适配器行**：状态点、display name、self_id、运行中/已停止 胶囊；启动/停止/重启按钮（`StartAdapter`/`StopAdapter`/`RestartAdapter`），断连禁用
- **刷新列表**：拉取群/好友/门控配置/管理员（挂载时与连接恢复时自动触发）
- **扫码登录区**：
  - 每 **5s** 轮询 `/api/qq/login-status`；在线即清除二维码；接口非 2xx 直接视为离线；状态未知（null）时显示「正在检查登录状态…」
  - 「获取登录二维码」→ `/api/qq/qrcode`（blob 转 object URL，旧的 revoke）；适配器未连接 + 离线 + 无二维码时**自动获取一次**（`qrRequested` 单次闸，不会重复自动拉取）；获取失败显示错误行
  - 二维码约 2 分钟有效；状态行区分 已连接/在线/离线（含适配器状态提示），已连接时追加昵称与 user_id；显示 `QQ 管理员：{owner}`（只读——`SetQqOwner` 协议命令未接 UI）与 NapCat WebUI 链接 `:6099`
- **门控模式**：分段选择 无约束/白名单/黑名单（`SetQqGateMode` 即选即生效），每模式附说明
- **2×2 名单卡**（白名单用户/黑名单用户/白名单群/黑名单群）：计数胶囊 + 已选 id 筹码（点击移除，黑名单染 error 色）+ 候选筹码（好友/群列表，点击切换归属，`UpdateQqAllowlist`/`UpdateQqDenylist` 两类 id 一并提交）；**黑名单群无候选列表**（提示"群列表仅对白名单开放，黑名单群请在 Core 侧配置"）

## 十一、任务 / Shell 视图

- **任务视图**：任务卡片**倒序**（最新在前），kind 四类标签（后台/并行/Subagent/临时回复）+ 状态六态（运行中/已完成/失败/已取消/**等待整合 awaiting**/**已整合 integrated**）+ 目标 + 创建时间 + 耗时；`running` 或 `awaiting` 状态时耗时**每 1s** 跳动；分支状态与结果逐条列出；运行中任务可「取消」——与聊天区同一约定：先本地乐观中断，再发 `CancelRequestedWork{session_id, all:false, team_id}`
- **Shell 视图**：新建会话（工作目录 + Enter）；每会话一张终端卡（卡头 = 创建时间 + 命令计数；运行中 spinner，完成/失败/超时 + 耗时（一位小数秒）+ 输出，终端区 max-height 320px）；命令输入 Enter 执行（`ShellExec`）、Esc 清空；新输出自动滚底；可停止会话；「刷新」按钮重拉会话列表
- **HTTP 通道**：WS 之外的仅有接口（QQ 状态/二维码、日志）走 `fetchWithTimeout`，超时 10s（`api.ts:4`）

