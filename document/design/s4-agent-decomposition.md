---
id: s4-agent-decomposition
title: "S4 计划：Agent 拆服务"
group: 规划
x: 320
y: 3320
---

# S4 计划：`Agent` 拆服务

> 前序：[decoupling-plan.md](./decoupling-plan.md) Phase 3 / 3.5 / S1-S3 已完成。
> 本文只谈 **S4**——把 27,917 行业务核心里的 `Agent` 从"一个 33 字段的上帝对象"
> 拆成若干 `Ctx` 服务。这是"所有代码都是插件"的最后 40%。

## 零、先说结论（给决策者）

| 项 | 判断 |
|---|---|
| 技术上可行吗 | **可行**，且无未知机制——`Ctx` / 插件树 / 命令域都已就位并有测试 |
| 值得做吗 | **分阶段**：S4a/S4b 值得（各 1-2 周，去掉真实耦合）；S4c 见仁见智；**S4d 不建议** |
| 主要收益 | 可测试性（服务可替换成 fake）、并发边界清晰、`manager ↔ Agent` 环解开 |
| 主要代价 | 6-10 周；`run_tool` 这类方法从"1 个 `&self`"变成"5 次服务解析"；**可读性换可替换性** |
| 不做的理由 | 那 236 个方法不是 10 个能力，是**一个应用的编排逻辑**；且没有真实的替换需求 |

**建议节奏**：先做 S4a+S4b（4 周内落地，风险可控、测试有安全网），S4c/S4d 视
S4b 的实际收益再定。**不要一次性重写**——`agent/` 有 7,146 行测试，但那不是
"随便改都安全"的意思，是"改坏了能立刻发现"的意思。

## 一、测量（2026-11 实测，不是估算）

```
source/backend/echo-agent/src   27,917 行   全仓 70,686 行的 39.5%
├── agent/                      13,836
├── session/                     5,367
├── packages/                    3,650   （federation 1,887 + instance_ops 1,363）
└── 顶层（plugins/config/metrics/agent_manager/…）  5,064
```

`Agent` 本体（**2026-11 复测，修正了本文初稿的误算**）：

```
pub struct Agent      39 个字段（初稿写 33 是 grep 漏了带注释的）
impl Agent            148 个方法，散在 12 个文件里
  agent/mod.rs          56   ← 核心：emit / trunk / prompt / store 接线
  agent/api_admin.rs    21   ← 模型出口（本次 S4a 的对象）
  agent/gating.rs       17
  agent/inbound.rs      17
  agent/commands.rs     16   ← 56 个 match 分支的 dispatch
  agent/workspace_commands.rs 5
  agent/subagent_runtime.rs   5
  packages/instance_ops/commands.rs 5
  agent/tool_dispatch.rs 2 · tool_exec 2 · plugin_reload 1 · compact 1
```

> **初稿的错**：说 `impl Agent` 有 236 个方法、散在 15 个文件，把
> `impl AgentConfig`（`config.rs`，35 个方法）与 `impl AgentManager`
> （`agent_manager.rs`，33 个）也算进去了——`grep "^impl Agent"` 会前缀匹配到
> 这两个类型。**`AgentConfig` 早就是独立类型**，不在上帝对象里。修正后是
> 148 个方法 / 12 个文件。

**从外部看它有多大**：组合根 `core/` 里 55 处直接调 `Agent` 的方法（`emit` /
`set_*` / `apply_*` / `trunk` / `process_message`），`packages/instance_ops` 23 处。
这 78 处是"外部世界与 Agent 的接口"，拆服务时它们全都要改。

**耦合环（最硬的一处）**：

```
Agent ──(字段 manager)──▶ AgentManager ──(字段 agents)──▶ Agent
                ↑ 27 处 `self.manager()` 回查
```

`manager()` 的用途是"按 team_id 找别的 agent"（命令转发、跨人格广播、目录浏览的
store 并集）。这个环是 `Agent` 拆不开的结构性原因之一：**拆出来的服务要么继续持有
manager（那环还在），要么改成注入"解析 team_id → 服务"的函数**。

## 二、目标与非目标

### 目标（可断言）

1. `Agent` 的字段从 39 减到 **≤ 16**（只剩身份、总线、句柄这类"接线"）；
2. 至少 4 个能力能**独立替换成 fake** 而不用起整个 Agent（prompt 装配、压缩、
   gating、api_admin）；
3. `Agent` ↔ `AgentManager` 的环解开（依赖单向）；
4. 现有 929 项测试**全部保留**（不许"因为难测所以删"）。

### 非目标（明确不做）

- **不把 `Agent` 变成插件**。它是"一个使用者的运行实例"，天然是组合产物，
  不是可插拔能力。硬做成插件只会得到一个 `Ctx` 里塞满 `Agent` 的假解耦。
- **不拆 `Session` / `TrunkStore`**（S3 已判定：先要接口设计，另立项）。
- **不改线协议**。`BackendCommand` / `BackendEvent` 一个字节不动——S1-S3 已经把
  命令域与文本块做成贡献，S4 只动进程内结构。
- **不追求"零 `&self`"**。有些方法（`emit`、`team_id`）本来就是实例级的。

## 三、39 个字段该归谁（分组的依据）

| 组 | 字段 | 归属 | 备注 |
|---|---|---|---|
| **A. 身份与接线**（留） | `team_id` `node_identity` `cancel` `draining` `pubevent_bus` `event_sink` `pubhandle` `manager` | `Agent` | 这些是"这个实例是谁、连到哪" |
| **B. 模型出口** | `provider` `config` `active_model` `config_store` `persona_api` `session_providers` `session_provider_locks` `balance_snapshot_lock` | `ModelService` | 换 provider / 会话级覆盖 / 余额 |
| **C. 提示词** | `pubskills` `system_prompt_cache` `last_prompt_blocks` | `PromptService` | 已有 `Contribution::Text`，天然对接 |
| **D. 工具执行** | `pubtools` `redactor` | `ToolService` | `run_tool` 140 行，碰 5 个协作点 |
| **E. 会话与压缩** | `pubtrunk` `_timeline_projection` `reply_branch_slots` `active_inbound_turns` `next_message_sequence` | `SessionService` | 与 S3 的接口设计合并 |
| **F. 插件与策略** | `plugin_host` `global_policy` `capabilities` `gating_log_state` `plugin_reload_started` | `PluginService` | 已大半在 `PluginHost` |
| **G. 外部子系统** | `pubinstances` `shell` `federation` `subagent` `workspace_store` | 各自���务 | 都已经有独立 crate，只是还挂在 Agent 上 |
| **H. 循环** | `loop_runner` | 已由 S2 变成 `Ctx` 服务 | ✅ 已完成 |
| **I. 杂** | `metrics` `manager_warned` | `MetricsService` / 删 | `manager_warned` 是一次性警告标志，可删 |

## 四、五个阶段（每阶段独立可交付）

### S4a：`ModelService` — 1 周

**做什么**：把 8 个模型相关字段 + `config.rs` 的 35 个方法搬进
`echo-agent/src/model_service.rs`，实现 `Ctx` 服务键 `"model"`。

**为什么先做它**：
- 边界最干净（这些字段只被 `config.rs` / `api_admin.rs` / `turn.rs` 用）；
- **有真实替换需求**：单测里换一个 fake provider 现在要造整个 Agent；
- 风险最低（35 个方法里 30 个是 getter/setter）。

**验收**：`Agent` 字段 39 → 32（实测达成，见 §八）；`config.rs` **本来就**是
`impl AgentConfig`（不在上帝对象里，初稿判断有误）；全量测试全绿。

**回退**：`git revert` 单个提交即可（不留双向兼容层）。

### S4b：解 `manager ↔ Agent` 环 — 1-2 周（**已完成，见 §九**）

**做什么**：把 27 处 `self.manager()` 换成注入的 `AgentResolver`：

```rust
/// 宿主注入：按 team_id 找 agent（或它的能力）。
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, team_id: &str) -> Option<Arc<dyn AgentHandle>>;
    fn all(&self) -> Vec<Arc<dyn AgentHandle>>;
}
```

`AgentHandle` 是"别的 agent 需要暴露什么"的最小面（`emit` / `team_id` /
`workspace_store` / `process_message`）——**不是整个 `Agent`**。

**为什么值得**：这是 `Agent` 拆不开的结构性原因。解开之后，`Agent` 不再持有
`AgentManager`，依赖变成单向（`AgentManager → Agent`）。顺带把 `workspace_host`
里那个 `Arc<dyn Fn() -> Vec<Arc<Agent>>>` 收敛到这个 trait（现在是临时方案）。

**风险**：27 处调用点，其中 `commands.rs` 的转发逻辑有"目标人格不存在"的分支
（错误消息要一致）。**用一个测试锁住**：`apply_command` 的跨 team 转发行为。

**验收**：`Agent` 不再有 `manager` 字段；`agent_manager.rs` 不在 `impl Agent` 里；
跨 team 转发的测试通过；`workspace_host` 改用 `AgentResolver`。

### S4c：`PromptService` + `ToolService` — 2-3 周

**做什么**：

- `PromptService`：`build_prompt_blocks` 的宿主部分（base / skills / boundary /
  插件块）+ 缓存。搬完 `prompt.rs` 不在 `impl Agent`。
- `ToolService`：`run_tool` / `dispatch_tool` / `record_interrupted_tool_result`
  （276 + 140 行）。它现在碰 5 个协作点（redactor / emit / trunk / tools / 会话 id）,
  拆开后变成"服务依赖服务"——**这是 S4 里唯一会让可读性下降的地方**。

**为什么合在一阶段**：两者都要改 `turn.rs` 的调用点，分开做等于同一个文件改两遍。

**验收**：`Agent` 字段 32 → 24；`/context` 视图输出与改造前**逐字节相同**
（用一份快照测试锁住——这是"重构不改行为"的硬证据）。

### S4d：`SessionService` — 2-4 周（**建议暂缓**）

**做什么**：把 `session/` 的 5,367 行 + 5 个字段拆成服务。

**为什么暂缓**：

1. S3 已经实测过它的依赖面：`history.rs` 依赖 `input_marker` 与 `timeline`（都是
   业务模块），`SessionInfo` / `TimelineMessage` 在 `echo-protocol`——**跨层依赖
   要先设计再动**，不是机械搬家；
2. 它是"事件日志的唯一真源"，改错的影响面最大（会话历史、投影、压缩、归档）；
3. **没有替换需求**：没人会换掉 trunk 的实现。

**如果要做**：先出接口设计（`SessionHost` trait：事件追加 / 投影 / 输入标记解析 /
压缩触发），再做搬家。与 `RemoteWorkspaceQuery` / `WorkspaceHost` 同一套路。

### S4e：收尾 — 1 周

- `Agent` 字段 ≤ 16，`impl Agent` 只剩 `agent/mod.rs` 一个文件；
- 55 处组合根调用点全部走服务；
- 文档：`document/overview/architecture.md` 加一节"服务一览"；
- 门禁：加一条测试断言 `Agent` 字段数（防回流）。

## 五、每阶段都要做对的三件事

1. **不加兼容层**。不留 `#[deprecated]` 转发、不留"新旧两条路径"。理由：兼容层会
   让"到底谁在调用"变得不可知，而这类重构最怕的就是看不见的调用点。
2. **行为快照先写**。S4c 的 `/context` 输出、S4b 的跨 team 错误消息——改之前先写
   断言，改完比对。929 项测试是安全网，但网眼大小要自己确认。
3. **一次一个提交，每提交可编译可测**。这批改动跨 15 个文件，攒在一起提交等于
   放弃 bisect。

## 六、风险清单

| 风险 | 触发条件 | 缓解 |
|---|---|---|
| 可读性下降 | `run_tool` 从 1 个 `&self` 变 5 次解析 | 把 5 个依赖**一次性**在入口取好，往下传结构体而不是每个函数各自解析 |
| 循环依赖拆不干净 | `SessionService` 反过来要 `PromptService` | 先定义接口再搬（S3 的教训） |
| 测试改得面目全非 | 为了编译而改断言 | 禁止改断言；要改先在提交信息里写"为什么原断言是错的" |
| 中途状态不可用 | 5 阶段做一半停下 | 每阶段独立可交付——S4a 单独提交就是成品 |
| 收益不达预期 | 拆完发现没人替换 fake | **S4b 之后重新评估**：如果三个月内没有"用 fake 测新能力"的实际需求，S4c/S4d 就别做 |

## 七、当前建议

**先做 S4a + S4b**（4 周内），因为它们解的是**真实耦合**（假 provider 难造、
`manager` 环），不是为了架构好看。S4c 之后要不要继续，看两个信号：

1. 有没有第三个"想单独测某个能力"的需求出现；
2. `ToolService` 拆完后 `run_tool` 的圈复杂度是降了还是升了（如果升了，说明
   这条路走不通，及时收手比硬撑有价值）。

**最后一句实话**：S4 换来的主要是**可替换性与可测性**，不是新能力。如果当下的
痛点是"加功能慢"，那瓶颈大概率不在这里——它在 `commands.rs` 的 56 分支 dispatch
与 Panel 的 109 个联合类型分支上，而那些已经被 S1 的 `Contribution::Command` 打开
了缺口，继续往那个方向做收益更直接。

---

## 八、S4a 完成记录（2026-11）

### 做了什么

`Agent` 的 8 个模型相关字段搬进 `crate::model_service::ModelService`，`Agent`
改为持 `Arc<ModelService>`：

```rust
// 之前（8 个字段）
provider: RwLock<Arc<dyn LlmProvider>>,
config: RwLock<AgentConfig>,
active_model: RwLock<String>,
config_store: tokio::sync::Mutex<Option<echo_config::ConfigStore>>,
persona_api: tokio::sync::RwLock<Option<String>>,
session_providers: DashMap<String, (String, Arc<dyn LlmProvider>)>,
session_provider_locks: DashMap<String, tokio::sync::Mutex<()>>,
balance_snapshot_lock: tokio::sync::Mutex<()>,

// 之后（1 个字段）
pub(crate) model: Arc<crate::model_service::ModelService>,
```

**字段数 39 → 32**。`ModelService` 新增 6 个语义方法（`provider()` /
`replace_provider()` / `config_snapshot()` / `model_name()` /
`set_active_model()` / `session_provider()` / `clear_session_provider()`），
它们封装的是"必须一起变"的那几组操作（不变式表见 `model_service.rs` 模块文档）。

### 三个过程中的真发现

1. **`config.rs` 不是 `impl Agent`**——它是 `impl AgentConfig`（既有独立类型）。
   初稿把它算成"上帝对象的一部分"是 grep 前缀匹配的错误。**这降低了 S4 的总量**：
   上帝对象是 148 个方法，不是 236 个。
2. **同名方法会静默改变语义**。机械改写 `self.active_model()` 时，`ModelService`
   上恰好也有个 `active_model()`——于是"**带回退**的模型名解析"被换成了"**不带
   回退**的原始字段值"，编译通过、只有一个集成测试失败（`LlmRequest.model` 变成
   空串）。修法是把 ModelService 的方法**改名** `model_name()`，让这类错误变成
   编译错误而不是运行期行为差异。
3. **字符串里的 `agent.provider` 会被误改**。全局正则把
   `"echo-agent.provider.llm"` 改成了 `"echo-agent.model.provider.llm"`——插件 id
   是线上契约，改了等于所有配置失效。已还原，并在提交前加了一道
   `grep 'echo-agent\.model\.'` 的自查。

### 验证

- **932 项 / 57 套件全绿**（+3：`ModelService` 的初始状态 / 名字与 provider 往返 /
  会话级覆盖设置与清除）；
- clippy 仅剩 1 条既有警告（`agent_manager.rs`，与本次无关）；
- **真实进程**：`SwitchModel` → `ApiConfigUpdated { model }` + 回执，这条路径串起
  `config.write()` → `set_model`（active_model 写）→ `emit_api_config`
  （config 读 + `active_model()` 带回退）→ `persist_config`（config_store 读），
  全部经 `ModelService`。

### 对后续阶段的影响

- **S4b 更容易了**：`Arc<ModelService>` 可以直接递给 `AgentResolver` 的实现与
  其它服务，不必再绕 `Agent`；
- **S4c 的 `PromptService`**：它只需要 `config_snapshot()`，现在拿一个
  `Arc<ModelService>` 就够，不用整只 `Agent`——这正是"把字段搬出去"的直接收益；
- **教训写进流程**：以后做机械改写前，先 `grep` 目标类型上**同名的方法**与
  **同名的字符串字面量**，两者都会让"编译通过的改写"变成静默 bug。

---

## 九、S4b 完成记录（2026-11）

### 先纠正计划里的两个数字

初稿写"27 处 `self.manager()` 回查"。实测：

```
17 处 self.manager()（另 3 处是 self.shell().manager()，无关）
├── 只读查询 12 处：resolve(7) / all(4) / infos(1)
└── 管理动作  5 处：save_profile / delete_profile / set_enabled+fatory_ref / api_ref_owners+replace_api_refs
```

而且 **那个"环"有一半早就解掉了**：`Agent.manager` 是 `Weak`，加上
`RunningAgent` 的 `_sealed` + 唯一构造入口 `bound()` 让"必须绑定"成了类型约束
（2026-10-20 事故后的补丁，做得很好）。所以 S4b 的真实目标不是"解环"，而是：

1. `Agent` 的**类型面里不该出现 `AgentManager`**；
2. 未绑定的路径**不该静默降级**。

### 做了什么

新增 `agent/registry.rs`，两个**分开的** trait：

| trait | 方法 | 关注点 |
|---|---|---|
| `AgentRegistry` | `resolve` / `all` / `infos` | 看兄弟（只读） |
| `TeamAdmin` | `save_profile` / `delete_profile` / `set_enabled` / `factory_ref` / `get_running` / `api_ref_owners` / `replace_api_refs` | 增删改人格 |

`Agent` 的 `manager: Weak<AgentManager>` 换成两个 `Option<Weak<dyn …>>`，
`set_manager()` 换成 `set_host(&registry, &admin)`；`AgentManager` 同时实现两个
trait（唯一实现）。

**为什么分两个 trait 而不是一个**：合成一个等于把 `AgentManager` 的方法表原样
搬过来，`Agent` 对管理的依赖一点没少，只是改了名字。分开之后，测试可以只造
查询面的 fake（§九的测试就是这么写的），而"管理面该搬出去"这件事也有了明确的
删除点：删 `TeamAdmin` 时 5 个调用点会全部编译失败并指出位置。

### 两个技术细节

**1. `save_profile` / `set_enabled` 用 `self: Arc<Self>` 接收者。**
它们内部要把 manager 交给新建的 agent 做绑定，`&self` 拿不到 `Arc`。`Arc<Self>`
是**对象安全**的接收者（稳定 Rust 支持），所以 `Arc<dyn TeamAdmin>` 上照常可调，
调用点写 `Arc::clone(&admin).set_enabled(..)`。

**2. `Weak::<dyn Trait>::new()` 不存在（`Sized` 约束）。**
所以字段是 `RwLock<Option<Weak<dyn …>>>`，初始 `None`。

### fail-closed 的具体改进

`ToggleTeam` 原来是：

```rust
match self.manager() {
    None => {}          // ← 未绑定时：用户点"启用"什么都不会发生，无日志
    Some(mgr) => { … }
}
```

现在是 `match self.require_admin() { Err(reason) => emit(Error { reason }), … }`
——失败必须有回执。这正是 S4b 相对"解环"更实际的那一半价值。

### 验证

- **319 项 lib 测试全绿**（+2：未绑定 fail-closed / fake registry 能解析兄弟）；
- 全 workspace：50 套件 ok，core 87 项 ok；
  唯一失败是 `doc_catalog::tool_catalog_is_fresh` ——**与本次无关**：并发文档
  重构改了 `document/plugins/tool-catalog.md` 的坐标（x: 960→1280），已用 HEAD
  版本单独验证该门禁通过；
- clippy：仅剩 1 条既有警告（就是 `RunningAgent` 的 `_sealed`，即那条被刻意保留
  的约束本身）；
- **真实进程**：`RequestTeamsList` → `TeamsList: ["t"]`（走 `infos`）；
  `ToggleTeam{enabled:false}` → `TeamsList` + `Error: agent t disabled`
  （走 `set_enabled` 的 `Arc<Self>` 接收者）。

### 剩下的（S4c 前置）

- `TeamAdmin` 仍是技术债：团队 CRUD 该由**进程级**处理器接掉（与 S1 的
  `Contribution::Command` 同一套路），而不是让被命令泵选中的 agent 代办。
- `running.agent` 这类"通过 RunningAgent 拿兄弟的 Agent"仍在用——那是
  `AgentRegistry::all()` 的返回类型决定的，可接受（数据载体，不是行为依赖）。
