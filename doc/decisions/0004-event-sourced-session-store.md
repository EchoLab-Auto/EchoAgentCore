# ADR-0004: 事件溯源会话存储(echo-session)

状态: accepted

## 问题

会话持久化(`echo-sessions.json` v4)直接保存 `trunk_history`(模型上下文
投影)与 display timeline 两份状态,存在三个缺陷:

1. **工具结构丢失**:`serialize_messages` 只序列化 `role`/`content`/
   `reasoning_content`,`tool_calls`/`tool_call_id` 被丢弃——重启后工具循环
   的消息降级为纯文本,"模型可见 ⟺ 已记录"被破坏(Phase 0 基线测试冻结了
   这一现状)。
2. **破坏性裁剪**:`trim_by_tokens` 直接从历史头部删除消息,被裁信息不可
   重放;模型上下文与 timeline 各自独立裁剪/落盘,双事实来源。
3. **无 fork/resume 语义**:会话无 lineage 元数据(父会话、seed 边界、来源、
   委托深度),无法区分继承历史与子会话工作。

## 决策

新建 `echo-session` crate,实现**事件溯源会话存储**:

- **`SessionEvent`**:五类可序列化事件——`UserMessage`(含 `message_sequence`
  供并发分支按请求序合并)、`AssistantMessage`(完整保留 `reasoning_content`
  与 `tool_calls`)、`ToolCall`、`ToolResult`(带 `tool_call_id` 回链)、
  `Compaction`(摘要替换前缀,append-only 的显式压缩)。
- **`EventLog`**:append-only 事件日志,整档 JSON 持久化(version=5,
  tmp+rename 原子写);`insert_after_sequence` 把并发分支的回复插入对应请求
  事件之后,保持投影顺序 = 请求到达顺序。
- **`derive_messages` / `project_messages`**:唯一的模型上下文投影;裁剪只
  在投影期(`trim_to_budget`),绝不破坏日志;compaction 事件覆盖其前缀事件。
- **`SessionHeader`**:fork/resume 元数据(`parent_session`/`seed_length`/
  `origin`/`delegation_depth`),持久化随日志。
- **`legacy::migrate_v4_document`**:v1(数组)/v2(shared_context)/v3(单
  trunk)/v4(trunk+timeline)全部兼容迁移进事件日志;旧工具结构缺失如实保留
  (迁移后的工具消息降级为纯文本,与新写日志不同——新日志保留结构)。

接入:`TrunkStore` 持有 `EventLog` 作为权威,`trunk_history` 降级为内存投影
缓存(`append_event` 在锁内 append 事件并重新投影);Agent 写入点
(`record_incoming_and_snapshot`/`register_incoming_branch`/
`record_assistant_reply`/`run_tool`)全部改为 append 事件;持久化 v5 以
`events` 为权威,旧格式读入自动迁移;组合根设置顶层级 `SessionHeader`。

## 备选方案

- **维持 v4 双存储**:工具结构继续丢失,不满足"模型可见 ⟺ 已记录"。
- **每会话独立日志文件**:当前是全局单 trunk 模型,per-session 日志需先
  拆分会话边界(Phase 4 TurnRunner 时再演进),此阶段保持单 trunk 语义。
- **Compaction 直接删事件**:违反 append-only;删除不可重放,摘要无法追溯。

## 后果

- 工具调用/结果现在是持久化的模型可见事实:重启后 `derive_messages` 重建
  完整上下文(集成测试 `event_log_persists_tool_structure_across_reload`
  冻结此行为)。
- 模型上下文顺序由事件顺序决定:并发分支经 `insert_after_sequence` 合并,
  投影与请求到达顺序一致(原 `record_assistant_reply` 的 history 定位插入
  逻辑被事件日志接管)。
- 行为变化:模型上下文现在包含工具消息(旧格式丢弃它们)——2 个 agent 测试
  随行为更新断言(工具消息进入 history)。
- 持久化格式升至 v5,旧 v1–v4 文件兼容读入并迁移;Phase 0 基线测试
  (`baseline_tool_message_structure_is_lost_on_roundtrip`)在新格式下改写为
  "断言保留"(echo-session 事件 roundtrip 测试承担该契约)。
- `TrunkStore` 仍是全局单 trunk;per-session 日志与 fork/resume 的完整接入
  随 Phase 4(会话边界拆分)推进,`SessionHeader` 已定义并持久化。
