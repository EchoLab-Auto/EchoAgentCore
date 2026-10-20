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
> 读者：内核与插件体系的实施者。**状态：实施中**——S1 已完成，S2–S6 见清单。

## 一、判据（为什么"代码在插件 crate 里"不等于"业务是插件"）

一个能力算不算"插件"，看三件事，缺一不算：

| # | 判据 | 反例（当前仍有） |
|---|---|---|
| 1 | **实现在插件 crate 里**（零 `echo-agent` 依赖） | 已基本达成 |
| 2 | **入口经插件机制注册**（命令域 / 文本块 / 工具 / 服务键），宿主不认识它 | `tools` / `skills` / `subagent` / `adapter` 的装载仍由宿主直接调用 |
| 3 | **副作用经插件��制发出**（事件、状态变更、对其他能力的通知） | `run_tool` 里 `call.name == "checklist"` 的特判；`subagent` 的 hook 派发 |

2026-11 实测（口径：应用代码，已排测试）：

```text
              起点（S1 前）        现在（S3 完成）
插件 crate     13,883  39.7%   →   18,956  51.1%
宿主实现       21,103  60.3%   →   18,143  48.9%
```

宿主那 57% 里，**约 8,000 行是宿主骨架**（插件注册表、命令泵路由、面板协议、
配置、时间线与指标）、**约 8,000 行是编排逻辑**（会话 / 压缩 / turn / 门控），
**约 4,500 行是本该是插件但还不是能力**（联邦 1,698 / API 管理面 1,208 /
subagent 执行体 401 / instance 桥接 747 / turn 驱动 512）。

### 命令域的迁移进度（最直观的读数）

```text
                 S3 前     现在
插件处理          7 条      35 条   ██████████████░░░░░░  66%
宿主兜底         46 条      31 条
文件行数        1967 行    1419 行   （-28%）
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

## 不搬清单（每条都试过，附理由）

诚实记录"没做"比做完的部分更重要——否则下一个人会把同样的时间再花一遍。

| 模块 | 行数 | 为什么不搬 |
|---|---|---|
| **API 管理面**（`agent/api_admin.rs`） | 1,208 | 核心逻辑建立在 **`AgentConfig`** 上（`update_api_config` 前 90 行都是"在配置聚合体上改 profile"）。搬它要先下沉 `AgentConfig`——那是整个配置层，而没人会替换"agent 的配置结构"。**可搬的部分已搬**：`ApiProfile`（定义层）、`api_pool`（provider 插件）、`list_api_profiles`（tools 插件） |
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

### S5：命令泵收口

`commands.rs` 53 个分支 → 每个域一条"插件未启用"兜底：

```text
session/上下文 17 · provider/api 14 · workspace 13（已迁） · skills 9
instance 7 · selfupdate/plugin 5（宿主保留） · team 4（已出 agent） · shell 4 · federation 4
```

### S6：Agent 退化为门面

`Agent` 从 30 字段 / 90 方法降到"会话身份 + 服务定位器 + 事件出口"，
其余方法变成 `self.service::<T>().method(...)` 的一行转发。**不做**的是
把 `Agent` 本身变成插件——它是 per-persona 的作用域，对应 Cordis 的
`ctx.fork()`，不是插件。

## 四、顺序与收益

```text
S1 工具集所有权        ✅ 已完成（本轮）
S2 ToolService         ← 一堵墙，做通它 S3 全是同构体力活
S3 四能力归位          ← 用户可感知：禁用插件后对应能力真的消失
S4 会话/turn 服务化    ← 编排骨架定边界
S5 命令泵收口          ← commands.rs 从 1967 行降到 ~300
S6 Agent 门面化        ← 字段 30 → ~8
```

**S1 的收益为什么值得先做**：它把"业务逻辑在插件里"从**代码位置**变成
**所有权**——宿主不再知道任何一个内置工具的名字、包标签、护栏正则。这是后面
每一步都要复用的模板（本轮已在 `tools.builtin` 上跑通：端口 4 项、服务键 1 个、
插件侧 230 行、宿主侧净�� ~220 行）。
