---
id: panel
title: "Panel 前端"
group: 前端
x: 1920
y: 500
link: ["panel-interaction | 交互定义 | r>l"]
---

# Panel 前端

> **定位**：Panel（EchoAgentPanel）是 EchoAgent 的 Web 管理面板——Rust 后端（axum）托管 Vue 3 + TypeScript 前端（`@echolab-auto/ui-frame` 新拟态组件库），并把浏览器 WebSocket 中继到 Core 的 management WS。默认 `:8080` 提供服务；Core 连不上时页面照常加载并显示「连接中…」（指数退避自动重连）。读者：需要了解 Panel 前后端结构、中继模型与配置运维的开发者。
> 交互与视图契约见 [交互定义](./panel-interaction.md) 及其 7 篇子文档（见「延伸阅读」）。

## 仓库布局

```text
EchoAgentPanel/
├── config/echo-agent-panel.toml     # 面板配置模板
├── source/echo-web-server/          # Rust 后端（axum + WS 中继）
│   ├── src/main.rs                  # 服务入口（路由装配：/ws、/media、/api/logs、/api/upstreams + 静态兜底）
│   ├── src/static_files.rs          # 静态资源（gzip 协商 / 弱 ETag-304 / 分级缓存）
│   ├── src/proxy.rs                 # 浏览器 ↔ 接入点 Core 帧中继（多接入点 {core, frame} 信封、10s 写超时）
│   ├── src/upstreams_api.rs         # 接入点管理 API（/api/upstreams 增删查）+ Bearer 认证中间件
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

## 中继模型

每个浏览器 WS 连接同时中继到**全部**接入点 Core 的 management WS（`[[cores]]`，单接入点 = 一条）；**单接入点时帧按原文转发，不解析、不记录负载**；多接入点时事件按来源节点加壳 `{core, frame}` 信封、命令按信封路由到目标节点（仅信封层，不触碰负载）——协议细节与日志、命令队列完全隔离。协议演进只需同步 core 的 `echo-protocol` 与 `web/src/protocol.ts`，后端零改动。中继对上下行均做 30s Ping 心跳并透传 Ping/Pong；**半死收割**：一侧超过 90s（3 个心跳周期）无任何帧即断开整条链路，设备休眠留下的僵尸连接不会悬挂累积；转发另有 **10s 写超时**（`SEND_TIMEOUT`）——对端写阻塞超时即断链，冻结标签页不会挂住中继、Core 发送缓冲不再无界积压。

- 中继仍无状态（proxy.rs 约 800 行，含接入点聚合、信封路由与令牌校验），可独立测试（`tests/proxy.rs` 用假 Core 验证双向帧透传）
- 多标签页 = 多条 Core 连接（Core 的 management 支持多 Panel）
- 已知限制：无会话恢复——刷新页面先由磁盘缓存（`trunk-cache.ts`）立即可渲染，再经 `RequestState` / `RequestTrunkTimeline{since_seq}` 增量 Bootstrap
- 否决的备选：后端做协议层转发/会话管理（等于重写 Core 桥接层）；浏览器直连 Core :3132（跨域 + 暴露 management 端口）

前端连接 / 重连与加载态的视图侧细节见 [布局与导航](./panel-layout.md)「连接生命周期」。

## 状态管理与数据流

- `store.ts`：全局响应式单例（Vue `reactive`），`dispatch(event)` 逐事件归约
- `state.ts`：reducer 按事件类型分派，原地深变异
- `state_domains/`：timeline（时间线转换/增量/工具配对）、orchestration（活动相位、子代理/后台任务、临时回复分支）、helpers
- 连接管理 `connection.ts`：WS 自动重连；重连后清空运行期状态（分支/任务/活动），时间线保留（内存 + `trunk-cache.ts` 磁盘缓存）并按游标 `since_seq` 增量补齐（响应 `full` 标志时整体替换）
- **加载态跟踪 `pending.ts`**：面板是 fire-and-forget 命令模型（无请求 id），等待态由「请求命令→响应事件」关联表统一登记与销账——`sendPending(cmd, key)` 发命令并登记（key 为视图本地命名，重复发送即覆盖=重试语义）；`dispatch` 收到响应事件自动销账；阶段按时间推进：**0–150ms 不显示**（快请求不闪）→ `pending`（骨架/转圈）→ **>6s `slow`**（附「Core 可能正在重启」提示）→ **>20s `timeout`**（错误 + 重试；请求幂等、重发安全），已显示的加载态保证**最短可见 400ms**。视图以 `usePending(key)` 读取（key 可传 getter，动态键如 QQ 多实例 `qq:filter:<实例>`），以 `LoadHint` 组件渲染；聊天区时间线用状态标志判定（`state.timelineArrivedOnce`）不走本表
- 实时事件按 `team_id` 归一化过滤后才进主时间线（跨 agent 不串显）；`TrunkTimeline` 按 `full` 标志区分全量替换/增量追加

视图渲染与交互细节见 [会话视图](./panel-chat.md)、[布局与导航](./panel-layout.md)。

## 配置与运维

### 接入点、访问令牌与调度

**一套架构，Panel 只是用户入口**：联邦网络是唯一实体（所有 Core 节点对等互联，见 [联邦](./federation.md)）；Panel 是接入这个网络的**用户面板 / 客户端**。`[[cores]]` 不是「另一套架构的上游」，而是 **Panel 的接入点配置**——它连哪些 Core 节点以看到 / 操作联邦网络。中继把浏览器单连接复用到全部接入点：事件按来源节点带 `{core, frame}` 信封、会话列表带来源徽标；命令按当前会话所属节点路由（`connection.ts::resolveTargetCore`），未选中会话时全局类只读命令（如 `RequestTeamsList`）裸广播全部接入点、各端各回一份聚合。接入点可在设置·Core 连接页在线增删（写回 `[[cores]]` 段持久化，中继动态并入 / 摘除，无需重启）；单接入点（`[core]` 或一条 `[[cores]]`）无信封、按原文转发，行为与单上游模式一致。

- **Bearer 访问令牌**：`[server].access_token` 非空时全站（WS 握手 / `/api/*` / `/media`）要求 `Authorization: Bearer <token>` 或 `?token=` 查询参数，静态前端豁免以加载登录页（`proxy.rs::authorized`、`upstreams_api.rs::require_auth`）；前端 URL `?token=` 播种一次后存 localStorage `echo-panel-token`，后续 HTTP 经 `panelFetch` 自动带 Bearer 头（`connection.ts:328-345`）。另有 `[core].access_token` 是连 Core management WS 的认证头，须与 Core 侧配置一致
- **分布式调度器 `scheduler.ts`**：新建会话（或向无归属新对话发首条消息）时的目标节点选择，策略持久化于 localStorage `echo-schedule-policy`：`least_busy`（默认，按 FederationStatus 各 peer 活跃 turn 数取最小，本机参与比较）/ `round_robin` / `prefer:<name>`（亲和，离线退 least_busy）；只做建议，最终路由仍走 connection.ts 的 coreForCommand 链
  - **节点身份统一**：调度器选出的「节点」与路由层认的「core」是**同一个**联邦节点身份（`local` = 本机 = null；联邦 peer 名 = 该节点）。`scheduleNode()` 在 `targetSession?.core` 为空（新会话尚无归属）时给出建议节点（`App.vue:170`），随后 `resolveTargetCore` 按会话/人格归属把命令路由到该节点——该节点必须在 Panel 的接入点配置（`[[cores]]`）里可达，否则调度建议落空。因此**接入点应覆盖联邦网络里要承载工作的节点**（通常 = 全部在线 peer）。

### 面板配置与运维

- `~/.config/echo-agent-panel/panel.toml`：`[server]`（bind_address、static_dir、media_dir 媒体库目录、access_token 面板访问令牌）、`[core]`（connect_url 连 Core 的 WS 地址、access_token 认证头；单上游兜底——`[[cores]]` 键出现过即以在线管理为准）、`[[cores]]`（接入点 name/url/access_token，可空，设置·Core 连接页在线增删并写回）、`[logging]`（level/format/log_file 滚动）
- `systemctl --user restart echo-agent-panel.service` 重启；静态资源在 `~/.local/libexec/echo-agent-panel/web`
- 更新走 `echo-agent-panel-update.service`（构建 Rust 服务 + `npm run build` + 替换静态目录 + 重启），详见 [部署与自更新](./ops-deploy.md)

## 开发与构建

```bash
cargo test --workspace          # 后端测试（含 /ws 中继集成测试）
cd web && npm run build         # 前端类型检查 + 构建
cd web && npm run dev           # 前端热更新（/ws 代理到 :8080）
```

## 延伸阅读

- 前端文档群：[交互定义](./panel-interaction.md)（总入口）→ [布局与导航](./panel-layout.md)、[会话视图](./panel-chat.md)、[模态与覆盖层](./panel-modals.md)、[设置视图](./panel-settings.md)、[QQ 管理·任务·Shell](./panel-qq-tasks.md)、[系统交互](./panel-system.md)
- 相关：[协议与数据流](./protocol.md)（WS 帧格式、命令与事件）、[部署与自更新](./ops-deploy.md)、[联邦（多机）](./federation.md)
