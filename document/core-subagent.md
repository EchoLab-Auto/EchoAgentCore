---
id: subagent
title: "Subagent 插件"
group: 后端模块
x: 955
y: 2047
---

# Subagent 插件

Subagent 插件（`echo-agent.subagent`，kind=Tool）让模型把独立子任务**委派给
一个隔离上下文的子 agent 执行**：子任务拥有自己的系统提示词与工具循环，
完成后经结构化 hook 把结论**作为新入站分支**回灌主会话。与
[Agent 循环](./core-agent-loop.md)的临时回复分支（同一上下文 fork/合并）
不同，subagent 的上下文**不回合并**——主上下文只看到一次工具调用（受理
回执）与稍后的结论事件。

## 动机与边界

- **上下文隔离**：探索性任务（如"在这个仓库里找出所有 X 的用法"）会产生大量
  中间工具调用；委派给 subagent 后主上下文只保留受理回执与结论，不被中间
  过程烧掉预算
- **聚焦**：子任务的系统提示词 = base + subagent 边界说明 + 任务描述；
  历史为空，不受主会话已触发技能的干扰
- **不做**：子 agent 的工具集**剥离 `spawn_subagent`**（单层委派，防递归失控）；
  子任务不能投递外部平台——结论经 hook 回灌后由主 agent 转述

## 生命周期

1. 模型调用 `spawn_subagent`（task + 可选 timeout_secs）——**同步受理**：
   注册进 `SubagentStore`（携带主 turn 的取消令牌子令牌）并立即返回
   子任务 id 回执；子任务在后台以隔离上下文执行
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
（2026-09-30 审计注：插件卸载路径当前仅停用工具，运行中子任务不随之取消——
`Agent::detach_subagent_runtime`（取消并摘除运行态）已备但尚无调用方，
待与 mount 侧的回挂一并接线。）

## 工具（`echo-agent.subagent` 包）

| 工具 | 说明 |
| --- | --- |
| `spawn_subagent` | 委派子任务（同步受理、异步执行）。参数：`task`（必填，完整自含描述——子上下文看不到主对话）、`timeout_secs`（可选，默认 600，钳制 30..=3600）。返回受理回执（含子任务 id） |

设计要点：

- **自含任务描述**：`task` 是唯一传给子上下文的用户消息；缺参/空白时返回
  纠正性文案（"子 agent 看不到当前对话，任务描述必须自含"）
- **取消传播**：主 turn 取消 → 子令牌 → 子循环的 select 中止；取消的子任务
  记为 `Cancelled` 并同样发 hook（主 agent 感知"没有结论会来"）
- **结果截断**：子任务回复超过 8K 字符时截断并标注，防单个结论反过来烧掉
  主上下文

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

内置循环不经 hook 接口：`run_tool` 直接特判分派 `spawn_subagent`（schema
在构建工具列表时按 `allows_dynamic_tool` 注入）——两条路径语义一致。

## 技能（同包）

| 技能 | 触发 | 内容 |
| --- | --- | --- |
| `subagent-delegation` | 常驻（`metadata.always: true`，每轮注入；2026-09 由关键词触发升级） | 委派与并行化指南：开工前扫"可委派块"、何时该委派、并行手法（`spawn_subagent` 是唯一真并行）、task 必须自含、单层委派、等回报期间不空转、结果截断 |

技能 frontmatter 声明 `package: echo-agent.subagent`，随包级门控与工具一起
启停（见 [插件化设计](./core-plugins.md)「Package」章节）。

## 事件与观测

- `SubagentStarted { session_id, task }` / `SubagentCompleted { session_id, success }`
  （`echo-protocol` 事件，Panel 据此渲染子任务状态——2026-09-19 起在会话视图
  入口行「任务」弹层中按当前会话过滤展示，替代原顶栏任务视图）
- 完成 hook 作为 `MessageReceived { adapter_name: "subagent" }` 进入显示时间线
  与 trunk（会话历史含完整委派轨迹：调用 → 回执 → 结论事件）

## 门控与接线

- 包级门控：`SUBAGENT_PLUGIN_ID` 在 `GATED_PLUGIN_IDS`/`BUILTIN_PLUGIN_IDS`；
  勾选区可见性经 Panel `PACKAGE_GATED_PLUGIN_IDS`（+`PACKAGE_DISPLAY_NAMES`）
  镜像；更新器 `scripts/update.sh` 校验清单含 `echo-agent.subagent`
- 装配（`core/main.rs`）：每 persona 注册 `SpawnSubagentTool`（注册表路径为
  兜底）+ `attach_subagent_runtime` 注入 store 与 spawn 闭包（弱引用 agent，
  不构成循环）；插件 mount/unmount 经 `reapply_plugin_gating` 逐 persona
  启停（`allows_dynamic_tool` 双重判定：运行态已装配 ∧ persona 白名单允许）

## 与既有概念的关系

| 概念 | 上下文 | 回写 trunk | 用途 |
| --- | --- | --- | --- |
| 回复分支（loop.parallel） | 主上下文快照 fork | 是（按请求序号合并） | 并发回答多条入站消息 |
| **subagent** | 全新隔离上下文 | 否（结论作为 hook 事件入站） | 委派独立子任务 |

## 测试守护

- 单元（`packages/subagent/mod.rs`）：受理注册/回执、空任务纠正性拒绝、超时钳制、
  结果截断、cancel_all 幂等、hook 信封结构
- 集成（`tests/agent_integration.rs`）：装配前后工具可见性、端到端
  「spawn → 子任务执行 → SubagentStarted/Completed → hook 入站」、
  插件白名单拒绝、主 turn 取消级联（子任务记 Cancelled 并发完成事件）
