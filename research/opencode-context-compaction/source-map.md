# 源码索引（opencode 上下文压缩）

> 快照：`anomalyco/opencode` @ `5d9cd9b`（2026-10-08）。路径省略仓库根前缀。
> v1 = `packages/opencode/`（实际运行版）；v2 = `packages/core/`（Effect 架构重写中）。

## 机制 → 关键位置

| 机制 | 位置 | 说明 |
| --- | --- | --- |
| **溢出判定** | `src/session/overflow.ts:10-33` | `usable():10`（预留 `min(20k, maxOutput)`）、`isOverflow():22`（用 provider 上报 usage） |
| **保留预算** | `src/session/compaction.ts:115-120` | `clamp(usable×0.25, 2k, 15k)`，可配 `preserve_recent_tokens` |
| **选段（turn 倒扫 + 分裂）** | `src/session/compaction.ts:223-269`（select）、`122-163`（turns / splitTurn） | 懒估算；`tail_turns` 可限；保不住时 tail fallback |
| **序列化（对话→文本）** | `src/session/compaction.ts:54-85` | `[User]:` / `[Assistant tool call]: name(input)` / `[Tool result]:`（截断 2000 字符） |
| **摘要模板（五段式）** | `packages/core/src/session/compaction.ts:16-46` | Objective / Important Details / Work State / Next Move / Relevant Files；v1 经 `buildPrompt` 复用 |
| **增量合并指令** | `packages/core/src/session/compaction.ts:47-55` | `<prior-summary>` + 新对话 → 新摘要；冲突以新对话为准 |
| **摘要生成（agent + 调用）** | `src/session/compaction.ts:358-448`（processCompaction） | 无工具、4096 输出上限、事件发布 |
| **compaction agent 定义** | `src/agent/agent.ts:220-233` | hidden、`tools:{}`、prompt = `src/agent/prompt/compaction.txt` |
| **视图重组（v1）** | `src/session/message-v2.ts:534-585` | `filterCompacted`：重排为 [压缩消息, 摘要, 保留尾部, 新消息] |
| **问答锚点** | `src/session/message-v2.ts:238-243` | compaction part 渲染为文本 "What did we do so far?" |
| **尾部切点持久化** | `src/session/compaction.ts:461-466` | `tail_start_id` 写入 compaction part |
| **溢出重放用户消息** | `src/session/compaction.ts:340-356, 468-549` | 媒体附件降级为文本；自动续跑 synthetic 消息 |
| **prune（工具输出修剪）** | `src/session/compaction.ts:271-317` | 保护 2 turns + 40k tokens；清除量阈值 20k；skill 除外 |
| **pruned 渲染** | `src/session/message-v2.ts:303-305` | `[Old tool result content cleared]` 替换 |
| **主循环接线** | `src/session/prompt.ts` | `1092`（filterCompacted）、`1149-1156`（压缩任务）、`1158-1167`（主动触发）、`1320-1327`（被动触发）、`1338`（prune fork） |
| **processor 溢出处理** | `src/session/processor.ts:490-496, 613-635, 693` | 流中收到 usage 实时检查（490-496）；ContextOverflowError → `needsCompaction`（629）→ 返回 "compact"（693） |
| **token 估算** | `src/util/token.ts` | `length / 4`（CHARS_PER_TOKEN=4） |
| **手动触发（summarize 路由）** | `src/server/routes/instance/httpapi/handlers/session.ts:273-292` | `compactSvc.create(auto:false)` + `promptSvc.loop` |
| **配置 schema / 开关** | `packages/core/src/v1/config/config.ts:149-168`；`src/config/config.ts:594-597` | auto / prune / tail_turns / preserve_recent_tokens / reserved；env 开关 |
| **v2：seq 截断加载** | `packages/core/src/session/history.ts:13-80` | `latestCompaction():13` + `messageRows():31`（只加载压缩点之后）；`load():66` |
| **v2：Compaction 一等消息** | `packages/core/src/session/message-updater.ts:377-390` | 存 `summary` + `recent` 尾部文本 |
| **v2：Context Epoch** | `packages/core/src/session/context-epoch.ts`；`CONTEXT.md` | 压缩结束 provider 前缀缓存基线；下一 epoch 重建 |
| **v2：事件** | `packages/schema/src/session-compaction-event.ts` | `session.compacted`（另有 Started/Ended 带 summary+recent） |

## 测试与文档

| 类别 | 位置 | 说明 |
| --- | --- | --- |
| 行为测试 | `test/session/compaction.test.ts` | 36+ 用例（isOverflow:382、create:566、prune:626、process:814）；BUG 复现 455-545 |
| 回滚交互 | `test/session/revert-compact.test.ts` | 压缩与 revert 的清理、消息边界 |
| 用户文档：配置 | `packages/web/src/content/docs/config.mdx:747-765` | `compaction` 选项说明 |
| 用户文档：插件钩子 | `packages/web/src/content/docs/plugins.mdx:337-395` | `experimental.session.compacting` 注入/替换 |
| 用户文档：agent | `packages/web/src/content/docs/agents.mdx:95-100` | compaction agent 说明 |
| v2 术语表 | 仓库根 `CONTEXT.md` | System Context / Context Epoch / Safe Provider-Turn Boundary 等 |

## 本报告文件

- [README.md](./README.md)——调研报告主文
- [source-map.md](./source-map.md)——本文件
