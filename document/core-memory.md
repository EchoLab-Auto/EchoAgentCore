---
id: memory
title: "会话记忆"
group: 框架
x: 960
y: 480
---

# 会话记忆

每个 agent 的"记忆"由**事件溯源会话日志**（`echo-session`）承担：日志是唯一事实来源，
模型上下文与显示时间线都是它的投影——**"模型可见 ⟺ 已记录"**。

## TrunkStore：日志、投影与文件

- 每个 agent 独立 `TrunkStore`：**事件日志**（append-only）、**显示时间线**、
  会话持久化文件（`echo-sessions-{id}.json`，每个 persona 一份）
- `SessionEvent` 五类：`UserMessage`（带 `message_sequence`，并发分支按请求序合并）、
  `AssistantMessage`（保留 `reasoning_content` / `tool_calls`）、`ToolCall`、
  `ToolResult`（`tool_call_id` 回链；带 `started_at_ms`/`elapsed_ms` 执行计时，2026-10）、
  `Compaction`（压缩是显式事件，日志保持可重放；`[历史摘要]` 前缀在投影期恰好渲染一次）
- `SessionHeader` 携带 fork/resume 元数据（parent/seed_length/origin/delegation_depth），随日志持久化
- 持久化：v6 整档 JSON（`events` 为权威，每事件带 `session` 归属；
  `trunk_histories`/`identities`/`timeline` 为持久化投影），tmp+rename 原子写；
  30s 周期保存（dirty 时）+ 关闭时 flush；v1-v4 旧格式加载期自动迁移

## 模型上下文（投影）

- 模型上下文 = 事件日志的投影（`derive_messages` / `project_messages`）：
  多会话按 `session` 归属过滤后**逐会话投影**，token 预算逐会话生效；
  裁剪只在投影期（`trim_to_budget`），绝不破坏日志
- 不同会话的上下文互不可见（QQ 私聊 / QQ 群 / 本地对话是独立对话；
  本地工作区通道见 [多 Agent 与会话](./core-agents.md)§工作区会话与项目通道）
- 发往 LLM 前在**投影出口**把媒体引用还原为 data URI（见 [多模态输入](./core-multimodal.md)）
- 并发分支：`insert_after_sequence` 把分支回复插入对应请求事件之后，投影顺序 = 请求到达顺序

## 压缩与归档

- 压缩三件套：`compact_preview`（只读预览）→ `archive_snapshot`（压缩前快照）→
  `apply_compaction`（落地为 `Compaction` 事件）
- 摘要为 LLM 交接摘要（逐会话组生成，固定四节；失败回退规则统计文案）；
  应用期单次取锁整体替换 + 压缩互斥锁（double compact / 与 clear 竞争均被排除）
- 归档：手动 `ArchiveHistory` 与压缩前自动快照都写入
  `archives/{stem}-{ts}[-precompact].json`（v6 完整快照）；保留最新 20 份（最旧先删）；
  **恢复** = 把归档文件复制回原会话文件名并重启 Core
- 命令入口：`RequestContext`（快照）/ `CompactHistory`（按会话压缩）/
  `ClearHistory`（清空该 agent 全部会话）/ `ArchiveHistory`（归档）

## 显示时间线

- 与模型上下文同源，额外携带来源/工具/推理元数据；条目级 `seq` 支持**增量同步**
  （`since_seq`）——切换 agent / 重连只传增量
- 工具条目就地更新（完成态带计时字段）；发往前端的快照做**推理瘦身**
  （近 40 条保全文、更早截断），持久化不动
- 加载恢复：`timeline_seq` 从条目最大 seq 重建；悬空 running 工具条目标注"已中断"

> 机制全景与不变量见 [架构总览](./architecture.md)§会话与持久化；持久化文件布局见 [配置持久化](./core-config-persistence.md)§Session 持久化。
