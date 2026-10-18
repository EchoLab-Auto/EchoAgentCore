---
id: subagent
title: "Subagent 插件"
group: 插件
x: 960
y: 2020
---

# Subagent 插件

> **定位**：本文描述 `echo-agent.subagent` 插件（kind=Tool）——让模型把独立子任务**委派给隔离上下文的子 agent 执行**（子任务有自己的系统提示词与工具循环），结论经结构化 hook **作为新入站分支**回灌主会话、子上下文不回合并。覆盖动机与边界、生命周期、工具与技能、hook 机制、事件与观测、包门控与测试守护。读者：Agent 使用者与插件维护者。与 [Agent 循环](../core/core-agent-loop.md) 的回复分支（同上下文 fork/合并）互为对照；装载与包门控见 [插件化设计](./core-plugins.md)。

## 动机与边界

- **上下文隔离**：探索性任务（如"在这个仓库里找出所有 X 的用法"）会产生大量
  中间工具调用；委派给 subagent 后主上下文只保留受理回执与结论，不被中间
  过程烧掉预算
- **聚焦**：子任务的系统提示词 = base + subagent 边界说明 + 任务描述；
  历史为空，不受主会话已触发技能的干扰
- **不做**：子 agent 的工具集**剥离 `spawn_subagent`**（单层委派，防递归失控）；
  子任务不能投递外部平台——结论经 hook 回灌后由主 agent 转述

与既有概念的对照：

| 概念 | 上下文 | 回写 trunk | 用途 |
| --- | --- | --- | --- |
| 回复分支（loop.parallel） | 主上下文快照 fork | 是（按请求序号合并） | 并发回答多条入站消息 |
| **subagent** | 全新隔离上下文 | 否（结论作为 hook 事件入站） | 委派独立子任务 |

## 生命周期

1. 模型调用 `spawn_subagent`（task + 可选 timeout_secs + 可选 node / profile）——
   **同步受理**：注册进 `SubagentStore`（携带主 turn 的取消令牌子令牌）并立即返回
   子任务 id 回执；子任务在后台以隔离上下文执行。这次调用与回执按普通工具口径
   落 `ToolCall` / `ToolResult` 事件（见「事件与观测」）
2. 子 turn 走独立循环（`packages/subagent/mod.rs::run_subagent_turn`：与主循环同源的
   迭代/截断续跑/取消语义，但**不回写 trunk**、不发会话级 LLM 事件）
3. 完成/失败/超时/取消 → 发射 `SubagentCompleted`，并把结论包进
   `<subagent_event>` hook 经 `dispatch_subagent_hook` 注入主会话——
   作为**全新入站分支**（复用 `process_inbound_branch` 完整生命周期，
   `group_id=None` = 后台来源，不推送外部平台）
4. 主 agent 在新 turn 里消化结论（转告用户/继续推理）；子任务条目保留
   1 小时供观测查询，由注册表后台清扫任务回收

并发与取消：子 turn 不占用 `reply_branch_slots`（异步任务，主 turn 已结束）；
主 turn 取消经令牌链传导到子任务（`parent_cancel.child_token()`），
`Agent::shutdown` 经 `SubagentStore::cancel_all` 兜底取消全部运行中子任务。
插件停用（`TogglePlugin`）当前只停用 `spawn_subagent` 工具（包门控）；已受理的
运行中子任务不随之取消（`Agent::shutdown` 时由 `SubagentStore::cancel_all`
统一兜底；按需的单独 detach 入口已随死代码清理移除）。

## 工具与技能（`echo-agent.subagent` 包）

| 工具 | 说明 |
| --- | --- |
| `spawn_subagent` | 委派子任务（同步受理、异步执行）。参数：`task`（必填，完整自含描述——子上下文看不到主对话）、`timeout_secs`（可选，默认 600，钳制 30..=3600）、`node`（可选，联邦远程节点名/peer 配置名——填入后子任务在该节点执行；来源：工具列表中 `<peer>__<tool>` 代理工具前缀名（如 `A100-40g__bash` → `A100-40g`），或工作区会话的 `[remote:<peer>]` 标注；填错/离线会立即失败，fail-closed 不会静默回本机；多节点并行委派时结果自动聚合汇报）、`profile`（可选，子代理模型——填全局供应商池 `[agent].api_profiles` 中的名称，如 `"kimi"`；单次生效，不改父 agent 模型；名字不存在/缺 key 时**立即失败**并回报可用清单，fail-closed 不会静默回退主模型；缺省 = 继承父 agent 模型）。返回受理回执（含子任务 id；指定 profile 时回执标注模型） |

设计要点：

- **自含任务描述**：`task` 是唯一传给子上下文的用户消息；缺参/空白时返回
  纠正性文案（"子 agent 看不到当前对话，任务描述必须自含"）
- **取消传播**：主 turn 取消 → 子令牌 → 子循环的 select 中止；取消的子任务
  记为 `Cancelled` 并同样发 hook（主 agent 感知"没有结论会来"）
- **结果截断**：子任务回复超过 8K 字符（`MAX_SUBAGENT_RESULT_CHARS`，
  `packages/subagent/mod.rs::truncate_result`）时截断并标注总字符数，防单个
  结论反过来烧掉主上下文。与 hook 信封 `task` 字段的 4000 字符上限是两回事：
  后者限**任务描述**的可审计长度，前者限**结论**回灌体量
- **子代理模型覆盖（`profile`）**：执行体按名从供应商池解析并
  构建独立 provider（经计量 + 脱敏装饰，与主 provider 同标准；计量归属 =
  profile 名，余额/用量图表可直接看到子代理消耗）。池以**共享 ConfigStore
  （core.toml）为首选事实源**读取（管理面每次变更都会写回；persona 侧
  启动快照可能过期——见 `echo-agent/src/api_pool.rs` 模块文档），文件读不到
  时回退本 agent 配置快照。解析失败在**执行前**被拒绝：store 落 `Failed`、
  通知对端（联邦下同时销账聚合组）、emit `SubagentCompleted{success:false}`、
  经 `<subagent_event>` 回报——与正常完成同一条收尾链，面板委派行不会悬挂
- **可用供应商查询**：`list_api_profiles` 工具（`echo-agent.tools.builtin`
  包）列出供应商池（名称/服务商/模型/端点/密钥就绪状态 + 顶层默认），
  输出不含明文密钥；数据源同为共享 ConfigStore（`api_pool::read_snapshot`）

### 同包技能

| 技能 | 触发 | 内容 |
| --- | --- | --- |
| `subagent-delegation` | 常驻（`metadata.always: true`，每轮注入） | 委派与并行化指南：开工前扫"可委派块"、何时该委派、并行手法（`spawn_subagent` 是唯一真并行）、task 必须自含、单层委派、等回报期间不空转、结果截断 |
| `model-profiles` | 常驻（`metadata.always: true`，每轮注入） | 模型供应商与子代理模型选择规范：**默认一律不传 `profile`**（继承主 agent 模型），仅当用户明确点名某模型时按名切换；`list_api_profiles` 查询可用清单；`profile` 名不存在/缺 key 时 fail-closed 立即失败并回报清单（规则随技能文件演进，直接编辑即可调整） |

技能 frontmatter 声明 `package: echo-agent.subagent`，随包级门控与工具一起
启停（见 [插件化设计](./core-plugins.md)「Package」章节）。

## Hook 机制（echo-loop 注入接口）

子任务完成通知与 QQ 消息/定时器同族：**结构化 hook → 新入站 turn**。
`<subagent_event>` 包装与解析由 `input_marker` 同族的
`packages/subagent/mod.rs::wrap_subagent_event` 承担。

异步编排工具接入 echo-loop 的接口是 `SubagentToolHooks`
（`source/loop/echo-loop/src/runner.rs`，由 `process_via_echo_loop` 装配进
`RunExtras`）：

- `is_async_tool(name)`：认领异步编排工具（当前仅 `spawn_subagent`）
- `execute_async`：**同步返回 future 句柄**的执行器——编排侧需要 tokio::spawn
  后台任务时经通道把结果带回（避免 async 闭包捕获引用的 Send 生命周期困境）；
  `spawn_subagent` 本身是同步受理，直接包 ready future
- `extra_tool_definitions`：模型可见 schema 追加接口（当前装配为 `None`——`spawn_subagent` 的 schema 由注册表统一提供，避免重复工具名）

`run_tool`（集成测试与直接分派入口）同样特判 `spawn_subagent`，与 hook 路径
语义一致（turn 只经 echo-loop 的 hook 通道）。

## 事件与观测

委派有三条观测面：**调用面**（一次普通的工具调用）、**任务面**（子任务自身的
生命周期）与**回灌面**（结论如何进入主会话）。

- **调用面**：`spawn_subagent` 经 echo-loop 的异步 hook 通道直入管线、不走
  `run_tool`，调用与回执仍按普通工具口径落账（`agent/turn.rs` 的
  `execute_async_subagent`）：先写 `ToolCall`（`arguments` 出口脱敏，带
  `started_at_ms`）→ 受理或拒绝后写配对的 `ToolResult`（回执文本，带
  `elapsed_ms`）。于是 spawn 时延可算，`task` / `profile` / `node` /
  `timeout_secs` 实参可查，面板委派行与普通工具行同源同账
- **任务面**：`SubagentStarted { session_id, task }` / `SubagentCompleted { session_id, success }`
  （`echo-protocol` 事件，Panel 据此渲染子任务状态；`started` 的 `task` 是全文、
  不受下面的 4000 字符上限约束；入口：会话视图入口行「任务」弹层，按当前会话
  过滤展示）
- **回灌面**：完成 hook 作为 `MessageReceived { adapter_name: "subagent" }`
  进入显示时间线与 trunk（会话历史含完整委派轨迹：调用 → 回执 → 结论事件）

`<subagent_event>` 信封字段（`packages/subagent/mod.rs::wrap_subagent_event`
包装，payload 由 `agent/subagent_runtime.rs` 的完成链与受理前拒绝链产出）：

| 字段 | 说明 |
| --- | --- |
| `event` | 固定 `subagent_event` |
| `subagent_id` / `session_id` | 子任务 id 与所属会话 |
| `task` | 任务描述，上限 4000 字符（见下） |
| `success` / `result` | 成败与结论文本（受理前拒绝时 `result` 形如「子任务受理失败：…」） |
| `parent_branch_id` | 发起本次委派的主分支 |
| `completed_at_ms` | 收尾时刻（epoch 毫秒） |

**`task` 字段的 4000 字符上限**（`subagent_runtime.rs::subagent_task_field`）：
不超过 4000 字符原样透传（783 字符这类实测常见长度、恰好 4000 的边界都原样）；
超过 4000 则保留前 4000 并追加「…[task 过长已截断，共 N 字符；完整文本见该
spawn_subagent 调用的 arguments]」。任务全文始终可从该次调用的
`ToolCall.arguments` 读到（沿革：此前固定截到 500 字符，长描述在 hook 里只剩
开头一段）。

## 门控与测试守护

### 包门控与装配

- 包级门控：`SUBAGENT_PLUGIN_ID` 在 `GATED_PLUGIN_IDS`/`BUILTIN_PLUGIN_IDS`；
  勾选区可见性经 Panel `PACKAGE_GATED_PLUGIN_IDS`（+`PACKAGE_DISPLAY_NAMES`）
  镜像；更新器 `scripts/update.sh` 校验清单含 `echo-agent.subagent`
- 装配（`core/main.rs`）：每 persona 注册 `SpawnSubagentTool`（注册表路径为
  兜底）+ `attach_subagent_runtime` 注入 store 与 spawn 闭包（弱引用 agent，
  不构成循环）；插件 mount/unmount 经 `reapply_plugin_gating` 逐 persona
  启停（`allows_dynamic_tool` 双重判定：运行态已装配 ∧ persona 白名单允许）

### 测试守护

- 单元（`packages/subagent/mod.rs`）：受理注册/回执、空任务纠正性拒绝、超时钳制、
  `truncate_result` 结果截断、cancel_all 幂等、hook 信封结构
- 单元（`agent/subagent_runtime.rs::subagent_task_field_keeps_long_descriptions_verbatim`）：
  信封 `task` 字段的边界——783 与 4000 字符原样、4001 截断且标注总字符数
- 集成（`tests/agent_integration.rs`）：装配前后工具可见性、端到端
  「spawn → 子任务执行 → SubagentStarted/Completed → hook 入站」、
  插件白名单拒绝、主 turn 取消级联（子任务记 Cancelled 并发完成事件）
- 委派落账（`agent/tests.rs::spawn_subagent_call_and_receipt_are_recorded_in_event_log`）：
  即便运行态未接线（受理被拒），`ToolCall` 与 `ToolResult` 也成对落库，
  实参含 `task` 全文与 `timeout_secs`、`started_at_ms` 非空
