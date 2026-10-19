---
id: instance-plugin
title: "实例插件化设计（规划）"
group: 规划
x: 960
y: 3160
link: ["adapter-genericization | 插件实例泛化（设计系列） | b>l"]
---

# 实例插件化设计（规划）

> **定位**：本页是「Chat 插件实例作为外部进程插件」的设计提案——目标架构、通用实例插件规范（协议扩展与接口）、宿主 / 插件两侧落地清单、迁移路线与验收。读者：插件体系与插件实例维护者。**状态：P1 已落地（外部进程 QQ 插件，2026-10）；P2 已落地、P3 收尾中（2026-10-16）**——实施进度以 [完全解耦推进计划](./decoupling-plan.md)（Phase 3 ④ 本设计对应项）为准。
> 前置阅读：[插件开发指南](../plugins/plugin-authoring.md)（协议与装载现状）、[完全解耦推进计划](./decoupling-plan.md)（方案背景与分期）、[QQ 实例门控](../plugins/qq-gating.md)（现状语义）。
> **评审记录**：本设计已过两轮独立评审。第一轮修订（协议 delta 清单 / 「先停后切」交接语义 / 入站反压·串行·回执路由修复）与第二轮（独立性审计 / 线协议通用化 / Panel UI 解耦 / UI 描述符规范）均已并入正文。

## 目标与非目标

**目标**

1. **独立仓库**：QQ 插件实例作为独立仓库的插件二进制交付（`echo-adapter-qq` 及相关代码迁出），经 `plugins.toml` 从外部插入 Core；
2. **通用规范**：定义「实例插件规范」——任何平台（QQ / Telegram / …）按同一接口交付。规范 = echo-plugin 协议**增量** + **线协议通用化**（§实例管理通道）+ **UI 描述符**（§Panel UI 解耦）+ 一组接口约定；类型即规范（`echo-plugin-api`）；
3. **三项收益**（与解耦计划一致）：**崩溃隔离**（插件实例挂 = 不拖垮 Core）、**独立演进**（改插件实例不必重编 Core）、**可热替换**（结合 P5 `PluginHandle::upgrade`；端口占有型插件实例为「先停后切」秒级自动恢复交接，非零停机——见 §交接与崩溃恢复语义）；
4. **零平台残留**：终态 Core / Panel 无线协议级平台词汇——判据：全仓 grep 无 `qq` / `onebot` / `napcat` 字面量（文档、迁移兼容层与测试名除外），见 §实施路线与验收。

**非目标**：动态库加载；把 agent 逻辑搬进插件。

**线协议口径（评审修正）**：原「Panel 零改动 / 线协议不动」口径**不成立**——平台词汇长在 `echo-protocol`（Core↔Panel 共享 crate）里，宿主侧做翻译层只会把残留从插件实例搬到协议适配层、永久固化。正确做法：**线协议同步通用化**（§实例管理通道），Panel 分两阶段迁移（先换通道、后换 UI，见 §Panel UI 解耦）。

**为什么走子进程**（而非 inproc 协议化）：外部仓库 + 从外部插入 = 必须免重编译，inproc 协议化仍是编译期链接、不满足该要求；崩溃隔离也只在子进程成立。性能上（解耦计划 §2.2 实测）往返 20–60µs/次，对消息与工具调用（秒级间隔）富余。

## 现状与缺口（为什么可行）

**已具备**——三块拼图已就位：

- **接缝本就平台中立**：定义层的 `PluginInstance` trait（`echo_defs::plugin_instance::PluginInstance`，`echo-defs::plugin_instance` re-export 兼容；start/stop + 描述符/管理面访问器（`management()` / `ui_descriptor()` / `status_info()` / `tool_specs()`）——**没有 send_text/get_groups/get_friends**：出站与目录查询已全部收敛为 `InstanceManagement`）、`InboundMessageHook`（单向入站桥）、`InstanceManagement` 能力 trait——插件实例对 agent 的接口早已与平台解耦；
- **物理拆分半程**：`echo-adapter-qq` 是独立 crate，不含 agent 逻辑；工具（`crates/echo-adapter-qq/src/tools.rs`）、入站桥（`echo-agent/packages/instance_ops/bridge.rs`）、命令域（`commands.rs`）、命令处理器（`core/handlers/`）都在宿主侧——天然留在 Core；
- **插件基础设施可用**：协议 v1（Hello/Register/Invoke/HostCall/…）、SDK（`serve_stdio`）、`PluginSupervisor`（状态机 / 崩溃重启 / upgrade 热替换）、`plugins.toml` 装载、`RemoteTool` 先例、`HostCall` 宿主服务先例（`sanitizer`）。

**缺口**：

| # | 缺口 | 后果 |
|---|---|---|
| 1 | 协议无「插件实例」贡献类型 | 插件无法声明自己是插件实例 |
| 2 | 宿主无 `RemoteInstance`（`PluginInstance` trait 的远程实现） | 插件实例列表 / 工具 / 命令域对接不上 |
| 3 | 入站方向无通道（`Emit` 无消费方、无 `core_api` 宿主服务） | 插件收到的消息进不了 agent |
| 4 | 平台命令处理器寄居实例注册表、依赖 OneBot Event | 远程化后必须归位宿主、改吃平台中立词汇 |
| 5 | SDK 无插件实例脚手架 | 作者样板成本高，规范难落地 |
| 6 | 实例 / 配置 / 持久化归属未定 | 迁移边界不清 |
| 7 | **UI 归属未定**（Panel 无插件 UI 扩展机制；`echo-protocol` 内嵌 10 个 QQ 命令、7 个 QQ 事件、`InstanceStatus` 4 个 QQ 字段、`command_clearance` QQ 枚举） | 平台残留固化在共享线协议；实例管理界面无法随插件走 |

## 总体架构与职责边界

```
┌─ 独立仓库：echo-adapter-qq（插件二进制）───────────────┐
│   OneBot v11 反向 WS · NapCat 容器 / 端口 / compose      │
│   平台事件解析 · 门控过滤 · 媒体下载 · 平台管理动作        │
│   InstancePlugin 实现：<name>:<op> 路由 + 入站上报        │
└───────────────────────────┬────────────────────────────┘
                            │ stdio · echo-plugin 协议 v1（+ 插件实例规范）
┌─ EchoAgentCore ───────────▼────────────────────────────┐
│  PluginSupervisor ──► RemoteInstance（实现 PluginInstance trait） │
│         │                    │                             │
│         │                    ▼                             │
│         │            InstanceRegistry（列表/状态/管理命令）  │
│         ▼                    │                             │
│  CoreApiService  ◄───────────┘（submit_input / report_*）  │
│         │                                                   │
│  结构化解包 → AgentMessageHook → agent（一轮 turn）          │
│  工具注册：插件贡献的工具 → RemoteTool / 实例工具桥          │
└─────────────────────────────────────────────────────────────┘
```

**Core 侧只剩四个接缝**（2026-10 去适配器化收尾后，名字里都没有平台字样）：

| # | 接缝 | 方向 | 载体 |
|---|---|---|---|
| 1 | **贡献申报** | 插件 → Core | `Contribution::{Tool, Skill, Service, Events, PluginInstance}`（`Register` 携带） |
| 2 | **入站投递** | 插件 → Core | `HostCall("core_api", "submit_input", {message})`；状态经 `report_state` / `report_ui_state` |
| 3 | **出站调用** | Core → 插件 | `Invoke("<instance>:<op>", payload)`——工具调用与管理动作同一条通道 |
| 4 | **进程监督** | Core → 插件 | `echo-plugin-host` 的 supervisor（启动/重启/热替换/排空），与平台无关 |

平台名只以**数据**形式出现（`InstanceContribution.platform`、`[instances.<name>]` 的段名），不出现在任何 Core 类型、模块或常量里。

**职责边界**：

| 归属 | 内容 |
|---|---|
| **插件**（平台侧） | 传输（反向 WS）、平台事件解析与消息转换、触发与门控过滤、媒体 / 文件下载、NapCat 容器管理、平台管理动作（登录二维码、群友列表…） |
| **宿主**（agent 侧） | 实例注册表与列表、persona 绑定与会话归属、消息送达（hook + 结构化标记）、工具注册与出站闸门（脱敏 / 敏感路径）、门控配置命令的接转、管理 WS 事件 |

**宿主不再做的事**（2026-10 移交或删除）：端口分配 / 容器名派生 / `docker-compose.yml` 生成（→ 插件 `deploy` 模块）；平台消息命令（`/echo` 等）与框架侧控制回复（取消回执 / 失败通知 / 临时等待回复——见下方「出站面收窄」）。

一句话：**平台内聚在插件，agent 面向策略在宿主**。

**实例模型**：一个插件进程可注册**一个或多个** PluginInstance 贡献（多账号）；部署建议每账号一条 `plugins.toml` 条目（进程隔离最大化），单进程多账号同样受规范支持。**persona 绑定在宿主侧**（条目配置中的 `persona` 由宿主读取，插件不解释 persona）；**端口由插件分配并持久化**（写入实例自己的 state 文件 `[deploy.ports]`，跨重启稳定；宿主只下发原始配置 `shared` + `overrides`）；实例名（账号）全局唯一，重名注册报可读错误。

## 协议规范（Adapters over echo-plugin）

> **增量原则**：只增不改——新增贡献类型与接口约定、给既有类型增补可选字段；不新增消息类型、不 bump 协议版本。`Invoke`（call_id / Cancel / deadline / 并发）与 `HostCall`（服务回呼 + 回执）两条既有通道直接复用。**本次协议 delta（小、向后兼容）**：① `InvokeOutcome::Ok` 增补 `#[serde(default)] result: Value`——插件实例 op 的结构化返回（`status` / 目录 / 二维码）经它承载，不再 JSON 塞 `text`；② `Hello` 增补宿主 `capabilities` 特征位；③ `Register` 解析改「逐项容错」（见「版本与兼容」）。

### 贡献：`Contribution::PluginInstance`

```rust
InstanceContribution {
    name: String,             // 实例名（= Invoke 前缀；全局唯一）
    platform: String,         // "qq"（会话作用域 / 显示）
    display_name: String,     // "QQ / OneBot"
    capabilities: InstanceCapabilities, // 能力位：send_text / send_media / groups / friends / gating / login
    config_schema: Option<Value>,      // 插件配置 JSON Schema（校验 / 面板表单，可选）
    ui: Option<Value>,                 // UI 描述符（Panel 通用渲染器）
    session_labels: Option<Value>,     // 会话分组标签
    tools: Vec<ToolContribution>,      // 贡献给模型的工具（宿主不认识任何工具名）
}
```

宿主只调用能力位声明过的方法；未声明的调用拒绝为 `unsupported` 错误（fail-closed）。

**线上 tag 冻结**：该变体在 JSON 里序列化为 `{"kind":"adapter", …}` —— 字符串**保持旧值不变**（Rust 侧名字去 adapter 是内部可读性，宿主与插件是两个可独立升级的进程，改它等于同时打断「新 Core + 旧插件」与「旧 Core + 新插件」两个方向）。`plugin_instance` 作为 `alias` 接受，为将来换 tag 留过渡窗口；`echo-plugin-api` 有冻结测试守着这一条。

### 宿主动作：`Invoke`（`contribution = "<name>:<op>"`）

| op | 参数（payload） | 返回（经 `result`） | 说明 |
|---|---|---|---|
| `start` / `stop` | `{}` | `{}` | 生命周期；宿主在 `PluginInstance::start/stop` 时调用 |
| `status` | `{}` | `{state, self_id?, started_at?}` | 状态快照（宿主缓存；变化由插件另行上报。**进程态仲裁**：`status_info()` = PluginSupervisor 状态 ∧ 缓存连接态——进程 Pending/Failed 时一律显示 Starting/Error，绝不显示缓存 Connected） |
| `send_text` | `{target, content}` | `{message_id?}` | 出站文本 |
| `send_media` | `{target, kind: "image"\|"file", source: {url\|path\|data_uri}}` | `{message_id?}` | 出站媒体（>4 MiB 走 path / 先 `store_media`，见安全边界） |
| `get_groups` / `get_friends` | `{}` | `[{id, name}]` | 目录（对应能力位） |
| `custom` | `{method, payload}` | 插件自定义（`result` 承载） | 平台专属动作透传（QQ：门控 / 名单 / owner / 登录二维码 PNG（base64）…） |

**结构化返回**：op 的返回值一律经协议 delta ① 的 `InvokeOutcome::Ok.result` 承载（不 JSON 塞 `text`）；`Ok.text` 对插件实例 op 忽略。

`target` / `message` 的 JSON 结构与 `echo-defs` 词汇同构（`MessageTarget` / `IncomingMessage`），在 `echo-plugin-api` 以纯 JSON 结构镜像（保持该 crate 零 echo-* 依赖）：

```jsonc
// target
{ "channel": { "type": "dm" } }
{ "channel": { "type": "group", "group_id": "456" } }

// message（交付载荷；当时叫 deliver_message，现为 submit_input）
{ "instance": "qq", "platform": "qq", "user_id": "123", "user_name": "Alice",
  "channel": {"type": "group", "group_id": "456"}, "group_name": "开发群",
  "content": "hello", "timestamp": 1733814154, "at_me": true,
  "metadata": {}, "images": ["/media/ab12…"], "files": [] }
```

### 插件侧分发表：`custom` 是唯一入口（2026-10 教训）

宿主有**两个下发入口**，落在插件侧却是**同一个 op**：

| 宿主侧来源 | 线上 op | method 可能是什么 |
|---|---|---|
| 工具桥（`instance_tools::invoke_management`） | `custom` | 工具名（`send_private_msg` / `get_group_list` …） |
| 管理面（`InvokeInstanceOp`，Panel 按钮） | `custom` | 生命周期（`status` / `start` / `stop`）或平台管理方法（`set_gate_mode` / `login_qrcode` …） |
| 宿主内部生命周期（`RemoteInstance::start()`） | 直连 op | `start` / `stop` |

因此插件必须把 **三张表**串成一条查表链，且**两个入口走同一条链**：

| 顺序 | 表 | 例子 |
|---|---|---|
| 1 | 模型工具 | `send_private_msg` / `get_group_list` |
| 2 | 生命周期与出站 | `start` / `stop` / `status` / `send_text` / `get_groups` |
| 3 | 平台管理方法 | `set_gate_mode` / `update_allowlist` / `login_qrcode` |

QQ 插件里这体现为 `QqPlugin::dispatch(adapter, op, payload, via_custom)`；
`via_custom` 只影响**未知名时的错误码**（直连 = `unknown_op`，`custom` = 管理表的
`unknown adapter method`），便于调用方区分「我发错了 op」与「插件没实现该方法」。

> **两个真实回归（2026-10 实测踩到，各自让一半功能失效）**：
> ① `custom` 只查管理表 → 模型调 `send_private_msg` 报 `unknown adapter method`，
> **10 个工具全废**；
> ② 修完①后 `custom` 只查工具表 → Panel 的「启动 / 停止 / 状态」同样报错，
> **实例面板全废**。
> 根因是插件里存在**三张平行分发表**、两个入口各查一部分。合表后两个入口语义一致，
> 并有 `custom_op_reaches_every_table` / `custom_op_reaches_tool_table` 两个用例
> 逐表断言可达。

### 插件动作：`HostCall` → 宿主服务 `core_api`

| method | 载荷 | 回执 | 语义 |
|---|---|---|---|
| `submit_input` | `{message}`（`instance` 在 `message` 里——宿主从 `message.instance` 取路由目标） | `{accepted}` / `Error{code:"backpressure"}` | 入站消息（门控已过）**入队**送达宿主；回执 = 已入队，**不等于已处理** |
| `report_state` | `{instance, connected, self_id?}` | `{}` | 连接状态变化——**`instance` 必填**，缺失即 `instance_not_found` |
| `report_stopped` / `report_error` | `{instance, reason?}` / `{instance, message}` | `{}` | 生命周期 / 错误上报——**`instance` 必填**；`reason` / `message` 目前宿主收到但**不读**（只用于路由） |
| `store_media` | `{path \| bytes_b64, mime?}` | `{ref: "/media/<id>"}` | 媒体登记（宿主 media_store 统一命名；同机优先传 path，避免大载荷过协议） |

> **共性**：`report_*` 三个回呼都靠载荷里的 `instance`（兼容旧字段 `adapter`）路由到具体实例，缺了就 `instance_not_found`；`submit_input` 的目标在 `message.instance` 里。

**入站三件套（实现约束）**：① **回执语义 = 已入队**——`HostService::call` 是同步签名，宿主 LLM 排队绝不能在回执路径内阻塞，入队后立即回执；② **每实例有界队列 + 反压**——队列满回 `Error{code:"backpressure"}`，插件退避 / 落盘重试；③ **每实例串行 worker 保序**——HostCall 每调用独立 spawn 会打乱同一会话消息序，`core_api` 必须按实例串行化入站。

**选型说明**：入站走 HostCall（有回执、插件可退避 / 防重）而非 `Emit`（当前无消费方、缓冲丢新语义）；宿主服务机制已有 `sanitizer` 先例。`Emit` 保留给未来通用事件（如子代理回灌一类）。

### 实例管理通道（线协议通用化，Core ↔ Panel）

> 平台词汇不只在内核：`echo-protocol`（Core↔Panel 共享 crate）里有 **10 个 QQ 命令**、**7 个 QQ 事件**、`InstanceStatus` 的 4 个 QQ 字段与 `command_clearance` 的 QQ 枚举分支（当时的变体名如 `StartAdapter` / `QqManagement` 现已在 `echo-protocol` 中物理删除）。**全部通用化**——否则平台残留固化在共享线协议里（评审判定：这是可行的前提，也是原「零改动」口径不成立的原因）。

**协议 delta（Core ↔ Panel）**：

```rust
// command.rs —— 唯一新增的实例管理命令（替代 10 个 QQ 变体）
BackendCommand::InvokeInstanceOp {
    instance: String,          // 实例名
    method: String,             // 插件自定义方法名（与插件协议 custom op 同名）
    payload: serde_json::Value,
}

// event.rs —— 新增 2 个事件
BackendEvent::InstanceOpResult { instance, method, result: Value }  // 请求-响应；pending 键配对
BackendEvent::InstanceState    { instance, state: Value }           // 插件主动状态推送（epoch 版本化）

// event.rs —— AdapterStatus 净化
// persona 保留（概念中立）；container / webui_url / onebot_url
//   → metadata: Vec<{ key, label, value }> 展示型键值对（平台自有字段名随插件来）

// event.rs —— id 语义去号码化（线协议 breaking）
// GroupInfo.group_id / FriendInfo.user_id: i64 → String
//（QQ 号码语义泄漏、Telegram/Discord 不成立；同发布节奏可承受，升级指南写明）
```

- **映射（迁移期历史）**：`RequestQqLoginStatus` → `{method:"login_status"}`；`RequestQqQrcode` → `{method:"login_qrcode"}`；名单 / 门控 / owner 六命令 → `{method:"update_allowlist" / "set_gate_mode" / …}`；宿主译为插件协议 `Invoke("<name>:custom")`（与 §宿主动作 表对齐）。**旧命令载体已随泛化收口物理删除（2026-10-16），映射仅存于本文档作为迁移记录**；
- **半通用项顺势改名**（`Adapter` 一词后来又在 `2d46b7d` 换成 `Instance`，下表是当时的中间态）：`RequestGroupList` / `RequestFriendList` → `RequestAdapterGroups` / `RequestAdapterFriends`（`Adapter::get_groups/get_friends` 本已存在）；`Start/Stop/RestartAdapter` + `RequestAdapterStatus` + `InstanceList` 本就平台中立，不动；
- **clearance 语义（安全面保留、判定方式改变）**：`command_clearance` 从「按 QQ 变体逐一枚举」改为「**按类别判定**」——一切 `InvokeInstanceOp` = Frontend-only（fail-closed；消除「忘加枚举臂」回归窗口）。现有「逐变体禁用清单」测试改写为「按类别判定」测试；
- **QQ 变体处置（2026-10-16 更新）**：新通道落地后原计划「10 命令 + 7 事件标 deprecated、两个版本周期后删除」；**决策人指示直接删除（不等 N+2）**——旧 10 命令 / 7 事件 / 3 字段与 `legacy.rs` 兼容层已同批物理删除，见 §实施进度。

### 出站面收窄：宿主不再代发（2026-10 决策）

去适配器化时明确了一条边界：**Core 不主动往平台推文本**。此前框架有三处代发
（都发生在 agent 停机或忙碌时，无法由 agent 自己调工具）：

| 场景 | 原行为 | 现行为 |
|---|---|---|
| 用户发「取消」 | 框架回「已停止当前任务」 | 回执只写入后端会话历史（模型可见、Panel 可见），**不推平台** |
| turn 执行失败 | 框架回「处理失败：…」 | 只发 `BackendEvent::Error`（Panel 可见） |
| 并行模式下 20 秒无输出 | 框架生成并推送「临时等待回复」 | 整条机制删除（含 `spawn_contextual_wait_reply` / `generate_wait_reply`） |

结果：**出站面只剩一条路**——模型调用插件贡献的工具（`send_*`）。好处是 Core
里不再有「按会话找回实例并让它发一句话」这条隐式通道，插件与宿主的接口因此
收敛为上表那四条；代价是平台侧少了三类框架回执。

同一轮删除的还有平台消息命令（`/echo` `/help` `/status` `/shutdown`，原
`core/src/instance_commands.rs`）：命令前缀与 owner 判定都是平台会话语义，
且回复同样依赖代发。

### 版本与兼容

- **旧宿主 + 新插件**：`Register` 里出现未知贡献变体 → 整包解码失败 → 握手失败（可读错误）。
  能力位走**反方向**：`capabilities` 在 `Welcome`（插件→宿主）里，插件据此自报能提供什么
  （SDK 的 `serve_instance_stdio` 会自动追加 `"adapter"` 位）；`Hello`（宿主→插件）只有
  `protocol` / `version` / `plugin_id` / `config` 四个字段，没有 capabilities。
  实际识别实例插件靠**贡献类型本身**（`Contribution::PluginInstance`），不靠能力位协商
- **新宿主 + 旧插件**：不受影响（serde 忽略未知字段）；
- **注册解析细节**：`Register.contributions` 的类型是 `Vec<Contribution>`——**没有** `Unknown(Value)`
  降级路径（`#[serde(other)]` 只能落在 unit variant、拿不到载荷，加不了兜底）。所以未知
  kind 直接让整包解码失败（契约测试 `tests.rs` 就断言必须拒绝）——这正是上面
  「旧宿主 + 新插件 = 握手失败」的机制来源
- **既有缺陷修复（P0 前置）**：`dispatch` 的 `HostCall` 分支经 `shared.current_conn()` 回发结果——upgrade / 排空切换瞬间，旧实例发出的 `submit_input` 回执会错投**新连接**（call_id 对新实例是孤儿，而载荷已被宿主消费；若插件因未收到回执重试 → 重复投递）。修复：回执按**来源连接**路由（或 call_id 携带连接代际）。

### 安全边界

- **帧上限 16 MiB**：大文件不走协议，走共享文件路径；**`send_media` 阈值**：`data_uri` 内联限 ≤4 MiB（base64 +33% 膨胀），超限强制 `path`（同机天然共享文件系统）或先 `store_media` 登记再传引用——避免 16 MiB 上限变成隐性「约 12 MiB 文件上限」且报错难懂；
- **env 白名单**：NapCat 容器管理需 `docker` CLI 可达（PATH 已透传；`DOCKER_HOST` 等如需须显式注入）；**插件自持久化目录**由宿主显式注入（env 白名单下无 HOME 假设）；插件日志走 stderr（宿主转发到 tracing）；
- **两层出站闸门（不混同）**：宿主闸门（脱敏器命中阻断 / 敏感路径拒发，在 `qq_tools` 层）+ 插件内 `check_outbound_gate`（gate_mode 出站检查，随平台走）——各拦各的，插件化不削弱 2026-10 安全基座；
- （随 P6）插件资源限额与权限声明。

## 宿主侧实现（Core）

1. **`echo-plugin-api`**：`InstanceContribution` + 能力位 + 消息 / 目标 JSON 结构；`Register` / 消息体改「逐项容错解析」（未知项降级 `Unknown(Value)` 并 warn，见「版本与兼容」）。
2. **`echo-plugin-host`**：
   - `RemoteInstance`（实现 `echo_defs::plugin_instance::PluginInstance`；`echo-plugin-host` 只依赖 `echo-defs` + `echo-plugin-api`/`-loader`，不依赖 `echo-defs::plugin_instance`）：异步方法 → `Invoke`；`status_info()` 读缓存但**合并 PluginSupervisor 进程状态**（进程 Pending/Failed 时绝不显示缓存 Connected，见 op 表仲裁）；**管理面生命周期（实施决策）**：`RemoteInstanceManagement` 挂为 `RemoteInstance` 的字段，持对宿主的 `Weak` 弱引用（避免循环强引用；宿主销毁后调用方拿到可读 `internal error` 而非悬垂指针）——零 unsafe、无占位指针；
   - **`QqManagement` / `qq_management()` 从 `echo-defs::plugin_instance` 删除**（评审升级）：置换为平台中立的 `InstanceManagement`——`cached_state() -> Option<Value>`（读 epoch 缓存）+ `async invoke_custom(method, payload)`（异步写）；远程化后「同步 setter」只剩乐观缓存假象，收敛为「缓存读 + 异步写」，不再提供假同步面；
   - **缓存失效 = epoch 语义**：连接态 / 门控缓存挂在 `PluginHandle` 的**重启 / 升级计数**上——插件崩溃重启后运行时状态（名单 / gate_mode）全丢，缓存不刷新即**安全相关漂移**（denylist 丢失 = 被拦用户恢复通行）；epoch 变化 → 缓存置脏、按需重拉；
   - `CoreApiService`（实现 `HostService`）：按 name fan-out 到各 `RemoteInstance` 的**有界入站队列 + 每实例串行 worker**（回执 = 已入队；满 → `backpressure`，见 HostCall 表）；
   - `StartedPlugins::instances()`（与 `tools()` 平行）汇总远程插件实例。
3. **组合根（`core/src/main.rs`）**：早期注册 `core_api` 服务；插件启动后把 `RemoteInstance` 注册进 `InstanceRegistry`（P1.9 起**唯一路径**——内置装配已删除）；persona 绑定 → 每实例串行入站 worker（`submit_input` → 命令域 → `AgentMessageHook`）；`instance_tools` 泛化为 `Arc<dyn PluginInstance>`、平台动作走 `<name>:custom`。**注册表前置依赖（P1.6）**：`InstanceRegistry` 改内部可变性（`RwLock<HashMap>` + `register(&self)` + `get` 按值克隆）——运行期远程插件实例注册需 `Arc` 下可写；
4. **平台命令处理链归位**：`/echo`、`/help`、`/admin` 从「实例注册表（OneBot Event）」迁到「宿主送达入口（平台中立词汇）」——处理器本就依赖宿主控制面（`shutdown_tx` / `tracker`），迁移同时使其平台中立；**内置插件实例一并切换**（单一路径），语义用双跑测试锁定。
5. **Panel**：线协议**同步通用化**（§实例管理通道）；Panel 两阶段迁移——先换通道（P2）、后换 UI（schema 渲染器，见 §Panel UI 解耦）；「零前端改动」口径已废弃（评审判定：翻译层会把平台残留永久固化）。

## 插件侧：SDK 与 QQ 插件仓库

**SDK 增量**——`InstancePlugin` 脚手架（作者只写平台逻辑）：

```rust
#[async_trait]
impl InstancePlugin for QqPlugin {
    async fn configure(&self, config: Value) -> Result<(), String>; // Hello.config（原始配置，见下）
    fn instance(&self) -> InstanceContribution { … }                 // configure 之后调用
    async fn invoke_op(&self, op: &str, payload: Value) -> InvokeOutcome; // 前缀 "<name>:" 已剥离
    async fn on_ready(&self, host: HostClient);                      // 自存，用于 submit_input / report_state
}
// main: serve_stdio(InstanceHandler::new(QqPlugin::new())).await
```

已落地（P1.2）：`InstanceHandler<A>` 把 `InstancePlugin` 包装为 `PluginHandler`（注册单个 `Contribution::PluginInstance`，按实例名匹配寻址）；SDK 读循环对 `"<name>:<op>"` 按已注册实例名放行；`PluginHandler::configure` 为通用钩子（缺省忽略，拒绝 = `Failed{invalid_config}`）；`serve_stdio` 自动在 `Welcome.capabilities` 追加 `"adapter"` 特征位（wire 值不变）。

**实例插件配置（`Hello.config` 形状，P1.7b 冻结；宿主组装，P1.8 已接线）**：

```jsonc
{
  "name": "qq",                  // 实例名（= Invoke 前缀；宿主分配，全局唯一）
  "persona": "default",          // 归属人格（可选；显示名与会话归属）
  "state_file": "/…/qq-<id>.toml", // 运行期状态持久化文件（可选但建议：
                                 //  门控 / 名单 / owner 落盘于此，重启回灌、
                                 //  崩溃不回退——安全相关，见 §风险）
  // 新宿主（2026-10）只下发**原始配置**，部署形状由插件算：
  "shared": { /* [instances.qq] 共享层，原样 */ },
  "overrides": { /* [instances.qq.instances.<id>] 实例覆盖，原样 */ },
  "data_dir": "/home/u/.config/echo-agent-core",
  // 旧宿主下发的是成品（共享层 ⊕ 覆盖的合并结果），只给已部署旧插件读；
  // 新插件读 shared/overrides 自己合并 + 派生端口 / 容器名 / compose。
  "config": { }
}
```

**QQ 插件已落地（P1.7a–c，独立仓库 `~/code_file/gitlab/echo-tech/echo-adapter-qq`）**：代码迁入（`crates/*` + 拆解后的自有模块），`plugin.rs` = `QqPlugin`（op 路由 `start`/`stop`/`status`/`send_text`/`send_media`/`get_groups`/`get_friends`/`custom`；入站 `submit_input`（反压退避重试）+ 连接态 `report_state`）；`configure` 合并 `state_file` 的运行期状态到宿主基线之上。P1.7c 去 vendor（领域词汇 / filter / ConfigStore 收归插件自有，入站图片走 `store_media`）；231 测试全绿 + 真实进程 stdio 冒烟（握手 / op / Dispose 干净退出）。**P1.8 宿主装配已落地**（2026-10）：`[instances.qq] mode = "plugin"` + `[instances.qq.plugin] command` 声明后，Core 按实例组装 `Hello.config` 并经 `plugins_runtime::start_with_specs` 拉起插件进程；远程插件实例融入统一实例表；入站队列起每实例串行 worker（`submit_input` → `AgentMessageHook`）；`report_state` → `InstanceStateChanged` 事件；`instance_tools` 泛化为 `Arc<dyn PluginInstance>` 且出站 / 目录 / 历史统一经管理面 `invoke_custom`。冒烟：core 真实拉起插件 → 注册 / 接线 / start（bind 端口）→ 真实 NapCat 反向 WS 接入 → SIGTERM 优雅停（Drain → 插件退出）。**P1.9 已完成**（2026-10）：命令域迁移（`core/src/instance_commands.rs`（已于 2026-10 随选项 B 删除）+ 插件侧 `sender_role` 标注）；**删宿主内置 QQ**——内置装配整体移除、`core/handlers/` 删除、配置本地化（`[instances.qq]` 以 TOML 表透传：共享层 ⊕ 实例覆盖深合并，宿主只读 `enabled` 与实例表 / 启动段（凭据按键名形态收集））、`echo-adapter-qq` / `echo-core` / `echo-server` 三 crate 从宿主工作区删除（代码在插件仓库）。

**QQ 插件仓库（首个样本）**——现状代码整体迁出 + 适配层：

```text
echo-adapter-qq/（独立仓库，cargo workspace；P1.7a–c 已落地）
  crates/echo-adapter-qq/   # 插件实例主体
    src/main.rs             # serve_stdio(InstanceHandler::new(QqPlugin::new()))
    src/plugin.rs           # InstancePlugin：分发表 + 入站上报 + store_media 媒体通道
    src/types.rs            # 领域词汇（消息 / 目标 / 事件 / 状态；插件自有）
    src/filter.rs + src/config_store.rs   # 门控过滤管道 + TOML 持久化（插件自有）
    src/napcat/ + src/adapter/qq/         # 容器管理 + OneBot 反向 WS（现主体）
  crates/echo-core + echo-server          # OneBot v11 协议与反向 WS（运输层）
```

依赖 `echo-plugin-sdk`（git tag / 发布通道）+ 传输依赖；**不再依赖 `echo-defs::plugin_instance`**（trait 词汇经 SDK 镜像）。

**P1.1 迁出方案（依赖面实测，2026-10）**：现 `echo-adapter-qq` 共 7.8k 行，对 echo-* 的引用按 crate 分五类：

| 依赖 | 实际用到 | 处置 |
|---|---|---|
| `echo-core`（OneBot 事件 / 段 / action，零 echo 依赖） | `Event`/`Segment`/`actions::*` 共 23 处 | **随插件迁出**（迁入插件仓库 `crates/echo-core`）；宿主侧仅 `core/handlers/` 3 个 handler 引用，随 handler 平台中立化一并移除 |
| `echo-server`（反向 WS 服务端，仅依赖 echo-core） | `Server`/`Handler`/`ConnectionTracker` 共 14 处 | **随插件迁出**（迁入插件仓库 `crates/echo-server`）；宿主无其他使用者 |
| `echo-defs::plugin_instance` | `filter::*`（门控 / 名单，957 行）、`ConfigStore`、`ensure_table`、`PluginInstance`/`InstanceManagement` trait（**现已定义于 `echo-defs::adapter`，`echo-defs::plugin_instance` 仅 re-export**）、`IncomingMessage` | filter / ConfigStore 收归插件自有（P1.7c 完成；宿主 `echo-agent` 侧自用副本 P1.9 收尾）；`PluginInstance` / `InstanceManagement` trait 退役为固有方法；领域类型插件自有，线协议转换在 `plugin.rs` 边界（`IncomingMessage`） |
| `echo-defs` | `MessageTarget`/`ChannelType`（15 处）、`media_store::*`（5 处） | 前者 → `MessageTarget`/`ChannelType`（当时叫 `AdapterTarget`/`AdapterChannel`）；**`media_store` 改走宿主 `core_api.store_media` HostCall**（当时服务名叫 `adapter_host`）（两侧均已完成，P1.7c：宿主 `store_media` + 插件 `HostMediaStore`，同机传 path） |
| `echo-context` | `kernel::get_or_init`（缓存 reqwest Client，1 处） | 换 `OnceLock` |

插件仓库最终只依赖 `echo-plugin-sdk` + 传输 crate（`crates/echo-core` / `crates/echo-server`，零 echo-* 依赖）。**宿主侧收尾（P1.9，已完成）**：`echo-adapter-qq`/`echo-core`/`echo-server` 三 crate 与内置 QQ 装配已删除；`core/handlers/`（echo/help/admin）先退役为 `core/src/instance_commands.rs`，**后又整体删除**（2026-10 选项 B：命令前缀与 owner 判定是平台会话语义，回复依赖宿主代发）。`[instances.qq] mode` 历史字段保留解析：`"builtin"` 在加载期告警并按 plugin 处理（内置路径不再存在）。

**迁移映射**：

| 现位置 | 新归属 |
|---|---|
| `echo-adapter-qq`（传输 / NapCat / 过滤 / 事件转换） | **插件仓库** |
| `echo-agent/packages/instance_ops/bridge.rs`（`AgentMessageHook`） | 宿主（保留，通用化） |
| `echo-agent/packages/adapter_qq/commands.rs`（QQ 命令域） | 宿主（转发 `<name>:custom`；getter 读缓存） |
| `crates/echo-adapter-qq/src/tools.rs`（`send_*` 工具 + 出站闸门） | 宿主（泛化 `Arc<dyn Adapter>`） |
| `core/handlers/`（echo / help / admin） | **整体删除**（2026-10，选项 B）：先退役为 `core/src/instance_commands.rs`（平台中立命令域 `/echo` `/help` `/status` `/shutdown`，运行于实例入站 worker、投递 agent 前消费），后**连同该文件一并删除**——命令前缀与 owner 判定都是平台会话语义，且回复依赖「宿主代发」，两者都随去适配器化消失。插件只负责在 `submit_input` metadata 里标注 `sender_role:"owner"`（`QqHandler::annotate_sender_role`），信任边界 = 插件为用户自装的可信代码 |
| `core/instance_deploy`（实例解析 / 端口分配 / compose / 写回） | **留宿主的只有实例解析**（读实例表 + persona 门控自动建档 + 默认 persona 推导，~230 行）。端口分配 / 容器名派生 / compose 渲染 / 端口持久化**全部随插件**（插件 `deploy` 模块）——它们是 NapCat 的部署形状，框架不该持有。persona 写回仍留宿主（那是归属决定） |
| `core.toml [instances.qq]` | 迁移期：宿主读取并组装 `Hello.config`；目标态：`plugins.toml [plugin.config]` + 插件自持久化运行期状态 |
| `echo-protocol` QQ 命令/事件/`InstanceStatus` 字段/clearance 分支 | 宿主（**线协议通用化**：`InvokeInstanceOp` / `InstanceOpResult` / `InstanceState` / `metadata` / 类别判定——见 §实例管理通道） |
| `echo-defs::plugin_instance` 的 `qq_management()` / `QqManagement` / `QqInstanceInfo` | 宿主（**删除**，置换平台中立 `InstanceManagement`：缓存读 + `invoke_custom` 异步写） |
| Panel：QqPanel.vue / QqLoginSection.vue / `qqByAdapter` / protocol.ts QQ 段 / SessionSwitcher 分组 | Panel 通用化改造（两阶段：**先换通道**（P2）**后换 UI**（schema 渲染器）——见 §Panel UI 解耦） |

## Panel UI 解耦（UI 描述符规范）

> 问题：QqPanel.vue（613 行）/ QqLoginSection.vue（148 行）+ `qqByAdapter` 状态族（`state.ts` 的 `qqByAdapter` 一族）+ `protocol.ts` 的 QQ 段都长在 Panel 里。约束：Panel 是**多上游聚合**的浏览器端，插件进程**不能直连**浏览器——一切 UI 数据与动作只能经 Core 管理 WS 转发。解耦 = 「UI 描述符」规范 + Panel 一个通用渲染器，使插件实例 UI 随插件走。

### 方案选型（含否决记录）

| 选项 | 判定 | 理由 |
|---|---|---|
| **schema 驱动** | ✅ 主路径 | 无远程代码（无 RCE 边界）；schema 随各 Core 来天然 `(core, adapter)` scoped；离线零成本；复用 Neumorphism 组件库与 sendCommand / pending / scopedKey 既有模式 |
| 运行期远程 Vue 组件 / Web Components | ❌ 否决 | 插件 JS 在 Panel 上下文执行 → 可触全部 state / WS / token；多上游 = 一个被攻破的插件 UI 暴露**所有上游**管理面；Vue / ui-frame 版本须与 Panel 对齐（与否决 dylib 同构的版本地狱）；资产也无分发通道 |
| iframe + postMessage 沙箱 | ⚠️ 逃生舱 | 安全最好但双端桥成本 + 视觉融合差；schema 表达不了的强交互才用（资产经 Core `/plugin-assets/<plugin>/` 静态路由——echo-web-server 已有 static_files 落点） |
| 构建期 npm 包（UI 随 Panel 发版） | ⚠️ 过渡 | 安全/离线好，但违背「免重编译、独立演进」，多上游下内嵌版本可能与插件错位 |

**关键观察**：QQ 的重管理 UI 天然由 NapCat WebUI 承担（`webui_url` 就是干这个的）——Panel 内 QqPanel 只是轻量管理面（状态/门控/名单/二维码），schema 表达力足够。

### 描述符（插件声明，`Register` 携带）

```jsonc
// InstanceContribution 增字段（当时叫 AdapterContribution）
"ui": {
  "version": 1,
  "panels": [{
    "id": "main", "title": "QQ 插件实例",
    "nodes": [
      { "type": "status",  "bind": "online", "label": "连接" },
      { "type": "text",    "bind": "self_id", "label": "账号" },
      { "type": "link",    "bind": "webui_url", "label": "打开管理页" },
      { "type": "select",  "bind": "gate_mode",
        "options": ["none", "allowlist", "denylist"],
        "action": { "method": "set_gate_mode", "arg": "$value" } },
      { "type": "list_editor", "bind": "allowlist_users", "label": "用户白名单",
        "action": { "method": "update_allowlist" } },
      { "type": "image",   "label": "登录二维码",
        "fetch": { "method": "login_qrcode", "result_key": "png_base64" } },
      { "type": "button",  "label": "刷新", "action": { "method": "refresh" } }
    ]
  }]
}
```

- **节点类型（v1 冻结）**：`status` / `text` / `link` / `select` / `list_editor` / `image` / `button`；**未知节点忽略**（向前兼容）；schema 版本化，渲染器按 version 分支；
- **状态流**：插件 → `HostCall core_api.report_ui_state { instance, state }`（当时服务名 `adapter_host`；**插件当前不发 epoch**——宿主注释明说「无 epoch 不校验」，失效靠宿主侧 generation 惰性判定） → Core 缓存（epoch 失效语义复用 §宿主侧实现）→ `InstanceState` 事件 → Panel 按 `scopedKey(core, instance)` 归并（`qqByAdapter` 泛化为 `adapterUiByKey`——Panel 内部键名，未随宿主改名）；
- **命令流**：渲染器 → `InvokeInstanceOp`（经 `connection.ts` `resolveTargetCore` 既有路由）→ Core → `Invoke("<name>:custom")` → 插件；结果经 `InstanceOpResult` + pending 键配对（复用 sendPending 模式）；
- **通用面板上移**：多实例切换 / 状态行 / **启停重启按钮**（`Start/Stop/RestartAdapter` 本已平台中立）→ Panel 内置**通用插件实例面板**（所有插件实例共享，不属于插件 UI）；插件只贡献 schema 部分；
- **会话分组**：`SessionSwitcher` 的 `platform === 'qq'` 分支（`:51-71`）通用化为「按 platform 字符串分组」，标签由 `InstanceContribution.session_labels: { dm, group }` 声明。

### 分阶段

1. **迁移期（随 P2）**：QqPanel 暂留 Panel，但底层命令/事件全部切 `InvokeInstanceOp` 通用通道——**Core 解耦不被 Panel 阻塞**，线协议先通用化（*历史设计意图*：这一步后来只有 Core 侧完成，Panel 至今仍在旧变体名上，见本节末的部署要求）；
2. **目标态**：schema 渲染器上线（约 200-300 行通用组件），删 QqPanel.vue / QqLoginSection.vue / `qqByAdapter` / protocol.ts QQ 段；
3. **逃生舱**：schema 表达不了时走 iframe 沙箱（`/plugin-assets/`）或平台 WebUI 外链（`link` 节点）。

### QQ 面板功能映射（parity 清单）

| 现功能 | 归属 | 机制 |
|---|---|---|
| 多实例切换 / 状态行 / 启停重启按钮 | **通用面板**（上移） | AdapterList + Start/Stop/RestartAdapter（已中立） |
| 门控模式选择 | 插件 schema | `select` + action |
| 白/黑名单四卡片 | 插件 schema | `list_editor` × 4 |
| 登录状态 + 二维码 PNG | 插件 schema | `status` + `image` + fetch action |
| owner 设置 | 插件 schema | `text` + action |
| 会话列表 QQ 分组 | 通用化 | platform 字符串分组 + `session_labels` |

## 交接与崩溃恢复语义

> 端口占有型实例的热替换**不是**「先切流量后排空」：QQ 插件独占反向 WS 监听端口（`TcpListener::bind`），新实例 `start` 时旧实例未释放端口必 `bind` 失败。本节是验收与实施的语义基准。

**热替换 = 先停后切（交接式）**：

```text
invoke old:stop（释放监听端口，NapCat 断连）→ drain old（等在途完成）→ 进程退出
→ invoke new:start（重新 bind，NapCat 自动重连）→ report_state
```

- **代价**：秒级断连窗口（QQ 平台离线消息兜底，恢复后补投）；**收益**：语义简单、可靠、可测。**验收口径从「会话不中断」改为「自动恢复 ≤Ns 且消息不丢（以平台缓存为界）」**；
- 远期可选：`SO_REUSEPORT` 双实例同端口做真零停机（实验轨，不进首版验收）；
- **崩溃恢复（自动重启路径）**：NapCat 登录态在容器数据卷，插件崩溃不杀容器——重启后重新 `bind` + 重连即可恢复；**门控运行时状态在插件侧落盘**（P1 即要，安全相关：崩溃重启不得让名单 / 模式回退到默认）；
- **强杀窗口**：deadline 强杀后到重连期间的入站消息丢失，由平台离线消息兜底（验收口径同上）；
- **状态三源仲裁**：① 插件进程态（PluginSupervisor，最高优先）→ ② 连接态（`report_state` 上报）→ ③ 宿主缓存；①② 未就绪时缓存不可信（缓存失效挂在重启 / 升级 epoch 上，见宿主侧实现）。

## 实施路线、风险与验收

**路线**：

- **P0 协议与宿主骨架**：**前置修复（先于骨架动工）**——① `InvokeOutcome::Ok.result` 字段 + conformance；② supervisor `HostCall` 回执按来源连接路由（见「版本与兼容」）；③《交接与崩溃恢复语义》定稿（本页 §交接与崩溃恢复语义）。然后：**线协议通用化**（§实例管理通道：`InvokeInstanceOp` / `InstanceOpResult` / `InstanceState` / `metadata` / clearance 类别判定）+ **管理面置换**（`QqManagement` → `InstanceManagement`）+ api / host / sdk 扩展 + conformance 补测（**adapter 套件**：注册 / 收 / 发 / 崩溃 / 交接 / **UI 描述符解析**）；loopback 测试插件实例（非 QQ）跑通「注册 → 列表 → 收 → 发」端到端。**工作项明细见 §P0 工作项明细**；
- **P1 QQ 插件外迁**：仓库拆分、代码迁入；**门控运行时状态落盘**（P1 即要，安全相关）；fake WS 对端 e2e；
- **P2 宿主切换**：配置开关切到远程插件实例；命令域 / 工具 / 处理器迁齐；**Panel 阶段 1**（底层命令/事件切 `InvokeInstanceOp` 通用通道；QqPanel 暂留至阶段 2 删除）——**Core 侧已落地（2026-10-16）；Panel 侧待跟改**——Panel 仍发 `InvokeAdapterOp`、收 `AdapterList` / `AdapterState` / `AdapterOpResult`，与 Core 的 `Instance*` 变体名不兼容（serde 无变体级 alias），该链路当前**双向不通**（见 [协议与数据流](../protocol/protocol.md)「协议名版本窗口」）；**Panel 全量命令清单回归**（实例增删 / 二维码 / 名单 / 门控 / 启停——泛化后逐条验证）；
- **P3 摘除内置路径**：关闭 feature 后 `echo-adapter-qq` 不在二进制；**Panel 阶段 2**（schema 渲染器 + 通用面板 + 挂载切换 + 旧组件/协议段删除全部完成，2026-10-16——vitest 65 文件/541 用例全绿、vue-tsc 零错误、死引用 grep 仅剩有意注释）；QQ 协议变体废弃周期启动（两版本后删除）；自更新流程扩展（插件二进制同步）；文档收口；
- **（叠加 P5）交接演练**：按《交接与崩溃恢复语义》先停后切，验证自动恢复 ≤Ns、NapCat 重连、门控状态不丢（专项用例）。

### P0 工作项明细（管理面置换为核心）

> 产出物 = **一个参考实现**（inproc QQ 插件实例全程经 `InstanceManagement` 运行）+ 协议三件套（`InvokeInstanceOp` / `InstanceOpResult` / `InstanceState`）+ 三项前置修复。**P1 外迁插件对照本参考实现实现同一接口；P2 远程插件实例必须过同一组 conformance**（参考实现先行 = 接口在真实负载下先验证，再外迁）。
>
> **实施进度（2026-10 开工）**：**P0 / P1 全部完成**（2026-10）。P0：**类型层与线协议**零平台词汇（`grep qq_management|QqManagement` 归零、双跑 diff 为空）——这不等于「生产代码零平台名字面量」：组合根仍有 `qq_host` / `qq_instances` 等局部命名（平台化收尾未做完）。P1：宿主侧骨架（`CoreApiService` + `RemoteInstance` + `store_media`）、SDK 脚手架（`InstancePlugin` / `configure` / `serve_stdio(InstanceHandler::new(..))`）、QQ 插件（P1.7a–c，独立仓库）、宿主装配（P1.8，`mode = "plugin"` 拉起 / 入站 worker / 状态转发 / 工具管理面路由，真实 core + 真插件进程 + 真 NapCat 连接冒烟）、命令域迁移与内置清除（P1.9：命令域 `core/src/instance_commands.rs` 后来于 2026-10 随「出站面收窄」**整体删除**、内置装配与 `echo-adapter-qq`/`echo-core`/`echo-server` 三 crate 删除、配置 TOML 表透传）均已落地；全工作区测试绿。后续收尾（2026-10）除末项外均已落地：compose 渲染 / 端口分配随插件（插件 `deploy` 模块）、框架代发与平台命令删除、`PluginKind` wire 名 `instance`、类型层/线协议零平台词汇（但组合根仍有 `qq_host` / `qq_instances` 等局部命名，平台化收尾未做完）。
>
> **泛化收尾轮（2026-10）**：Core 侧插件实例泛化收尾——`instance_ops` 改名 + 回退/广播泛化（`cea95bd`）、`instance_tools` 改名 + 实例/persona 泛化与平台残留清理（`ac2c7b9`）；插件侧 UI 描述符 + `get_state` + 部分载荷修复（`4fdc50c`）；UI 描述符链路与状态推送接线（`be94746` / `8109535` / `7c2517d`，含 W2.4 id 串化、epoch 缓存失效）；Panel 阶段 1 通道切换（`cd93c8c..8ced31c`）与阶段 2 通用渲染器 / 通用面板 / 会话分组（`78811dc`、`2f34377` / `b24a7c0`、`f823972`）。**Panel 阶段 2 收尾已完成（2026-10-16）**：挂载切换 → 旧组件与协议段删除 → 注释清理（`67136a3`/`8a46878`/`2fcfa10`/`d8a7779`，含搁置弹层重构 WIP 整理与启停 pending 结算修复 `3d34ff9`）。**旧协议变体已直接删除（2026-10-16，决策人指示不等 N+2）**：10 命令 / 7 事件 / `InstanceStatus` 三字段 / `legacy.rs` 兼容层 / `id_compat` 双读层全部物理移除；`cargo test --workspace` 全绿。**当时（2026-10-16）泛化收口、无剩余发版项。
>
> ⚠️ **但 2026-10-19 的 `2d46b7d` 让这条结论失效了一半**：Core 把线协议变体从 `Adapter*` 整体改名
> `Instance*`，且**只给字段加了 alias、没给变体名**；Panel 前端仍停在旧变体名上——该链路现在
> 双向不通，属**未完成发版项**。见 [协议与数据流](../protocol/protocol.md)「协议名版本窗口」。

**W1 前置修复（既有缺陷，先于骨架动工）**

- [x] W1.1 `echo-plugin-api`：`InvokeOutcome::Ok` 增补 `#[serde(default, skip_serializing_if = "Value::is_null")] result: Value` + `InvokeOutcome::ok()` 便捷构造；新增用例「结构化结果往返 / 旧帧缺省 null 兼容 / null 上线省略」（含全部消费点迁移）
- [x] W1.2 `echo-plugin-host`：`HostCall` 回执按**来源连接**路由（`dispatch` 增 `source` 参数，三处调用点同步；旧行为经 `current_conn()` 错投）；回归用例 `f_drain_window_hostcall_reply_routes_to_source_connection` 已入库，**负向验证确认可抓旧缺陷**（旧实现下回执错投第 2 代实例）
- [x] W1.3 交接语义定稿（§交接与崩溃恢复语义）——已随本文档完成

**W2 线协议通用化（`echo-protocol`）**

- [x] W2.1 新增 `InvokeInstanceOp { instance, method, payload }` / `InstanceOpResult { instance, method, result, error? }` / `InstanceState { instance, state }`（result 空时线上省略；两个变体名在 `2d46b7d` 由 `Adapter*` 改名，`instance` 字段保留 alias `adapter`）
- [x] W2.2 `InstanceStatus` 增 `metadata: Vec<InstanceMetadataEntry>`（`persona` 保留；`container` / `webui_url` / `onebot_url` 已随 Panel 阶段 2 删除）。**后续收尾（2026-10）把它变成恒空**：平台展示字段改由插件 UI 描述符声明（`InstanceStatus.ui`，字段绑定 `instance.*`），发射端 `emit_instance_list` 直接给空 vec——否则 Core 又要把「容器名 / WebUI 地址」这类平台知识逐键命名、配中文标签，等于把刚搬走的耦合换个字段搬回来
- [x] W2.3 `command_clearance`：`InvokeInstanceOp` 入 Frontend-only（**类别判定**，含未知方法 fail-closed）；逐变体测试改为类别测试（`instance_management_commands_are_frontend_only_by_category`，`command.rs`）
- [~] W2.4 `GroupInfo.group_id` / `FriendInfo.user_id`：`i64` → `String`——**这一项是被「提取」掉了，不是做完了**：两个类型随去适配器化整块移出 `echo-protocol`（现在只在插件仓库 `crates/echo-core/src/model.rs`，`group_id` 仍是 `i64`，工具 schema 也声明 `"type": "integer"`；好友列表直接透传 OneBot JSON，连 `FriendInfo` 都没有同构类型），Core 线协议里已不存在任何平台消息类型，改类型/改名的动机随之消失。当年的障碍（Panel 侧 `QqPanel.vue` 的比较与 toggle 链路依赖 number 形态）也随组件删除而不存在了。

**W3 管理面置换（核心：`QqManagement` → `InstanceManagement`）**

- [x] W3.1 `echo-defs::plugin_instance`：定义平台中立 `InstanceManagement`——`cached_state() -> Option<Value>`（**同步读、零 IO**）+ `async invoke_custom(method, payload) -> Result<Value, AdapterError>`（写）；`PluginInstance` 面新增 `management()` 访问器（`qq_management()` 标迁移期保留，随 W3.5 删除）
- [x] W3.2 `echo-adapter-qq`：`QqAdapter` 实现 `InstanceManagement`——12 method 路由表（`set_gate_mode` / `get_gate_mode` / `update_allowlist` / `update_denylist` / `get_filter_config` / `set_owner` / `get_owner` / `login_status` / `login_qrcode`（base64）/ `get_groups` / `get_friends` / `instance_info`；未知方法可读错误 fail-closed）+ `cached_state` 状态文档；二维码逻辑抽为固有方法（双管理面共用）；契约测试入库（含「双面同源」断言）
- [x] W3.3 `echo-agent`：新增 `InvokeInstanceOp` 处理臂（经 `invoke_adapter_method` → `invoke_custom`；写类方法对齐其余实例）；**全部 9 个旧 QQ 臂也统一改走管理面**（invoke_custom）；旧 `broadcast_to_other_qq_adapters` 已删除（`broadcast_adapter_op_to_others` 替代）
- [x] W3.4 **状态下发单源化**：`emit_adapter_state_events`（`cached_state` 一次性读 → 派生 `InstanceState` + 全部旧 QQ 事件）统一三处发射点（写臂 / 读臂 / `RequestAdapterList`）；`emit_adapter_list` 的实例信息也改读同一状态文档；旧 QQ 事件全部由**单一状态投影**产出
- [x] W3.5 **删除** `QqManagement` / `QqInstanceInfo` / `qq_management()` / re-export（`echo-defs::plugin_instance`）；**8 处产品消费点全部迁移**（命令域 6 + `agent/mod.rs` 实例信息 + `agent/commands.rs` 状态臂）；`grep qq_management|QqManagement` = 0
- [x] W3.6 测试更新：`echo-defs::plugin_instance` mock 断言改 `management()`；`echo-adapter-qq` 管理面契约测试（含「读路径与固有同源」）；命令域测试经 `InvokeInstanceOp`（含双跑）；conformance 用例就绪（custom op 路由 / 未知方法可读错误已在契约测试覆盖）
- **验收**：grep `qq_management` / `QqManagement` 归零（测试名与迁移注释除外）；QQ 管理功能经新通道与旧路径**行为一致**（W4.2 双跑锁定）

**W4 双跑与兼容（迁移期）**

- [x] W4.1 旧 10 命令 / 7 事件（迁移期曾 deprecated 保留，发射源 = `InstanceState` 投影）——**已于 2026-10-16 直接物理删除（决策人指示不等 N+2）**
- [x] W4.2 双跑对照曾入库（`dual_run_old_and_new_gate_commands_produce_equivalent_state`）——随旧变体删除同步移除（新通道测试独立覆盖；历史记录见 `adapter-genericization/`）
- [x] W4.3 升级指南：线协议 breaking 清单 + 直接删除口径——随本轮收口定稿（正文见下「升级指南（W4.3）」）
- [x] W4.4 loopback 端到端（mock 插件实例全链路：`InvokeInstanceOp` → `invoke_custom` → `InstanceOpResult` + `InstanceState`；广播断言。旧事件派生已随变体删除）
- **验收（已达成）**：Panel 现状功能零回归（全仓 63 组测试全绿）；双跑 diff 为空（双跑测试入库）；`cargo test --workspace` 全量绿

### 升级指南（W4.3）

> 发行说明（2026-10-16 定稿：**旧变体已直接删除，不等 N+2**——决策人指示）。

**线协议 breaking（已生效）**

- **旧 10 命令 / 7 事件 / 3 字段已物理删除**：`RequestGroupList` / `RequestFriendList` / `RequestQqLoginStatus` / `RequestQqQrcode` / `UpdateQqAllowlist` / `UpdateQqDenylist` / `RequestQqFilterConfig` / `SetQqGateMode` / `SetQqOwner` / `RequestQqOwner`（命令）、`GroupList` / `QqFilterConfig` / `QqGateMode` / `QqLoginStatus` / `QqQrcode` / `QqOwner` / `FriendList`（事件）、`AdapterStatus.container` / `webui_url` / `onebot_url`（字段）。旧客户端再发这些 = 反序列化失败（`unknown variant`）。
- 管理面唯一通道：`InvokeInstanceOp { instance, method, payload }` → `InstanceOpResult` + `InstanceState`（状态文档，id 一律字符串）。
- 旧字段替代：插件 **UI 描述符** 的 `bind`（如 `instance.webui_url` / `instance.container`）。`InstanceStatus.metadata` 当前**恒为空**（`emit_instance_list` 直接给 `Vec::new()`），该字段只为不改线协议而保留——别指望从这里读到容器名。

**用户侧操作**

1. Core 与 Panel **必须同批部署**——**但这一条目前尚未满足**：Core 侧已删除旧变体（协议里只剩
   `Instance*`），Panel 前端仍在发 `InvokeAdapterOp` / 等 `AdapterList`（`protocol.ts`），
   变体名不兼容且无 alias，**当前该链路双向不通**。补齐 Panel 侧后才能算「已同步」。QQ 插件无需升级。
2. 自建客户端：把管理命令改为 `InvokeInstanceOp { instance, method, payload }`，监听 `InstanceOpResult` / `InstanceState`；方法映射见 §实例管理通道。
3. 配置文件不变（`[instances.qq]` 中 `owner_qq` 等仍为整数，属插件私有）。

**风险与开放问题**：

| # | 问题 | 倾向 |
|---|---|---|
| 1 | 同步 getter / setter（`QqManagement`）vs 远程异步 | getter 缓存 + **epoch 失效**（见 §交接与崩溃恢复语义）；setter 改「乐观缓存更新 + 异步确认 + 失败回滚 + 日志」或命令域 async 化；长期把命令域换到 async 远程接口 |
| 2 | 多实例：1 进程 N 账号 vs N 进程 | 规范两者皆支持；部署建议 N 进程 |
| 3 | 配置双源期（core.toml vs plugins.toml） | 迁移期单源 = core.toml 注入（确定性优先级 + 冲突警告）；目标态 plugins.toml + 插件自持久化运行期状态 |
| 4 | 命令处理链门控前后语义差 | 与现行为对齐（**owner 绕过 gate** 列专项用例），双跑测试锁定 |
| 5 | 崩溃 / 强杀期间消息丢失 | 由平台离线消息兜底（验收以平台缓存为界）；Panel 状态可见 + 自动重连 |
| 6 | `store_media` / `send_media` 大图载荷 | 同机优先 `path`；>4 MiB 强制 path / 先落盘再登记（见安全边界） |
| 7 | 可观测性 | 入站计数 / lag、重启次数、回执失败率进 Panel 状态面；管理命令在插件重启期间返回「插件重启中（第 N 次）」而非裸 `not_ready` |
| 8 | UI schema 表达力陷阱（演进成拙劣 DSL） | 版本化 + 未知节点忽略 + iframe 逃生舱 + 承认「复杂管理走平台 WebUI 外链」 |
| 9 | 三维版本矩阵（Core 协议 × 插件 × Panel 渲染器） | conformance 覆盖 UI 描述符解析；渲染器按 schema version 分支；未知节点忽略 |
| 10 | 线协议 breaking（id 改 String、旧变体删除） | Core/Panel 同发布节奏 + 升级指南写明 + 两版本废弃周期 |

**验收**：

1. QQ 插件实例以**独立仓库二进制**交付，经 `plugins.toml` 装载，收发 / 门控 / 多实例 / 管理全功能；
2. `kill -9` 插件：Core 无感、自动按策略重启、工具调用返回 `plugin_crashed`、Panel 状态可见；**重启后门控运行时状态不丢（专项用例）**；
3. 关闭 feature 后二进制不含 `echo-adapter-qq` 且功能不减；
4. 全量测试 + 双跑对照（内置 vs 远程路径 diff）+ Panel 全量命令回归通过；
5. 通用性：非 QQ 的 loopback 插件实例走同一规范端到端可用；
6. **交接**：先停后切流程自动恢复 ≤Ns、NapCat 自动重连、消息不丢（以平台离线缓存为界）；
7. **零平台残留**：关闭 QQ 插件后全仓 grep 无 `qq` / `onebot` / `napcat` 字面量（文档、迁移兼容层与测试名除外）；`echo-protocol` 无平台词汇、`command_clearance` 为类别判定；
8. **Panel parity**：§QQ 面板功能映射 清单逐项过（通用面板 + schema 渲染器）；QqPanel.vue / QqLoginSection.vue / protocol.ts QQ 段删除且功能不减。
