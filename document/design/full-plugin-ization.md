---
id: full-plugin-ization
title: "全插件化：业务逻辑全部组织为插件（dsh 对齐）"
group: 规划
x: 320
y: 3640
---

# 全插件化：业务逻辑全部组织为插件（dsh 对齐）

> **定位**：目标是把**全部业务逻辑**组织为插件（对齐 dsh / Cordis 的形态：
> 能力以服务键暴露、以插件装载、可替换可禁用），而不是把宿主掏空。
> 读者：内核与插件体系的实施者。**状态：S1–S6 已完成**（2026-11）；剩余部分
> 见 [不搬清单](#不搬清单每条都试过附理由)——每一条都试过，附理由。

## 一、判据（为什么"代码在插件 crate 里"不等于"业务是插件"）

一个能力算不算"插件"，看三件事，缺一不算：

| # | 判据 | 反例（当前仍有） |
|---|---|---|
| 1 | **实现在插件 crate 里**（零 `echo-agent` 依赖） | 已基本达成 |
| 2 | **入口经插件机制注册**（命令域 / 文本块 / 工具 / 服务键），宿主不认识它 | 仅剩自管理面与会话写（见 S5 的 11 条） |
| 4 | **构造不带运行期句柄**（插件自己按键解析宿主能力） | 已达成（S6）：`catalog(version)` |
| 3 | **副作用经插件��制发出**（事件、状态变更、对其他能力的通知） | `run_tool` 里 `call.name == "checklist"` 的特判；`subagent` 的 hook 派发 |

2026-11 实测（口径：应用代码，已排测试）：

```text
              起点（S1 前）        现在（S3 完成）
插件 crate     13,883  39.7%   →   21,959  58.5%
宿主实现       21,103  60.3%   →   15,594  41.5%
```

宿主那 41% 里（15,594 行）：

| 类 | 行数 | 内容 |
|---|---|---|
| 插件注册表与门控 | 2,071 | `plugins.rs` 1,559 + `agent/gating.rs` 512——插件机制本身 + persona 白名单 |
| `Agent` 骨架 | 1,509 | 字段 / 构造 / 事件出口 / 服务句柄 |
| 命令泵 | 1,419 | 31 条命令的内置分支 + 路由（域已归插件的只剩兜底） |
| 会话存储 | 1,033 | `TrunkStore`：事件日志、投影、落盘、压缩落地 |
| 管理面 | 764 | `AgentManager`（人格生命周期） |
| 入站编排 | 1,344 | `inbound.rs` 590 + `instance_ops/bridge.rs` 754 |
| 配置 | 721 | `AgentConfig` / `TeamMember` / 持久化投影 |
| turn 驱动 | 512 | harness（把宿主接到 `echo_runner`） |
| 其余 | ~4,200 | 模型服务 / 提示词服务 / 各种端口实现 / 命令域兜底 |

**没有剩「本该是插件但还不是」的业务逻辑**——见下面的不搬清单。

### 命令域的迁移进度（最直观的读数）

```text
                 S3 前     现在
插件处理          7 条      35 条   ██████████████░░░░░░  66%
宿主兜底         46 条      31 条
文件行数        1967 行    1419 行   （-28%）

（`commands.rs` 只剩 31 条内置分支：14 条 provider/API + 11 条会话/上下文
+ 6 条自管理面。3 条域（shell / skills / tools+instance+workspace+federation）
的 35 条已改走插件注册关系。）
```

**已迁进插件的域**（35 条）：

| 插件 | 命令 | 能力本体 |
|---|---|---|
| `echo-plugin-workspace` | 7 | store + git/文件采集 + 提示词块 |
| `echo-plugin-skills` | 8 | SKILL.md / 目录校验 / `.sources.json` / git 安装 / 列表投影 |
| `echo-plugin-tools` | 13 | shell ×4（+ `shell_*` 工具）· instance ×7（+ `instance_*` 工具 + 清单投影）· tools ×2 |
| `echo-plugin-federation` | 7 | 路由表 / 代理工具 / 聚合器 / 跨机查询 / 出口集合 |

**宿主仍处理的 31 条**（每条都记录了理由，见下面 §不搬清单）：

| 类 | 条数 | 内容 |
|---|---|---|
| provider / API 管理面 | 14 | `SwitchModel` · `SwitchProvider` · `UpdateApiConfig` · `SwitchApi` · `SetSessionApi` · `ClearSessionApi` · `RequestRemoteApiProfiles` · `TestApi` · `QueryApiBalance` · `QueryApiMetrics` · `DeleteApi` · `RenameApi` · `SetSystemPrompt` · `RequestSystemPrompt` |
| 会话 / 上下文 | 11 | `RequestState` · `RequestContext` · `RequestTrunkTimeline` · `RequestTimelineEarlier` · `RequestSessionHistory` · `ClearHistory` · `ArchiveHistory` · `CompactHistory` · `CancelRequestedWork` · `SendMessage` |
| 自管理面（宿主保留） | 7 | `RequestPluginsList` · `TogglePlugin` · `RestartPlugin` · `RequestTeamsList` · `SaveTeam` · `DeleteTeam` · `ToggleTeam` |

## 二、形态约定（dsh 对齐的三条硬规则）

1. **能力 = 服务键**。插件在 `mount` 里 `ctx.provide(&ServiceKey, impl)`，
   宿主与其它插件用 `ctx.service(&key)` 解析。宿主**不持有**该能力的构造知识。
   （范例：`loop` 的 `TurnRunner`、S1 的 `tools.builtin` provider。）
2. **依赖 = 声明**。插件用 `required_services` / `Inject` 声明"没它我不能挂"，
   注册表在跑 mount 闭包**之前**校验；缺失即 `MissingService{available}`，
   闭包根本不执行。
3. **副作用 = 事件**。能力做事引起的事（工具结果、清单变化、会话变更）经
   插件 → 宿主出口（`MountContext::emit`）或宿主事件总线（`on_event`）发出，
   宿主不认识"哪个插件在什么时候该做什么"。

## 三、分阶段（每阶段独立可交付、可断言）

### S1：能力所有权（工具集）——**已完成 2026-11**

工具清单的所有权从宿主搬进 `echo-plugin-tools`：

- 新增 `plugin.rs`：`BuiltinToolProvider`（端口进，配好的 `ToolRegistry` 出）、
  `BuiltinTools` 实现、`core_self_stop_guard`、`manifest`、`plugin()`；
- `mount` 把 provider `provide` 到 `tools.builtin` 服务键，卸载时撤销；
- 宿主 `packages/tools_builtin` 从"工具清单所有者"降级为**端口接线**
  （90 行，其中 40 行是文档与测试）；`register_all` → `build_registry`；
- 组合根不再出现任何内置工具名，也不再写自停护栏正则。

**验收**：`cargo test -p echo-plugin-tools` 65 项全绿（含"每个工具都带包标签"、
"护栏拒 stop 放行 restart"、"挂载提供键 / 卸载撤销键"三项新契约）。

**遗留（S1b）**：

- `list_api_profiles` 仍由宿主注册——它依赖 `ApiProfile` / `api_pool`，
  等这两个类型下沉到 `echo-config` 后跟着进插件；
- `core_agent` 早于插件挂载创建，走"直接构造同一实现"路径；把插件清单块
  提到 agent factory 之前即可只剩解析一条路径。

### S2：工具执行链（`ToolService`）——**已完成 2026-11**

执行链本体搬进机制层 `echo_tool::execute`，宿主只剩四个接缝：

| 接缝 | 宿主实现（`agent/tool_dispatch.rs`） |
|---|---|
| `redact` | `Agent::redact_text_tracked` |
| `on_call` | `BackendEvent::ToolCall` + `SessionEvent::ToolCall` |
| `on_result` | `+ SessionEvent::ToolResult` + **状态广播** |
| `run_orchestration` | `spawn_subagent`（同步发起，不必 async_trait） |
| `tool_arguments` | `__session_id` / `__team_id` 的宿主映射表 |

- `ToolExecutor::run(&dyn ToolExecutionHost, …)`：接缝**按调用传入**而不是
  存字段——宿主实现因此可以借用 `&Agent`（存 `Arc<Agent>` 会成引用环）。
- **`if call.name == "checklist"` 已删除**：`Tool::state_key()` 让工具自己
  声明"我有可见状态"，执行链在结果落盘后统一问一句；宿主只保留
  `state_key → 线上事件变体` 的协议映射表。新增有状态工具 = 一个 trait 方法。
- `agent/tool_exec.rs` 只剩超时口径（用户配置 + 工具 hint，硬上限 600s）。

### S2 原计划：工具执行链（`ToolService`）——wall 级（已落地，保留设计记录）

`run_tool` 一次碰 5 个协作点（`redactor` / `emit` / `trunk` / `tools` /
`session_id`），这是"工具能力归插件"的最后一堵墙。做法：

1. 在 `echo-tool` 定义 `ToolExecutionHost`（5 个方法的最小面：发事件、
   落会话事件、取脱敏器、解析会话上下文）；
2. 执行体（参数预检 → 分派 → 结果脱敏 → 耗时统计）搬进 `echo-plugin-tools`；
3. 宿主 `run_tool` 变成 `resolve("tools.executor").execute(...)`；
4. 顺手消掉 `call.name == "checklist"` 特判——清单变化成为执行器在结果上的
   一个**后置动作**（插件内），宿主不再认识工具名。

**验收**：`grep -rn '"checklist"' source/backend/echo-agent/src/agent/` = 0；
工具执行的全部既有集成测试（含超时 / 取消 / 图片结果 / 远程分流）不改断言通过。

### S3：其余能力归位——**已完成 2026-11**

| 能力 | 做法 | 结果 |
|---|---|---|
| **联邦**（1,698 行） | 解阻：`FederationPorts::command_handler` 的类型 `Fn(&Agent, cmd)` → `Fn(cmd) -> future`（实现只需要事件出口）。整块搬进新 crate `echo-plugin-federation` | 宿主 `packages/federation/` 只剩 re-export |
| **subagent 执行体**（401 行） | 解阻：九个端口 `SubagentExecutionHost`（提示词 / 工具 / provider / 循环参数 / 事件 / 联邦通知 / hook 投递）。执行体搬进 `echo-plugin-subagent::execution` | 宿主 401 → 98 行 |
| **instance 命令域**（429 行） | 解阻：`ApiProfile`… 不，这里的解阻是"依赖只有注册表 + 事件出口"（原本就够）。连同 `InvokeInstanceOp` 一起进 `echo-plugin-tools::instance_ops` | 实例域一个插件 |
| **provider / API** | 解阻：`ApiProfile` 下沉到 `echo_defs::api_profile`（跨边界词汇）。`api_pool`（132 行）+ `list_api_profiles` 工具进插件 | **管理面本身未搬**，见 §不搬清单 |

**逐块记录**：

- **联邦**：`router`（1030）/ `aggregator`（372）/ `remote_tool`（195）/
  `workspace_query`（101）/ `ports`（298）/ `commands`（171）。顺带修一个隐性
  耦合：此前每个 persona 都往同一个进程级 ports 里塞"自己的 emit"，谁的装配
  最后跑谁生效；现在出口在装配期固定（进程级汇聚点经 `OnceLock` 交付）。
- **subagent**：三个搬家助手跟上——`subagent_task_field`（4000 字符上限 +
  标注）、`is_turn_cancelled`（取消 vs 真失败，单点定义在 `echo_defs::llm`）、
  `wire_status`（插件状态 → 线协议词汇）。拒绝路径与正常终态走**同一套**回报
  （漏掉任何一步都会让面板的子任务永远停在"运行中"）。
- **instance**：三条不变量逐条保留（多实例寻址回退 / 失败也回执 / 状态下发
  单源）。`SHARED_WRITE_METHODS` 加了一条测试——它是平台行为契约，漏一行会
  表现为"改了实例 A，B 没跟上"。
- **provider**：`echo_agent::config::ApiProfile` 与 `crate::api_pool::…` 都保留
  为路径别名，调用方零改动。

### S3 续：策略类业务逻辑（2026-11，六轮追加）

命令域搬完后，剩下的是**策略**（纯逻辑，此前住在宿主只因需要几样宿主设施）。
逐块搬走：

| 搬走的东西 | 行数 | 归属 | 解阻点 |
|---|---|---|---|
| 上下文压缩策略（转录渲染 / 分层裁剪 / 摘要调用 / 失败回退） | ~290 | `echo-plugin-loop::compaction` | `CompactGroup` / `CompactPreview` / `CompactionSummary` / `CompactionApplied` 下沉 `echo-session`（压缩契约与 `SessionEvent` 同层） |
| 供应商池管理规则（空字段继承 / 空 key 保留 / 删激活回落 / 改名连带） | ~180 | `echo-plugin-provider-llm::admin` | `ApiProfile` 下沉 `echo_defs`（此前在 `echo-agent::config`） |
| 显示时间线实时投影（哪些事件进面板 / 工具完成态原地合并 / wire 瘦身） | ~400 | `echo-plugin-management::timeline` | `TimelineHost` 三方法（**刻意不给 `timeline_mut`**：那会让插件认识宿主的锁类型） |
| 显示历史派生（事件日志 → 时间线形状 / 水位线） | ~380 | `echo-plugin-management::history` | 派生改成吃 `&[SessionEvent]`（纯函数） |
| 计量与成本估算（余额 / 小时桶 / 定价匹配 / 高峰时段 / 缓存命中） | 716 | `echo-plugin-provider-llm::metrics` | `ModelPrice` 下沉 `echo_defs`；`UsageRecorder` 的 impl 回到同 crate（此前靠孤儿规则勉强成立） |
| 输入信封（`<X>_message_hook>` / `<timer_event>`） | 174 | `echo-defs::input_marker` | 纯词汇——实例桥 / 面板 / 循环 / 显示投影四方共用格式 |
| 工具执行链（参数预检 / 超时口径 / 成对落盘 / 出口脱敏） | ~250 | `echo-tool::execute` | `ToolExecutionHost` 四接缝；**`Tool::state_key()` 取代 `if call.name == "checklist"`** |
| subagent 执行体（受理 → 上下文 → spawn → 终态三分支 → hook） | ~280 | `echo-plugin-subagent::execution` | `SubagentExecutionHost` 九端口（含 `deliver_hook`：插件只表达意图，入站流程留宿主） |
| 联邦执行层（路由表 / 代理工具 / 聚合器 / 跨机查询 / 出口集合） | 1,698 | `echo-plugin-federation`（新 crate） | `command_handler` 的类型去掉 `&Agent`（实现只需要事件出口） |

**这一轮的方法论**：每次搬迁都先找「**唯一的宿主依赖**」并消掉它——不是
"给插件一个 `Arc<Agent>`"。9 次里有 6 次的解阻点是**把一个跨边界类型下沉到
定义层**（`ApiProfile` / `ModelPrice` / 压缩四词汇 / 输入信封）。

### S6：插件零捕获 ——**已完成 2026-11**

用户要求"完全插件化"。这一步回答的是其中一个最具体的问题：**组合根还知不
知道"哪个插件需要宿主的什么"？**

以前知道。`catalog(version, hosts: &Hosts)`，`Hosts` 有 13 个字段，全是组合
根手里的运行期句柄：

```text
persona 管理器 · 事件出口 · 联邦端口集合 · 实例句柄 · 实例接线/运行状态位
平台名 · Panel 通道桥 · 管理面地址 · 管理面令牌 · 合并模式联邦句柄 · 外部条目
```

后果不是"main 太长"，而是三条：

1. 新增一个插件 = 改三处（目录、参数组装、门控闭包）；
2. **插件自己反而不声明它需要什么** —— 依赖知识住在组合根；
3. 6 个插件各收一个语义完全相同的 `gate: impl Fn(bool)`。

现在：

```rust
pub fn catalog(version: &str) -> anyhow::Result<Vec<Arc<dyn Plugin>>>
```

三层各就各位，每层只回答自己那个问题：

```text
  host_services::register(ctx, wiring)   ← 宿主：我有什么能力，按什么键
                ↓  （Context 服务表）
  插件 mount: mctx.require(&KEY)          ← 插件：我需要什么（缺了就挂不上）
                ↓
  catalog(version)                        ← 目录：这个二进制里有什么
```

框架侧为此补了两件（v0.11.0 / v0.11.1）：`MountContext::service` /
`require` / `context`，以及 `ServiceSlot<T>`（mount 时解析、处理器里取用、
卸载自动清空）。

**门控从 6 份闭包收成 1 处**：挂载/卸载事件 → 逐 persona 重算。安全性由既
有语义保证（`reapply_plugin_gating` 内部再算 `persona_allows_plugin`），所以
"挂上 = 全局可用"不会突破白名单；新增插件（含组合根不认识的外部插件）自动
纳入。

**配置驱动的那两项刻意留在目录之外**：平台实例插件（id 由 `[instances.<平台>]`
派生）与 `plugins.toml` 的外部条目——它们的"存在"取决于这份部署的配置，不
是这个二进制里有什么。由调用点在目录之后追加。

#### 顺带修掉的两个真 bug

| 现象 | 根因 |
|---|---|
| 管理面 WS 起监听后**立刻被卸载**，面板连不上 | 服务是 RAII 的（`Disposer` 落下即注销）；注册句柄放在内层 block 里，block 结束服务全注销 → 注册表 reconcile 把刚挂上的插件级联卸载 |
| 第一次跑守卫就红 | `loop` 插件声明硬依赖 `llm`，而那个键由组合根更早注册，不在 `host_services` 的清单里 |

前者是真机冒烟抓到的，后者是新增守卫测试抓到的。第二个的修法不是"补个例
外"，而是让 `provided_keys()` 诚实列出**三类来源**（本层注册 / 组合根早期
注册 / 插件自 provide）。

#### 同轮归位的两处能力

| 搬走 | 去哪 | 为什么它算能力 |
|---|---|---|
| `core/instance_tools.rs`（643 行） | `echo-plugin-tools::instance_bridge` | 插件实例能力的**工具面**（合并 / 寻址 / 出站闸门）。它住在组合根的理由（工具要实现 `echo_agent::Tool`）早已过期——trait 下文到 `echo-tool` 两年了 |
| `fetch_balance` 等（~110 行） | `echo-plugin-provider-llm::balance` | 供应商的 HTTP 知识（端点归一是真实故障修出来的、响应字段是供应商专有） |

两处都牵出词汇下沉：`OutboundPolicy` / `sensitive_local_path` → 
`echo-defs::sanitize`（宿主装配与工具插件共用），`ProviderConfig` 投影
（宿主 7 行适配器）。

#### 数字

```text
builtin_plugins.rs     676 → 336 行   （-50%，只剩目录 + 三项守卫）
core/instance_tools.rs 643 →  13 行   （re-export）
core/security.rs       390 → 324 行   （两个词下沉）
api_admin.rs         1,257 → 1,167 行 （余额 HTTP 搬走）
插件 crate           +848 行          （instance_bridge 647 + balance 201）
7 个插件             plugin(version, host…, gate) → plugin(version)
```

#### 可执行守卫（防止回退）

`catalog_shape_tests` 四条：

| 守卫 | 抓住什么 |
|---|---|
| `every_declared_dependency_is_registered_by_the_host` | 插件声明了宿主没注册的键 → 启动起不来 |
| `no_command_is_declared_twice_across_the_catalog` | 两插件抢同一命令 → 先挂的静默赢 |
| `catalog_ids_are_well_formed` | 幽灵 id（`echo-agent.instance.`） |
| `static_catalog_has_no_deployment_specific_entries` | 配置驱动项混进静态目录 |

加上各插件自己的"声明 ⇒ 认领"测试，"插件声明的东西一定可达"这条不变量在
两个方向上都有人守。

### S7：工具按需取上下文 ——**已完成 2026-11**

上一轮（S6）结束时还剩两处宿主持有工具名：`spawn_subagent`（编排特判 +
一条并行异步通道）与 `checklist`（参数注入 + 状态事件映射表）。这一轮把
根因和表现一起拆掉。

#### 根因

**工具拿不到调用上下文**。会话 / 分支 / 归属人格 / 取消信号都在宿主手里，
而 `Tool::execute(&self, arguments: Value)` 只有一个 JSON。于是宿主只能：

- 往参数里塞 `__session_id` / `__team_id`（魔术键，模型可见面的污染）；
- 需要取消信号的工具（子代理）另开一条通道，因为参数里放不下令牌。

#### 修法：把上下文变成可选入参

| 词汇 | 在哪 | 作用 |
|---|---|---|
| `ToolContext` | `echo-defs::tool` | `session_id` / `branch_id` / `team_id` / `cancel` |
| `Cancellation`（trait） | `echo-defs::tool` | 「能问是否已取消 / 能等它取消」——**不绑定** `tokio_util` |
| `Tool::execute_in(&self, cx, args)` | `echo-defs::tool` | 默认转 `execute_rich`；需要的工具覆写 |
| `ToolRegistry::execute_rich_in` | `echo-tool` | 执行链的上下文入口 |
| `ToolRegistry::register_shared` | `echo-tool` | 注册到**注册表寿命**（无 Disposer） |

不需要上下文的工具一行都不用改（默认实现丢掉它）。

#### 删掉的四张表

| 表 | 曾经 | 现在 |
|---|---|---|
| 参数注入 | `match tool { "checklist" => __session_id, "shell_*" => __team_id }` | 工具覆写 `execute_in` |
| 编排分派 | `if call.name == "spawn_subagent"` → 另一条通道 | 它就是普通工具 |
| 状态广播 | `match state_key { "checklist" => ChecklistUpdated }` | 绑定方声明（插件提供 `all_state_event_bindings`） |
| 门控 | `allows_dynamic_tool` 里按名字判断插件装没装 | 注册表 + 包标签（`set_package_enabled`） |

#### 顺带删掉两条平行路径

`echo-loop` 的 `SubagentToolHooks`（`is_async_tool` / `execute_async` /
`extra_tool_definitions`）整组删除。`spawn_subagent` 曾在此**重新实现**了
一遍"记 ToolCall / 记 ToolResult / 受理"——与注册表路径重复的两份逻辑。

工具 schema 因此只剩**一个来源**（注册表）：`extra_tool_definitions` 造成
的同名重复曾让 API 直接拒绝整个请求（"Tool names must be unique"）。

#### 真实踩到的坑（写进注释与测试）

注册是 **RAII** 的：装配期用 `register_reversible` 却不保存返回值，
`Disposer` 在函数返回时 drop，工具**当场消失**——表现是"注册了但模型看不到
schema"。`register_shared` 存在的唯一理由就是这个。

#### 数字

```text
tool_dispatch.rs   3 处工具名 match → 0（只剩注释）
turn.rs            少 120 行并行通道
gating.rs          少 1 条名字分支
echo-loop          少 ~35 行死接口
生产代码里工具名字面量：0（grep 只剩注释）
```

**结论**：`crate::subagent::SPAWN_SUBAGENT_TOOL` 这类常量现在只出现在
"工具自己的实现"与"注册它的那一行"——这正是插件该有的形态。

### S8：远程面与沙箱范围由工具声明 ——**已完成 2026-11**

S7 把宿主里的工具名表清到了 0（生产代码）。这一轮清的是**插件侧**：
联邦里还留着两份"哪些工具能远程"的名单，以及一份"哪些工具碰文件系统"
的名单。三份都是同一类错误——**别人的知识被写在了自己家里**。

#### 三张表

| 表 | 位置 | 曾经 | 现在 |
|---|---|---|---|
| 握手宣告 | 组合根 `main.rs` | 写死 6 个工具名 | 空集 + `set_tool_reporter`，握手现取注册表 |
| 代理候选集 | 联邦插件 `router.rs` | `const SUPPORTED: [&str; 6]` 与上表求交集 | 镜像对端 caps（去重保序） |
| 沙箱范围 | 联邦插件 `router.rs` | `const FILE_TOOLS: [&str; 5]` | `Tool::path_arguments()` 声明 |

前两张表**同一个意思写了两遍**（在两个 crate）。后果不是"多写了几行"：

- 插件新加的工具**永远**进不了远程面（对端看不到，也不会有人报错）；
- 任一工具改名 → 静默掉出远程面；
- 沙箱那张更糟：插件新加的文件工具**不受**工作区根约束（获授
  `allow_tools=["*"]` 的 peer 能读任意绝对路径）——这是真漏洞，不是风格问题。

#### 两个新词汇（都在定义层）

| 词汇 | 语义 | 默认 |
|---|---|---|
| `Tool::remote_capable()` | "别的节点可以直接调我" | `false`（fail-closed） |
| `Tool::path_arguments()` | "我的哪些参数是文件系统路径" | `&[]` |

默认值的选择是这次改动的核心判断：`false` 意味着**不声明就不可远程**。
反过来（默认 `true`）会让每个新工具自动暴露给所有 peer。

#### 顺带补的一条准入闸门

`handle_invoke` 此前只查 peer 策略（`allow_tools`）就执行，`["*"]` 是配对
互信的默认档——**任何工具名都能穿透**，包括 `workspace` / `instance_stop` /
`spawn_subagent` 这类会改宿主状态、只该本地调的管理工具。现在两道闸门：

```text
注册表里没有           → FedError::UnknownCall（明确拒绝，不再静默失败）
有但未声明 remote_capable → FedError::Forbidden（即使 allow_tools = ["*"]）
```

#### 真机踩到的坑：顺序即契约

改完第一次双节点冒烟，A 看到的 B 是 `tools=[]`，B 看到的 A 是 6 个工具。
原因不是逻辑错，是**启动顺序**：`Federation::run`（监听 + 连出）在
`main.rs` 里比 supervisor 早 900 行，工具注册表那时还不存在。而对端拿到空
caps 后**不会重问**——远程工具缺失一直持续到下次断线重连（两节点同时重启
时稳定复现，正是最常见的场景）。

修法两层：① `run()` 延后到 `set_tool_reporter` 之后启动（监听晚起几百毫秒，
换掉一个静默的半残远程面）；② `hello()` 在 reporter 尚未注入时显式
`warn!`，把"顺序错了"从静默变成可观测。

#### 数字

```text
生产代码里"远程工具名"字面量：  12（两处 × 6）→ 0
生产代码里"文件工具名"字面量：  5 → 0
新增 trait 方法：              2（都有 fail-closed 默认实现）
包装器 RemoteAwareTool 补转发：6 条声明（此前只转发 3 条）
```

**教训（写进注释）**：包装型工具漏转发一条声明 = 被包的工具静默失能，而
Rust 的默认实现让漏转发**编译通过**。`declarations_survive_wrapping` 测试
逐个断言转发到位——新增声明方法时它会红。

### S10：persona 作用域（对齐 `ctx.fork()`）——**已完成 2026-11**

S9 之前的形状是**星形单层**：10 个插件挂在一张进程级注册表上，共享一份
宿主服务表。能力确实归插件了，但**"作用于谁"没有答案**——四个
`Agent*CommandHost`（shell / tools / skills / provider）全都绑到进程级核心
服务代理（`core_agent`，`team_id = None`），于是：

- `ToggleTool` 改的是 core_agent 的工具表，再靠一段"逐 persona 重算"的
  fan-out 补回来；
- `SwitchModel` 改的是 core_agent 的模型配置；
- 结果对，但那是**巧合**：它依赖"fan-out 恰好覆盖了所有该改的地方"。

#### 做成什么：作用域树

```text
root scope   ← Ctx = 宿主服务表（进程级：实例 / 联邦端口 / 管理面 / 事件…）
├── persona "base"    ← Ctx::child()（父链继承、同名键遮蔽）
│                       provide: SHELL_HOST · TOOLS_HOST · SKILLS_HOST · PROVIDER_HOST
│                       mount:   kind ∈ {tool, skill, provider} 的插件（**全新实例**）
├── persona "Alice"   ← 同构，适配器绑 Alice 的 Agent
└── persona …         ← 按需建立（首次有人格命令时）
```

命令分发变成 `dispatch_command_for(&agent, cmd)`：先问该人格的作用域，
落空再问进程级。于是"执行者 = 归属者"由**作用域**保证，而不是由谁恰好持有
哪个 `Arc` 决定。

#### 三个必须讲清的技术点

**① 为什么必须是目录工厂，不是目录。** 插件实例内部持有 `ServiceSlot`
（`mount` 时绑定宿主适配器）。同一个 `Arc<dyn Plugin>` 挂进两层会让两层的槽
互相覆盖——第一个人格的适配器被第二个人格顶掉，**全程零报错**。
`persona_scoped_plugins_are_fresh_instances` 守着这条。

**② 为什么存 `Weak<Agent>`。** `Agent → plugins.host → scope → Agent` 是环；
persona 重启后旧 `Agent` 永远 drop 不掉。用 `Weak::ptr_eq` 判"这层过期了吗"：
同一 team_id 换了 Agent 就重建，`persona_scope_is_rebuilt_when_the_agent_changes`
守着这条。

**③ 哪类插件进人格作用域。** 由 manifest 的 `kind` 决定（`tool` / `skill` /
`provider`）。这不是"宿主认识插件名"的复发：`kind` 本来就是宿主的分类词汇
（框架文档明确它"开放标签、框架从不按它分支"），而"哪类插件该按人格隔离"
是**宿主策略**。判据是"它的宿主适配器是不是 per-persona 的"。
`loop` / `federation` / `management` / `orchestration` / `instance` 是进程级
单例（监听端口、链路表、updater 单元），挂 N 份会真的开 N 个监听。

#### 顺带修掉 `llm` 键的 ownership 漂移

`echo-plugin-provider-llm` 里曾有个 `SERVICE_KEY = "llm"`，文档写"本插件贡献
的服务键"——**它并不提供**这个键（提供的是命令域接缝 `PROVIDER_HOST`）。
真正的注册方是组合根，消费方是 `loop`。三处各写一遍字面量，这次恰好一致。

现在键在 `echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`（**词汇归定义层**）：
注册方、消费方、声明方都引用它。`scripts/deps-policy.txt` 不允许 `loop` 依赖
provider 插件，所以定义层是唯一合法位置——约束本身就指明了答案。

守卫 `llm_service_key_has_exactly_one_owner` 四条断言：宿主声明提供它 /
loop 声明依赖它 / 插件侧别名指向定义层 / 源码里不再出现那两种历史形状
（扫描非注释行，跳过定义层自己）。

#### 边界（诚实记录）

- **命令路由仍按协议分**：`ToggleTool` / `ToggleSkill` / `SwitchModel` 等
  不带 `team_id`，按设计是进程级语义（持久化到 `[agent]`、逐人格重算）。
  它们落在根作用域——**不是**因为作用域没生效，而是协议就是这么定的。
  要按人格切这些命令得改协议 + Panel。
- **人格作用域里的插件仍然只有 5 个**：其余 5 个是进程级单例，本来就该一份。

### S11：联邦命令域进插件 ——**已完成 2026-11（第①②步）**

S10 之后联邦还剩一处"声明在插件、实现在宿主"：`echo-plugin-federation`
声明处理 5 条命令，而真正的处理在 `core/federation_rt.rs`
（`handle_federation_command`，约 300 行）。于是**归属是假的**——读插件读得出
"我管这几条"，读宿主才看得到"怎么管"。

#### 关键动作不是搬代码，是切规则与状态

| 内容 | 搬到哪 | 判据 |
|---|---|---|
| "token 不能为空" / "不能与管理面令牌相同" / "同一 token 只能属于一个 peer" / "URL 必须 `ws://host:port`" | 插件 `admin.rs`（**纯函数**） | 把 peer 表换成数据库也照样成立 |
| "邀请串 = listen + 新 token" / "未配对占位 1 小时后清扫" | 插件 `admin.rs` | 邀请语义属于联邦，不属于 Core |
| `invite_host` / `advertise_host`（地址推导） | 插件 `admin.rs`（**纯函数**，无依赖） | 纯计算 |
| `Federation::add_peer` / 配置写回 / `drop_link` / 反查表清理 / `router.drop_peer` | 宿主 `core/federation_admin_host.rs`（`impl FederationAdminHost`） | 真的改内存与磁盘 |

结果：**规则可单测（9 条纯函数测试），宿主实现短到一眼读完**。这与
`echo-plugin-provider-llm::admin`（`PoolState` 进 → `PoolEffect` 出）是同一个
形状——凡是"用户点了面板一个按钮"的命令都长这样。

#### 出口类型的变化

```text
S3:  Option<Arc<dyn Fn(&Agent, BackendCommand) -> future>>   ← 整条处理在宿主
S3+: Option<Arc<dyn Fn(BackendCommand) -> future>>           ← 去掉 Agent
S11: Option<Arc<dyn FederationAdminHost>>                    ← 每个原子动作一个方法
```

`FederationAdminHost` 只有一个实现者（组合根），9 个方法。

#### 数字

```text
core/federation_rt.rs         1801 → 1371 行（-430，含迁走的测试）
core/federation_admin_host.rs   +257（新增，impl 一个 trait）
plugin/admin.rs                 +477（规则 + 命令域 + 9 条纯函数测试）
宿主里联邦命令的 match 臂        300 → 0
```

#### 回归守卫

`plugin_dependency_direction.rs` 新增了一条**源码扫描**断言：组合根里不得出现
`BackendCommand::<SaveFederationPeer\|DeleteFederationPeer\|RequestFederationStatus\|RequestFederationInvite\|MigrateSession>`
的匹配臂，且插件必须真的认得这 5 条。双向：既防"实现回流"，也防"只删不搬"
（命令没人处理 = 面板点了没反应）。

这类漂移只有源码能抓——两条路径都编译通过、测试也全绿，只是实现长在错的
一边。

#### 真机验证（6 条链路）

```text
① RequestFederationInvite   → echofed://127.0.0.1:39911?name=fedtest#db25b460…
② SaveFederationPeer 缺 token → 「联邦 peer token 不能为空」        （插件判）
③ SaveFederationPeer 坏 URL   → 「必须以 ws:// 或 wss:// 开头」      （插件判）
④ SaveFederationPeer 合法     → FederationStatus 含新 peer          （宿主落状态）
⑤ 同 token 另一 peer          → 「已被 peer「peer-x」使用」          （插件判）
⑥ DeleteFederationPeer        → peers 列表少一项                    （宿主删）
   配置落盘：[federation.peers.saved-peer] token/url/allow_tools 全在
```

#### 还没做的第③步

`federation_router_pump`（LinkEvent 消费循环，约 400 行）仍在组合根。它要
"全部 persona"视角（链路 Up → 给每个 persona 注册代理工具），而 S10 的人格
作用域已经提供了新解法——让 `kind = "federation"` 也进人格层，每人格自己装
代理工具，泵就不再需要遍历 supervisor。

**没做**的理由：它是唯一跨机子系统，内含两处事故驱动逻辑（邀请配对提升、
地址漂移自愈，文档各有 2026-10 报障记录）；搬错的代价是两台机器互相连不上，
而收益只是"形式上更完整"。等有真实的"多节点拓扑变化"需求再做。

### S12：插件之间零依赖 ——**已完成 2026-11**

`scripts/deps-policy.txt` 此前只禁"插件依赖业务 crate `echo-agent`"，
**插件之间的依赖一直是允许的**。实测 4 处：

```text
federation → subagent    impl RemoteDelegationSink for AggregatorDelegationSink
federation → workspace   use {QueryFuture, RemoteWorkspaceQuery}
federation → tools       type RemoteInvoker = echo_plugin_tools::RemoteInvoker
tools      → provider    use api_pool::{PoolSnapshot, read_snapshot}
```

后果不是"多一条边"，是**替换不可能**：想换掉 federation 就必须先接受 subagent
的词汇。它们一直能编译、一直没人拦——前三轮的每次修一处（`llm` 键、
`FederationAdminHost`）都是手动发现、手动修的。

#### 修法：trait/struct 长在错的一边 → 下沉定义层

| 词汇 | 新位置 | 为什么它不属于任何一方 |
|---|---|---|
| `RemoteDelegationSink` | `defs::subagent` | 声明方 subagent / 实现方 federation |
| `RemoteWorkspaceQuery` + `QueryFuture` | `defs::workspace` | 声明方 workspace / 实现方宿主 |
| `RemoteInvoker` | `defs::remote_tool` | 声明方 tools / 实现方联邦 |
| `PoolSnapshot` + `parse_pool` | `defs::api_profile` | "用户在配置里写了什么" |

第 4 处不是纯移动：`read_snapshot(store: &ConfigStore)` 拆成两半——**解析**
（吃 `toml::Value`，进定义层）+ **读文件**（留插件/宿主）。于是
`list_api_profiles` 工具不再需要认识 provider 插件。原位置全部保留 `pub use`
re-export，下游路径不变。

#### 门禁（两层，互为冗余）

1. `deps-policy.txt` 新增第 4 条纪律 + 收紧两行白名单；
2. `plugins_do_not_depend_on_other_plugins` 扫每个插件 `Cargo.toml`，点名
   "哪个插件依赖了哪个插件 + 该怎么修"。

例外只有机制 crate（`echo-plugin` / `-api` / `-host` / `-sdk` / `-loader`）：
它们是"插件怎么装"，不是"某个插件"。

**反向验证**：临时给 loop 加一条 `echo-plugin-tools` 依赖，两道门禁同时红，
然后还原。

```text
插件间 echo-plugin-* 依赖：4 → 0
剩余 3 条全是机制 crate（adapter/example/loop → api/host/sdk）
```

### S13：新人格拿不到远程工具 ——**已修 2026-11**

一个**静默缺口**，不是重构需求：`register_remote_tools` 只有一个调用点
（`LinkEvent::Up`）。于是链路已通之后新建的人格：

- 面板上看得见它（TeamsList 里有）；
- 模型看不见 `peer__read_file`（注册表里没有）；
- **没有任何报错**——要等下次断线重连才补上。

#### 修法：拆成"按一个 agent 装" + 两个入口

```text
register_remote_tools_for_agent(rt, agent, info, peer_name) -> Vec<Disposer>
   ├── register_remote_tools(rt, personas, …)      链路 Up → 遍历各装一次
   └── adopt_active_links(rt, agent, persona_id)   人格新建 → 只给自己装
```

keepers 从 `node_id → Vec<Disposer>` 改成 `node_id → persona → Vec<Disposer>`：
"只给一个人格注册"要求桶能按人格分辨（此前只能整 node 重注册）。

组合根的 agent factory 末尾 `tokio::spawn` 一次 `adopt_active_links`——factory
是同步闭包，而注册要读 `PeerInfo`；"人格刚建好"到"它跑第一个 turn"之间隔着
面板操作，几十毫秒窗口不可感知。

#### 顺带修掉的泄漏

`Disposer` 内部持有 `Arc<ToolRegistry>`（RAII 注销要用）。人格被删除后 keepers
仍留着它的句柄 → **整个注册表被一条不会再被读的记录吊着**，反复新建/删除会
线性累积。新增 `prune_deleted_personas`，在"人格新建"与"链路 Up"两个必然发生
的时点顺手回收（不另起定时器）。

#### 真机验证（两台节点 + 运行期新建人格）

```text
16:xx:xx link up peer=node-… name=b tools=[bash, edit_file, list_files,
                                              read_file, search_code, write_file]
16:xx:xx remote proxy tools registered peer=node-… name=b
         package=echo-agent.federation.b tools=6
--- SaveTeam {id:"Zoe"} （运行期新建，此前不存在）---
16:xx:xx agent Zoe saved
16:xx:xx persona 新建：补装已在线的远程代理工具 persona=Zoe peer=node-…
         package=echo-agent.federation.b tools=6
         installed=["b__bash", "b__edit_file", "b__list_files",
                    "b__read_file", "b__search_code", "b__write_file"]
```

3 条新单测：只填目标人格（bob 不受影响）/ 包标签正确 / Disposer drop 即注销 /
无 peer 时是廉价 no-op。

## 不搬清单（每条都试过，附理由）

诚实记录"没做"比做完的部分更重要——否则下一个人会把同样的时间再花一遍。

| 模块 | 行数 | 为什么不搬 |
|---|---|---|
| **API 管理面**（`agent/api_admin.rs`） | 1,243 | **池管理规则已全部搬走**（`echo-plugin-provider-llm::admin`：继承 / 保留密钥 / 删激活回落 / 改名连带）。留在宿主的是**编排**：读 `AgentConfig` → 调规则 → 写回 → 落盘 → 重建 provider → 广播 → 重绑会话覆盖 → 传播到其它人格。这些步骤的每一步都要 `AgentConfig` / provider 重建 / 事件出口——把 `AgentConfig` 整体下沉等于搬整个配置层，而没人会替换"agent 的配置结构" |
| **turn 驱动**（`agent/turn.rs`） | 512 | 它是 **harness**：把宿主的提示词 / 工具执行器 / 流式转发 / 事件翻译接到 `echo_loop::TurnRunner`。引用 13+ 个宿主协作点，造出来的 seam 就是 `Agent` 本身——那是**最坏的抽象**（seam 变影子）。它存在的意义就是"接线"，不是能力 |
| **instance 入站桥**（`instance_ops/bridge.rs`） | 754 | 名字叫 bridge，实质是**入站入口**（会话键 / 单并行模式展示会话 / 事件 / turn 启动 / 脱敏）。核心入站编排，不是 adapter 能力；adapter 能力早已出进程（`echo-plugin-adapter` + 独立仓库 `echo-adapter-qq`） |
| **会话 / trunk**（`session/`） | 2,119 | 事件日志唯一真源 + 投影 + 压缩编排。**没有替换需求**（谁会换掉会话存储？），且 `s4-agent-decomposition.md` 已论证过服务化的收益/成本 |
| **`Agent` 本身** | 1,539 | 对应 Cordis 的 `ctx.fork()` **作用域**，不是插件。门面化（字段 30 → 8）可以做，但把它做成插件会得到一个没人会替换的插件 + 一层间接 |
| **插件注册表 / 面板协议 / 配置 / 时间线 / 指标** | ~5,000 | 宿主骨架。插件化它们等于把宿主变空壳 |

**判据**：能不能回答"谁会替换它"。能回答的是能力（搬）；不能回答的是编排
或骨架（留）。这条判据在 2026-11 的六轮迁移里没有一次失效。

### S4：会话与 turn（编排骨架的边界）

`session/`（2,119）与 `turn.rs`（512）是**编排**而非能力，判断标准是"有没有
替换需求"：

- `turn.rs` → 搬进 `echo-plugin-loop`（驱动是循环策略的职责），宿主只留
  事件 → 面板的翻译；
- `SessionService`：以服务键暴露 `SessionStore`（**不**出进程），宿主解析；
  这是 `document/design/s4-agent-decomposition.md` 里被"暂缓"的那一项，
  全插件化的口径下它不再可选——但**服务化**不等于出进程。

### S4.5：插件机制三层合并（2026-11，已完成）

用户问「能不能把 2（宿主适配层）和 3（内置插件身份）合并到 1（框架机制）」。
查清后答案是**分半**：

**3 早就合并了**。内置插件是 `Arc<dyn Plugin>`，与子进程插件、`example`
在同一个 `PluginRegistry`、同一份 `mount_plan`、同一套 `mount` 语义。
证据是启动日志那一行 `plugins mounted`——12 个 id 一视同仁。

**2 上收到框架了**（框架 v0.10.0，本轮新增 + push + tag）。宿主
`PluginHost` 曾自己维护四样与领域无关的状态：缓存的 `MountContext`、事件
订阅兴趣集、投递重入保护、投递计数。四者上收到 `PluginRegistry` 之后：

| 宿主删掉 | 框架新增 |
|---|---|
| `mount_ctx: Mutex<Option<MountContext>>` | `set_mount_ctx` / `mount_ctx` / `enable` / `restart_cached` / `reconcile_cached` |
| `event_interest: RwLock<EventInterest>` + 影子结构 | `has_event_subscribers` / `wants_event` / `refresh_event_interest`（**注册表在挂/卸/启停后自己刷**） |
| `delivering: AtomicBool` + `Reset` 守卫 | `deliver_event_guarded` |
| `event_delivered` / `event_failed` | `event_counters` |

`PluginHost` 1,559 → 1,496 行，文档写清了它只做三件本项目的语义：
`plugins.toml` 计划、`BackendEvent` 翻译、子进程控制分派。

**判据**写成一句话：「框架知道这件事吗？」知道（生命周期 / 事件 / 兴趣集）
就上收；不知道（`plugins.toml` 长什么样、`BackendEvent` 是什么、子进程怎么
起）就留。这比"宿主包一层更安全"务实——包着只会让两边都有半份实现的可能。

**同时把 3 的装载代码归位**：`main.rs` 的八个「实化 N」段（378 行）搬进
`source/core/src/builtin_plugins.rs`，形态变成 dsh 的样子：

```text
  builtin_plugins::catalog()   ← 代码里的插件目录（构造 + mount）
             ↓
  plugins.toml 的 builtin 行   ← 配置：顺序、启用状态、每插件的 config
             ↓
  PluginHost::mount_plan_when_ready
```

`main.rs` 2,400 → 2,006 行。

**没做的两件，附理由**：

1. **子进程运行态合并进 `descriptors()` 未上收**：框架的 `PluginDescriptor`
   有 `state` / `restarts` / `generation` / `error` 四个字段，但**值只有本
   项目的 supervisor 知道**；上收会把"生成清单时读一下"变成"每次状态变化
   都要推送"，反而更复杂。留 24 行换一个更简单的数据流。
2. **门控壳未消除**：`builtin_plugins` 里 8 个条目的 `mount` 大多只做
   `reapply_plugin_gating`，真实实现已在各插件 crate。壳留下的原因是**构造
   参数是宿主的东西**（哪个人格 / 哪个端口 / 哪个事件出口）。要彻底消除它们
   需要框架提供「插件自己声明构造参数」的能力（factory 注册表）——下一步。

## S5：命令泵收口——**已完成 2026-11（除"不搬清单"三项）**

口径：每个域一条"插件未启用"兜底，宿主不再有该域的业务分支。

```text
                       S5 前            现在
commands.rs 行数       1,967          1,051   （-47%）
插件认领的命令            35             53     （provider/api 14 → 13，会话视图 5）
宿主实处理的分支          31             11
```

**本轮搬走的两域**：

| 域 | 条数 | 归属插件 | 宿主接缝 |
|---|---|---|---|
| provider / API 管理面 | 13 | `echo-plugin-provider-llm`（此前是**名义挂载**） | `AgentProviderCommandHost`（13 个原子动作） |
| 会话视图（快照 / 上下文 / 时间线 / 翻页 / 派生历史） | 5 | `echo-plugin-management` | `AgentSessionViewHost`（5 次语义快照 + active_turns） |
| 自更新（触发 updater + 读状态） | 2 | **新 crate** `echo-plugin-self-update` | `SelfUpdateHost`（只有事件出口） |

**自更新这一域顺带修掉一条死路**：两条命令原来既在 `HOST_RESERVED_COMMANDS`
（插件不得接管）名单里，实现又只经"插件分发 → 联邦插件的命令处理器"可达——
于是**必失败**，且文案误报"联邦插件未启用"。现在归属明确：
reserved 名单只留真正的自我管理面（插件清单/启停/重启/Shutdown/会话迁移），
自更新作为内置插件认领自己的命令；updater 单元名与状态文件布局是**部署形状**，
换部署方式只改插件。真机验证：`RequestSelfUpdateStatus` 经 management WS 回
`SelfUpdateStatus{state:"idle"}`，`RequestState` 回 `ApiConfigUpdated` /
`ApiProfilesUpdated` / `InstanceList`（按序、无 Error）。

两域的共同形状：**"发什么、按什么顺序、文案长什么样"归插件；"状态怎么迁移"归宿主**。
典型分工证据是会话视图的**推理正文瘦身**——它是展示规则（宿主不该知道"面板嫌
推理太长"），因此从 `commands.rs` 搬进了插件，与显示时间线的其它投影策略同处
一地。

**宿主仍实处理的 11 个分支，逐类理由**：

| 类 | 条数 | 为什么不搬 |
|---|---|---|
| 自管理面（插件清单 / 启停 / 重启 / 人格增删改） | 6 | **自锁**：这些命令被抢走或依赖插件，禁用后无法经界面恢复。`HOST_RESERVED_COMMANDS` 明确挡着 |
| 会话写 + turn 控制（ClearHistory / ArchiveHistory / CompactHistory / CancelRequestedWork / SendMessage） | 5 | 动的是会话真源（`TrunkStore` 落盘）与 turn 驱动。造出来的 seam 就是 `TrunkStore` 本身——"seam 变影子"，且没有替换需求（见不搬清单） |
| 进程级改名（`RenameApi`） | 1 | 改的是**全局供应商池**（不属于任何人格），生产路径由 `process_commands` 先接走；agent 分支的 fail-closed 说明接线异常，必须留在宿主 |

于是"再往下搬"需要的不是更多胶水，而是**替换需求**：谁会换掉会话存储？答不上来
的就不该插件化——这条判据从 S1 到 S5 没有一次失效。

### S9（原计划 S6）：`Agent` 门面化 ——**已完成 2026-11**

`Agent` 从 **32 个平铺字段降到 12 个**。做法不是"加一层服务定位器"，而是
**按不变式分组**——组内成员必须一起变，否则不合并。

| 字段 | 是什么 | 为什么在这一层 |
|---|---|---|
| `model` / `prompt` | 服务 | 各自的字段必须一起变（S4a / S4c 已论证） |
| `tools` / `trunk` / `instances` | **真句柄** | 独立所有权，组合根 / 联邦泵 / 管理面直接持有 |
| `identity` | 组 | 「我是谁」：team_id / node_identity / capabilities |
| `egress` | 组 | 「事件怎么出去」：四条出口 + 状态事件绑定表 |
| `turns` | 组 | 「谁在跑」：驱动 / 并发闸 / 在飞表 / 序号 / 退出 |
| `instrumentation` | 组 | 脱敏 / 指标 / 门控日志去重 |
| `plugins` | 组 | 宿主 / 全局策略 / 重载闸 |
| `ports` | 组 | 插件装上来的接口（workspace / shell / subagent / federation） |
| `manager` | 组 | 反向弱引用 + 告警去重 |

**为什么不做成"服务定位器"**：`self.service::<T>()` 会把类型错误推迟到
运行期，而这些组都是**编译期已知**的。定位器只适合"谁来由插件决定"的东西
——那已经是 `ports` 与 `plugins` 两个组了。

**为什么三个真句柄不并进组**：`tools` / `trunk` / `instances` 是独立所有权
的**真句柄**，被组合根、联邦泵、管理面直接持用。往定位器里塞真句柄只会
多一层解引用，不变式收益为零。

#### 结构守卫（防止慢慢退回去）

门面化的失败模式不是"写错"，是"下一个人需要一个跨组共享的状态，最省事的
做法就是在 `Agent` 上再平铺一个字段"——编译通过、测试全绿，两个月后又是
30 个字段。所以加了 `tests/agent_facade.rs`，直接读源码断言：

1. `Agent` 字段数 ≤ 12（超了说明没想清楚归属）；
2. 已分组的 25 个字段名**不得**再出现在 `Agent` 的直接字段里；
3. `groups.rs` 里的字段必须真的被某个组持有（防"搬出去忘了用"）。

第 2 条是关键：字段数上限能被"删一个加一个"绕过，而"`team_id` 又回到
`Agent` 上了"必须显式撞墙。

**不做**的是把 `Agent` 本身变成插件——它是 per-persona 的作用域，对应
Cordis 的 `ctx.fork()`，不是插件。见「不搬清单」。

## 四、顺序与收益

```text
S1 工具集所有权        ✅ 已完成（本轮）
S2 ToolService         ← 一堵墙，做通它 S3 全是同构体力活
S3 四能力归位          ← 用户可感知：禁用插件后对应能力真的消失
S4 会话/turn 服务化    ← 编排骨架定边界
S5 命令泵收口          ← commands.rs 从 1967 行降到 ~300
S9 Agent 门面化        ✅ 已完成（字段 32 → 12，含结构守卫）
```

**S1 的收益为什么值得先做**：它把"业务逻辑在插件里"从**代码位置**变成
**所有权**——宿主不再知道任何一个内置工具的名字、包标签、护栏正则。这是后面
每一步都要复用的模板（本轮已在 `tools.builtin` 上跑通：端口 4 项、服务键 1 个、
插件侧 230 行、宿主侧净�� ~220 行）。
