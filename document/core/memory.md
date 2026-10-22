---
id: memory
title: "会话记忆"
group: 框架
x: 960
y: 980
---

# 会话记忆

> **定位**：本文描述**会话记忆**——事件溯源日志（`echo-session`）、模型上下文投影、压缩与归档、显示时间线。读者：需要理解"模型看到了什么、为什么"的开发者。
> 相关：[多 Agent 与会话](./agents.md)（会话模型与工作区通道）、[Agent 循环](./agent-loop.md)（上下文的消费方）、[配置持久化](./config-persistence.md)（会话文件布局）、[多模态输入](./multimodal.md)（媒体引用还原）。

每个 agent 的"记忆"由**事件溯源会话日志**（`echo-session`）承担：日志是唯一事实来源，
模型上下文与显示时间线都是它的投影。记录与可见是**单向**关系——**"模型可见 ⇒ 已记录"**：
模型能看到的每条事实必须能从日志重放；反之不成立，日志同样记录模型永远看不到的
显示层事实（推理）。可见性分工见 §可见性边界。

## TrunkStore：日志、投影与文件

- 每个 agent 独立 `TrunkStore`：**事件日志**（append-only）、**显示时间线**、
  会话持久化文件（`echo-sessions-{id}.json`，每个 persona 一份）
- `SessionEvent` 七类：`UserMessage`（带 `message_sequence`，并发分支按请求序合并）、
  `AssistantMessage`（保留 `reasoning_content` / `tool_calls`）、`ToolCall`、
  `ToolResult`（`tool_call_id` 回链 + `elapsed_ms` 耗时；`started_at_ms` 在上一跳的 `ToolCall` 上）、
  `Reasoning`（一步的思考正文；带产出时刻 `timestamp`，serde 标签 `kind = "reasoning"`）、
  `ModelStep`（一次模型往返的观测事实：`step_index` / 模型 / token / 请求→响应
  `elapsed_ms` / 本步 `tool_call_ids`；serde 标签 `kind = "model_step"`，wire
  kind 为 `step/model`）、
  `Compaction`（压缩是显式事件，日志保持可重放；`[历史摘要]` 前缀在投影期恰好渲染一次）
- `ModelStep` 是**观测缺口的补丁**：此前日志只有逐个 `ToolCall`，
  没有"这一响应里带了几个工具"的事实，审计只能用相邻 `started_at_ms` 间隔 <1s 猜
  同轮打包（实测 95% 的响应被判定为单调用轮，无法区分"模型只发一个"与"框架丢弃
  并行"）；它还带每步 token 与往返延迟，替代此前只能从 `echo-usage.jsonl` 全局汇总
  的口径。事件由 harness 在 `step/model` 总线事件里落库——`ModelResponse` 现在携带
  `tool_calls` 全集，`AgentRequest` 观察者记起点算耗时
- 推理落成事件是为了保住时序：显示历史由日志派生，推理只活在实时窗口时，
  历史快照与窗口形状不对称，刷新后"思考条目堆到正文之后"无法根治。落盘前按
  `MAX_REASONING_EVENT_CHARS` 截断（2 万字/步）——日志是整份 JSON、30s 一次
  整体重写，不封顶时少数超长推理就能把文件撑到几十 MB
- 该变体是向后兼容的增量：**不升日志格式版本**，旧日志没有 `Reasoning` 事件照常解析
  （读取侧按缺省处理，见 §显示时间线）
- `insert_after_sequence` 合并并发分支回复时跳过本轮的工具往返、推理**与
  `ModelStep`**——最终回答始终排在整轮之后（否则显示历史里回答会出现在
  "思考"/步骤事件之前，且会切断 `ToolCall` → `ToolResult` 的配对合成）
- `SessionHeader` 携带 fork/resume 元数据（parent/seed_length/origin/delegation_depth），随日志持久化
- 持久化：v6 整档 JSON（`events` 为权威，每事件带 `session` 归属；
  `identities`/`timeline` 为持久化投影；v6 起**不再存 `trunk_histories`**，加载时按 `events` 重投影），tmp+rename 原子写；
  30s 周期保存（dirty 时）+ 关闭时 flush；**v5 及更旧**格式加载时自动迁移（v1-v4 先经事件化迁移再按 `session` 归因，写回即 v6）——口径与触发条件见 [配置持久化](./config-persistence.md)「Session 持久化」

## 可见性边界

同一份日志被三类投影消费，各自可见的事件种类不同——**事件落库不等于所有消费方都能看到**：

| 事件 | 模型上下文 | 显示历史 / 时间线 | 压缩转录 |
|---|---|---|---|
| `UserMessage` | ✓（媒体引用在出口还原） | ✓ | ✓ |
| `AssistantMessage` | ✓（正文 + `tool_calls`） | ✓（空正文的纯工具轮不落条目） | ✓（正文） |
| `ToolCall` / `ToolResult` | ✓（严格配对，悬空调用补合成错误结果） | ✓（工具行，按 `tool_call_id` 就地更新） | ✓ |
| `Reasoning` | — | ✓（独立 `reasoning` 条目，与工具行真实交错） | — |
| `ModelStep` | — | —（审计事实，不造条目；实时侧也不发） | — |
| `Compaction` | ✓（`[历史摘要]` 渲染一次） | ✓（「上下文已压缩 · 替换 N 条事件」系统行） | ✓（`[更早摘要]`） |

- 推理对模型不可见：它是中间草稿，对"接下来该写什么"没有边际信息量，体积却常是
  正文的数倍——放进请求只烧预算。`project_messages` 直接跳过该变体，也不触发
  tool_call 缓冲的 flush（推理处在一步开头，夹在 `ToolCall` 与其结果之间也不会打断配对）
- 推理对显示可见：只有落成事件，历史回填才能与实时投影同形状，前端不必为推理
  保留"只存在于实时窗口"的特殊路径
- 推理不进压缩转录：与"assistant 消息的 `reasoning_content` 从不渲染"一致，
  避免思考正文吃满 transcript 预算
- 这层不对称是**有意设计**而非遗漏：以后新增"派生不含"的条目种类时，必须同步把
  它排除在历史水位线之外（见 §显示时间线），否则实时条目会被误判成"快照已覆盖"

## 模型上下文（投影）

- 模型上下文 = 事件日志的投影（`derive_messages` / `derive_messages_for` /
  `derive_messages_from_filtered` / `project_messages`）：
  多会话按 `session` 归属过滤后**逐会话投影**，token 预算逐会话生效；
  裁剪只在投影期（`trim_to_budget`），绝不破坏日志
- 推理不进上下文：`project_messages` 遇 `Reasoning` 直接跳过（见 §可见性边界）；
  `ModelStep` 同口径
- **不可见事件不重建投影**：`SessionEvent::affects_projection()` 为假的变体
  （`Reasoning` / `ModelStep`）追加时跳过 `reproject`（只物化会话句柄 + 标脏）——投影
  重建是 O(本会话事件数) 的整份克隆，观测事件（每步一条）不该摊这份成本
- **投影切片**：切片键是事件自带的 `session` 归属。单次 append 只重投影本会话——
  `append_event` → `reproject(Some(id))` → `EventLog::events_for_session` →
  `install_projection`，克隆量 O(本会话) 而非 O(全部会话)；全量路径只用于加载 /
  压缩 / 未归因事件（`session = None` 的旧事件不属于任何会话，`reproject_all` 与
  `install_projection_from_log` 取一次全量后在内存里逐会话切分）；持久化序列化走
  `with_events` 持锁只读，不克隆整份日志
- 不同会话的上下文互不可见（QQ 私聊 / QQ 群 / 本地对话是独立对话；
  本地工作区通道见 [多 Agent 与会话](./agents.md)§工作区会话与项目通道）
- 发往 LLM 前在**投影出口**把媒体引用还原为 data URI（见 [多模态输入](./multimodal.md)）
- 并发分支：`insert_after_sequence` 把分支回复插入对应请求事件之后，投影顺序 = 请求到达顺序

## 压缩与归档

- 压缩三件套：`compact_preview`（只读预览）→ `archive_snapshot`（压缩前快照）→
  `apply_compaction`（落地为 `Compaction` 事件）
- 摘要为 LLM 交接摘要（逐会话组生成，固定四节；失败回退规则统计文案）；
  落地在单次取锁内完成读-改-写（`EventLog::transform`——窗口期并发 append
  要么并入保留尾、要么落在替换之后，不再被克隆快照整体覆盖丢弃），且
  **替换切片 = 预览值**（窗口期新到事件全部保留在尾部，不产生无摘要的
  静默丢档）；外加压缩互斥锁（double compact / 与 clear 竞争均被排除）
- 压缩转录逐事件取文（`render_with_caps`）：推理**不进**转录（与 §可见性边界一致），
  但 `replaced_count` 仍按事件条数统计（含推理）——"已压缩 N 条事件"贴合事实，
  同样的 `keep_recent` 覆盖的**轮次**相应变少，语义仍是按事件计
- 归档：手动 `ArchiveHistory` 与压缩前自动快照都写入
  `archives/{stem}-{ts}[-precompact].json`（v6 完整快照；数据来自**内存
  快照**——不复制可能落后一个保存周期的磁盘文件）；保留最新 20 份
  （最旧先删；文件名匹配锚定时间戳段，防跨 stem 误删）；
  **恢复** = 把归档文件复制回原会话文件名并重启 Core
- `ArchiveHistory` 要求 `session_id`：仅归档并清空该会话
  （其他会话不受影响）；缺失 = **拒绝**（迁移期 fail-closed——旧语义的
  降级执行是"整档归档 + 全量清空"，误清代价不可恢复）
- 命令入口：`RequestContext`（快照）/ `CompactHistory`（按会话压缩）/
  `ClearHistory`（按会话清理；`session_id` 缺省 = 全部会话）/ `ArchiveHistory`（归档）

## 显示时间线

- 与模型上下文同源，额外携带来源/工具/推理元数据；条目级 `seq` 支持**增量同步**
  （`since_seq`）——切换 agent / 重连只传增量
- **历史回填**：`RequestSessionHistory` 的 `SessionHistory` 按会话从事件日志派生
  （`display_history_for_session`）——推理是独立 `reasoning` 条目，按事件自带的
  `timestamp` 排序（旧数据缺失时退回 carry-forward 时间），与工具行真实交错；
  快照与实时窗口形状一致，刷新 / 切会话不再丢思考、也不再堆到正文之后
- **合并水位线**：`SessionHistory.max_seq` = `history_watermark`——该会话中
  **派生会覆盖的 kind** 的最大条目 `seq`（显示端按 `seq ≤ 水位线` 判"快照已覆盖"）。
  水位线**排除 reasoning**：旧日志没有 `Reasoning` 事件，计入会让那些会话的实时
  推理被判成"已有"而整条消失（"看不到思考 chip"）；显示端另有"推理永不按 seq
  过滤"的独立防线，两处判据刻意互不依赖
- 工具条目就地更新（完成态带计时字段）；发往前端的快照做**推理瘦身**
  （近 40 条保全文、更早截断），持久化不动（落盘按步封顶 2 万字，截断处有标注）
- 加载恢复：`timeline_seq` 从条目最大 seq 重建；悬空 running 工具条目标注"已中断"

> 在架构中的位置见 [架构总览](../overview/architecture.md)§会话与持久化；持久化文件布局见 [配置持久化](./config-persistence.md)§Session 持久化。
