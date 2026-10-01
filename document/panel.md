---
group: 前端模块
x: 591
y: 894
link: ["panel-interaction | 布局与交互定义"]
---

# Panel 前端

Panel（EchoAgentPanel）是 Web 管理面板：Rust 后端（axum）托管 Vue 3 + TypeScript 前端（`@echolab-auto/ui-frame` 新拟态组件库），并把浏览器 WebSocket 中继到 Core 的 management WS。默认 `:8080` 提供服务，Core 连不上时页面照常加载并显示"连接中…"（指数退避自动重连）。

## 仓库布局

```text
EchoAgentPanel/
├── config/echo-agent-panel.toml     # 面板配置模板
├── source/echo-web-server/          # Rust 后端（axum + WS 中继）
│   ├── src/main.rs                  # 服务入口（路由装配：/ws、/media、/api/logs + 静态兜底）
│   ├── src/static_files.rs          # 静态资源（gzip 协商 / 弱 ETag-304 / 分级缓存）
│   ├── src/proxy.rs                 # 浏览器 ↔ Core 帧中继（不解析负载、10s 写超时）
│   └── src/config.rs                # 面板配置加载
├── web/                             # Vue 3 + TypeScript + Vite 前端
│   ├── vendor/ui-frame/             # ui-frame 组件库本地快照（dist + package.json，入库）
│   ├── scripts/use-ui-frame.mjs     # 引用方式切换（本地快照 ⇄ npm 包，见下）
│   └── src/
│       ├── protocol.ts              # echo-protocol 线格式的 TS 镜像
│       ├── state.ts                 # 状态 + reducer
│       ├── state_domains/           # timeline / helpers
│       ├── connection.ts            # WS 连接（重连退避 + Bootstrap）
│       ├── pending.ts               # 请求-响应跟踪（加载三态：150ms/6s/20s）
│       └── components/              # 聊天 / 设置 / QQ / 任务 / 清单
└── scripts/                         # install.sh / update.sh / uninstall.sh
```

**ui-frame 引用方式**：默认以 `file:./vendor/ui-frame`（仓库内提交的本地快照）引用；
`web/scripts/use-ui-frame.mjs` 提供 `ui-frame:local` / `ui-frame:npm` / `ui-frame:sync`
/ `ui-frame:diff` 命令，在本地快照与 npm 已发布包之间切换（依赖行是唯一事实来源，
切换会体现在 git diff）；`sync` 支持从 ui-frame 源码仓库重建快照，`diff` 列出
快照中尚未发版的改动。日常开发/CI 无需切换（CI 用 `npm ci` + 提交的快照）。

## 后端：无状态字节级中继

每个浏览器 WS 连接对应一条到 Core management WS 的专用连接；帧按原文转发，**不解析、不记录负载**——协议细节与日志、命令队列完全隔离。协议演进只需同步 core 的 `echo-protocol` 与 `web/src/protocol.ts`，后端零改动。中继对上下行均做 30s Ping 心跳并透传 Ping/Pong；**半死收割**：一侧超过 90s（3 个心跳周期）无任何帧即断开整条链路，设备休眠留下的僵尸连接不会悬挂累积；转发另有 **10s 写超时**（`SEND_TIMEOUT`）——对端写阻塞超时即断链，冻结标签页不会挂住中继、Core 发送缓冲不再无界积压。

- 中继极薄（一个 crate、约 200 行），可独立测试（`tests/proxy.rs` 用假 Core 验证双向帧透传）
- 多标签页 = 多条 Core 连接（Core 的 management 支持多 Panel）
- 已知限制：无会话恢复——刷新页面先由磁盘缓存（`trunk-cache.ts`）立即可渲染，再经 `RequestState` / `RequestTrunkTimeline{since_seq}` 增量 Bootstrap
- 否决的备选：后端做协议层转发/会话管理（等于重写 Core 桥接层）；浏览器直连 Core :3132（跨域 + 暴露 management 端口）

## 状态管理与数据流

- `store.ts`：全局响应式单例（Vue `reactive`），`dispatch(event)` 逐事件归约
- `state.ts`：reducer 按事件类型分派，原地深变异
- `state_domains/`：timeline（时间线转换/增量/工具配对）、orchestration（活动相位、子代理/后台任务、临时回复分支）、helpers
- 连接管理 `connection.ts`：WS 自动重连；重连后清空运行期状态（分支/任务/活动），时间线保留（内存 + `trunk-cache.ts` 磁盘缓存）并按游标 `since_seq` 增量补齐（响应 `full` 标志时整体替换）
- **加载态跟踪 `pending.ts`（2026-09-30）**：面板是 fire-and-forget 命令模型（无请求 id），等待态由「请求命令→响应事件」关联表统一登记与销账——`sendPending(cmd, key)` 发命令并登记（key 为视图本地命名，重复发送即覆盖=重试语义）；`dispatch` 收到响应事件自动销账；阶段按时间推进：**0–150ms 不显示**（快请求不闪）→ `pending`（骨架/转圈）→ **>6s `slow`**（附「Core 可能正在重启」提示）→ **>20s `timeout`**（错误 + 重试；请求幂等、重发安全），已显示的加载态保证**最短可见 400ms**。视图以 `usePending(key)` 读取（key 可传 getter，动态键如 QQ 多实例 `qq:filter:<实例>`），以 `LoadHint` 组件渲染；聊天区时间线用状态标志判定（`state.timelineArrivedOnce`）不走本表
- 实时事件按 `team_id` 归一化过滤后才进主时间线（跨 agent 不串显）；`TrunkTimeline` 按 `full` 标志区分全量替换/增量追加

## 主视图（聊天）

- `ChatView.vue`：消息列表 + 吸底输入区；自渲染消息行（库 `ChatTray` 容器）拦截扩展角色（reasoning）渲染。
  消息图片渲染（2026-09-24）：Core 侧的 `/media/<id>` 引用直接 `<img loading=lazy
  decoding=async>`（同源、强缓存）；遗留 data URI 兼容；空串（Core 侧"图片已省略"
  占位）渲染为文字标
- `SubagentEventBlock.vue`：子代理委派行（运行中/完成/失败 + 任务摘要，点击展开结论），见 [会话视图](./panel-chat.md)§7.3c。
- `MessageItem.vue`：消息行（user/agent/system）——库 `ChatBubble` + doc `MarkdownRenderer` 自组（2026-09-30，库 chat 组合件移除后）。
- `ReasoningBlock.vue`：推理打字机动画（实时消息 6 秒封顶；历史回放不播）
- 活动浮条：思考中 / 调用工具 / 子代理 的 spinner + 动态文案
- 消息入场动画 0.28s 淡入上移，仅实时消息（`animate` 标记）播放

## 侧边栏与设置页

- `PanelSidebar.vue`：边栏卡片栈（RailStack：连接状态 / 文件浏览器 / Shell / 临时分支；
  `side` 区分左右两列、上下排列、可折叠、分隔条拖动、**卡片可拖到另一列**（拖动机制
  与动画见 [会话视图](./panel-chat.md)§7.8；归属与顺序持久化在 `rail-layout.ts`））
- `WorkspaceFileBrowser.vue`：文件浏览器卡内容（只读；多根切换 chip 悬停**速览绝对
  路径**——Teleport 到 body 的 fixed 速览，绕开卡体滚动容器裁剪，见 [会话视图](./panel-chat.md)§7.8）
- `RailDragGhost.vue`：拖动拖影（Teleport 到 body，跟随指针 + 落位飞行）
- `ConnectionStatusCard.vue`（2026-09-23 从顶栏迁入）：Core 管理通道状态点 + QQ
  适配器逐实例运行态（`已连接`/`等待连接`/`已停止`）；断连时附重连提示。
  紧凑卡（固定高度、不参与 flex 分配）
- `SessionSwitcher.vue`：入口行「会话」按钮弹出——按平台分组（Local/QQ 私聊/QQ 群/其他）切换会话；并行模式附「全局」项；每个会话独立上下文（2026-09）
- `SettingsView.vue`：设置视图——API 设置（`ApiSettings.vue`：概览视图 + 点击「编辑」/「添加 API 服务商」时展开表单，默认不常驻）+ **技能/工具/插件三套工作台**（2026-09-23 重排版：筛选栏 + 双行行卡 hover 快速启停 + 分区详情检查器 + 包⇄工具/技能交叉跳转；技能按包分组、工具按包分组、插件按门控语义分组）+ 智能体的浏览、启停、编辑、删除（左侧一级菜单 + 右侧工作区；2026-09-04 起取代原资源视图与 API 弹窗）。技能编辑含**「系统提示词」开关**（`system: true`，详见 [技能系统](./core-skills.md)）；智能体编辑含**「系统提示词 skills」勾选**（SaveTeam.system_skills）与 Git 安装弹层。布局与交互细节见 [设置视图](./panel-settings.md)§9.2
- `AgentSwitcher.vue`：输入框上方 Agent 切换悬浮卡片；卡片与菜单行显示当前 persona **生效模型**（按 `api_profile` 从全局供应商池解析，未引用 = 全局默认 model）
- `AgentConfigModal.vue`：聊天区 ⚙「配置」按钮唤起的**会话区内磨砂玻璃弹层**——
  名称/描述/系统提示词/启用/**API 供应商下拉**（`api_profile`，见 [设置视图 §9.1.1](./panel-settings.md)）/插件/工具/技能白名单（表格 + pkg 分组；"系统提示词 skills"勾选**不在此弹层**——仅设置页智能体编辑提供），
  保存走 SaveTeam；上/左/右距会话框 12px、底部距配置按钮 12px
- `ContextView`（ChatView 内）：入口行「上下文」唤起的弹层——**几何与配置弹层一致**（上/左/右 12px、底部距入口行 12px），点遮罩关闭、无返回按钮
- `ShellPanel.vue`：Shell 详情视图（无顶栏入口，经边栏「Shell」卡「详情」进入）——持久 bash 会话终端可视化
  （停止会话、命令回显 + 流式输出自动吸底、运行态 spinner、
  完成/失败/超时状态、Enter 执行 Esc 清空；**新建会话在边栏「Shell」卡**（`ShellList.vue`，
  workdir 缺省），详情头部显示该会话 workdir——本视图无工作目录输入）

## 取消任务的即时反馈

- 前端 `cancelSessionWork`（state.ts）**乐观中断**：点击取消后立即
  活动浮条/取消按钮消失（phase → completed）、running 任务/分支 tab → cancelled、
  running 工具卡 → failed（"已取消"），不等后端确认
- 取消命令带 `team_id`（当前 agent）——多 agent 下按 persona 路由；
  后端取消成功即回 `AgentCompleted` 对齐状态
- 入口：聊天区"取消任务"按钮（busy 时显示）与任务页取消按钮，共用同一逻辑

## 时序与动画规范

主时间线对 Agent 运行过程的展示规范（一致性基线，改动需保持三条规则）：

**1. 及时性（实时渲染，不缓冲）**：`AgentReasoning` / `ToolCall` / `ToolResult` / `AgentOutput` 事件到达即渲染进主时间线，**不得**等回复完成后一次性写入；推理是独立 `reasoning` 角色消息按序插入，不缓冲到回复尾部。

**2. 时序表现（还原真实顺序）**：主时间线顺序 = 事件到达顺序（`用户消息 → 推理 → 工具调用 → 工具结果 → … → 正式回答`），推理真实穿插在工具调用之间，正式回答始终最后；历史回放（`loadTimeline`）同样把 `backend.reasoning` 拆分为独立 reasoning 消息、插在回答之前，与实时路径时序一致；侧边栏临时分支详情继承同一规则（仅做连续 reasoning 的合并展示）。

**3. 动画反馈（对应位置对应动画）**：

| 阶段 | 位置 | 动画 |
| --- | --- | --- |
| 连接中 | 边栏「连接状态」卡（2026-09-23 起；原顶栏状态点，可拖到任一列） | 状态点呼吸 + 重连提示 |
| 思考 / 调用工具 / 子代理 | 输入框上方活动浮条 | 旋转 spinner + 动态文案 |
| 推理输出 | 推理块 | 打字机逐字显示（自适应速度，约 6s 封顶）+ 光标/呼吸点 |
| 工具执行中 | 工具卡 | running 状态 spinner |
| 消息到达 | 每条实时消息 | 0.28s 淡入上移入场动画 |

- 打字机/入场动画**仅**对实时消息（`DisplayMessage.animate === true`）播放；历史回放、刷新加载不播动画
- 动画为纯视觉层：`animate` 是展示元数据，不进入任何数据/逻辑判断
- 实现要点：ui-frame `ChatRole` 不含 `reasoning`，必须在 `ChatView` 的消息行渲染层拦截（自渲染行循环，见 [会话视图](./panel-chat.md)§7.3b；否则未知角色会被渲染成 Agent 气泡）；`pendingReasoning`/`completedReasoning` 为**历史遗留字段**（分支合并块已随 ui-frame 移除，现无任何读取方，仅声明与初始化）

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
