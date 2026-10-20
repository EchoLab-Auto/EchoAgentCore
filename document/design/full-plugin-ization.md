---
id: full-plugin-ization
title: "全插件化：业务逻辑全部组织为插件（dsh 对齐）"
group: 规划
x: 320
y: 3400
link: ["decoupling-plan | 完全解耦推进计划 | b>t", "s4-agent-decomposition | S4 计划：Agent 拆服务 | b>t"]
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
插件 crate 13,883 行   ████████████░░░░░░░░░░░░░░░░░░  39.7%
宿主实现   21,103 行   ██████████████████░░░░░░░░░░░░  60.3%
```

宿主那 60% 里，**约 8,000 行是宿主骨架**（插件注册表、命令泵路由、面板协议、
配置、时间线与指标）、**约 8,000 行是编排逻辑**（会话 / 压缩 / turn / 门控），
**约 4,500 行是本该是插件但还不是能力**（联邦 1,698 / API 管理面 1,208 /
subagent 执行体 401 / instance 桥接 747 / turn 驱动 512）。

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

### S2：工具执行链（`ToolService`）——wall 级

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

### S3：其余四个能力归位

| 能力 | 现状 | 做法 |
|---|---|---|
| **subagent 执行体**（401 行） | 工具 + store 在插件，执行体在宿主 | 注入 `SubagentExecutionHost`（建 agent / 取 provider / 发 hook），执行体搬进插件 |
| **instance 桥接**（747 行） | 插件有实现，宿主侧是 `impl Agent` 粘合 | 同 workspace 的收法：`InstanceHost` 四方法（emit / trunk / registry / gate） |
| **联邦**（1,698 行） | 有机制 crate（`echo-federation`），无插件 crate | 新建 `echo-plugin-federation`：7 条命令 + 4 个 peer 级后台任务 + `FederationPorts` 注入 |
| **API 管理面**（1,208 行） | provider 插件只有工厂 | 管理面归 `echo-plugin-provider-llm`：`ApiAdminHost`（配置写回 / 探测 / 计量），14 条 `*Api*` 命令迁出 |

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
