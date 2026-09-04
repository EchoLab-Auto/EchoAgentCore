---
group: 前端模块
x: 606
y: 727
link: ["panel-interaction | 交互定义"]
---

# Panel 前端

Panel（EchoAgentPanel）是 Web 管理面板：Rust 后端（axum）托管 Vue 3 + TypeScript 前端（`@echolab-auto/ui-frame` 新拟态组件库），并把浏览器 WebSocket 中继到 Core 的 management WS。默认 `:8080` 提供服务，Core 连不上时页面照常加载并显示"连接中…"（指数退避自动重连）。

## 仓库布局

```text
EchoAgentPanel/
├── config/echo-agent-panel.toml     # 面板配置模板
├── source/echo-web-server/          # Rust 后端（axum + WS 中继）
│   ├── src/main.rs                  # 服务入口（静态托管 + /ws 路由）
│   ├── src/proxy.rs                 # 浏览器 ↔ Core 帧中继（不解析负载）
│   └── src/config.rs                # 面板配置加载
├── web/                             # Vue 3 + TypeScript + Vite 前端
│   └── src/
│       ├── protocol.ts              # echo-protocol 线格式的 TS 镜像
│       ├── state.ts                 # 状态 + reducer
│       ├── state_domains/           # timeline / orchestration / helpers
│       ├── connection.ts            # WS 连接（重连退避 + Bootstrap）
│       └── components/              # 聊天 / 设置 / sudo / QQ / 任务 / 清单
└── scripts/                         # install.sh / update.sh / uninstall.sh
```

## 后端：无状态字节级中继

每个浏览器 WS 连接对应一条到 Core management WS 的专用连接；帧按原文转发，**不解析、不记录负载**——sudo 密码帧因此与日志、命令队列完全隔离。协议演进只需同步 core 的 `echo-protocol` 与 `web/src/protocol.ts`，后端零改动。中继对上下行均做 30s Ping 心跳并透传 Ping/Pong；**半死收割**：一侧超过 90s（3 个心跳周期）无任何帧即断开整条链路，设备休眠留下的僵尸连接不会悬挂累积。

- 中继极薄（一个 crate、约 200 行），可独立测试（`tests/proxy.rs` 用假 Core 验证双向帧透传，含 sudo 帧）
- 多标签页 = 多条 Core 连接（Core 的 management 支持多 Panel）
- 已知限制：无会话恢复——刷新页面后从 `RequestState` / `RequestTrunkTimeline` 重新 Bootstrap
- 否决的备选：后端做协议层转发/会话管理（等于重写 Core 桥接层且 sudo 帧需额外安全处理）；浏览器直连 Core :3132（跨域 + 暴露 management 端口）

## 状态管理与数据流

- `store.ts`：全局响应式单例（Vue `reactive`），`dispatch(event)` 逐事件归约
- `state.ts`：reducer 按事件类型分派，原地深变异
- `state_domains/`：timeline（时间线转换/增量/工具配对）、orchestration（分支/任务/活动）、helpers
- 连接管理 `connection.ts`：WS 自动重连；重连后清空运行期状态与时间线缓存，全量重建
- 实时事件按 `team_id` 归一化过滤后才进主时间线（跨 agent 不串显）；`TrunkTimeline` 按 `full` 标志区分全量替换/增量追加

## 主视图（聊天）

- `ChatView.vue`：消息列表 + 吸底输入区；`#message` slot 拦截扩展角色渲染
- `ReasoningBlock.vue`：推理打字机动画（实时消息 6 秒封顶；历史回放不播）
- 活动浮条：思考中 / 调用工具 / 子代理 的 spinner + 动态文案
- 消息入场动画 0.28s 淡入上移，仅实时消息（`animate` 标记）播放

## 侧边栏与设置页

- `PanelSidebar.vue`：临时分支卡 + 会话卡（按当前 agent 能力开关显示/隐藏）
- `SessionGroups.vue`：按平台分组（全局/Local/QQ 私聊/QQ 群/其他），组内按最近活跃排序，按 agent 过滤
- `SettingsView.vue`：设置视图——API 设置（ApiSettings.vue）+ 技能/工具/插件/智能体的浏览、启停、编辑、删除（左侧一级菜单 + 右侧工作区；2026-09-04 起取代原资源视图与 API 弹窗）
- `AgentSwitcher.vue`：输入框上方 Agent 切换悬浮卡片

## 时序与动画规范

主时间线对 Agent 运行过程的展示规范（一致性基线，改动需保持三条规则）：

**1. 及时性（实时渲染，不缓冲）**：`AgentReasoning` / `ToolCall` / `ToolResult` / `AgentOutput` 事件到达即渲染进主时间线，**不得**等回复完成后一次性写入；推理是独立 `reasoning` 角色消息按序插入，不缓冲到回复尾部。

**2. 时序表现（还原真实顺序）**：主时间线顺序 = 事件到达顺序（`用户消息 → 推理 → 工具调用 → 工具结果 → … → 正式回答`），推理真实穿插在工具调用之间，正式回答始终最后；历史回放（`loadTimeline`）同样把 `backend.reasoning` 拆分为独立 reasoning 消息、插在回答之前，与实时路径时序一致；侧边栏临时分支详情继承同一规则（仅做连续 reasoning 的合并展示）。

**3. 动画反馈（对应位置对应动画）**：

| 阶段 | 位置 | 动画 |
| --- | --- | --- |
| 连接中 | 顶栏状态点 | 状态点呼吸 |
| 思考 / 调用工具 / 子代理 | 输入框上方活动浮条 | 旋转 spinner + 动态文案 |
| 推理输出 | 推理块 | 打字机逐字显示（自适应速度，约 6s 封顶）+ 光标/呼吸点 |
| 工具执行中 | 工具卡 | running 状态 spinner |
| 消息到达 | 每条实时消息 | 0.28s 淡入上移入场动画 |

- 打字机/入场动画**仅**对实时消息（`DisplayMessage.animate === true`）播放；历史回放、刷新加载不播动画
- 动画为纯视觉层：`animate` 是展示元数据，不进入任何数据/逻辑判断
- 实现要点：ui-frame `ChatRole` 不含 `reasoning`，必须在 `ChatView` 的 `#message` slot 层拦截（否则未知角色会被渲染成 Agent 气泡）；`pendingReasoning`/`completedReasoning` 保留给分支合并块消费，主时间线不再读取

## 主题

- `ThemeProvider` + `NeumorphismThemeToggle` 三态开关（浅色/自动跟随系统/深色）
- 偏好持久化于 localStorage（`echo-panel-theme`），index.html 防闪烁脚本同步初始化

## 开发与构建

```bash
cargo test --workspace          # 后端测试（含 /ws 中继集成测试）
cd web && npm run build         # 前端类型检查 + 构建
cd web && npm run dev           # 前端热更新（/ws 代理到 :8080）
```

## 配置与运维

- `~/.config/echo-agent-panel/panel.toml`：`[server]`（bind_address、static_dir）、`[core].connect_url`（连 Core 的 WS 地址）
- `systemctl --user restart echo-agent-panel.service` 重启；静态资源在 `~/.local/libexec/echo-agent-panel/web`
- 更新走 `echo-agent-panel-update.service`（构建 Rust 服务 + `npm run build` + 替换静态目录 + 重启），详见 [部署与自更新](./ops-deploy.md)
