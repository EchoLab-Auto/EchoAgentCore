# opencode 上下文压缩机制调研

> **归档说明**：本文是对 opencode（外部开源编码 agent）上下文压缩机制的源码级调研报告，供 EchoAgent 会话压缩设计参考。
> 调研时间：2026-10-08 ｜ 上游快照：`anomalyco/opencode` @ `5d9cd9b`（版本 1.18.35）｜ 方法：源码精读（v1 运行版 + v2 重写版）+ 测试用例核对，结论附 file:line。
> 配套文件：[source-map.md](./source-map.md)——关键源码位置索引（按机制组织）。

---

## 0. 总览：两级机制 + 两种触发 + 两代实现

| 机制 | 做什么 | 是否调 LLM | 默认开关 |
| --- | --- | --- | --- |
| **prune（修剪）** | 清空旧工具调用输出（内容置为 `[Old tool result content cleared]`） | 否 | 默认 **关**（`compaction.prune=false`） |
| **compaction（压缩）** | 用 LLM 把旧对话摘要成结构化 Markdown，保留近期尾部原文 | 是（专用 compaction agent） | 默认 **开**（`compaction.auto=true`） |

**触发路径**（v1 `packages/opencode`，实际运行版）：
1. **主动（两处）**：流处理中收到 usage 事件即实时检查 `isOverflow()`（`processor.ts:490-496`）；下一请求前用上一条 assistant 消息的 usage 再检查一次（`prompt.ts:1158-1167`）
2. **被动**：provider 返回 ContextOverflowError 时，processor 置 `needsCompaction` 返回 "compact"（`processor.ts:629,693`），主循环补建压缩任务
3. **手动**：TUI `<leader>c` → `POST /session/:id/summarize` → `compactSvc.create(auto:false)`（`httpapi/handlers/session.ts:273-292`）

**v2**（`packages/core`，Effect 架构重写中）：独立 `SessionCompaction` 模块（`core/src/session/compaction.ts`，248 行），发布 `Compaction.Started/Ended` 事件，配合「Context Epoch」设计（仓库根 `CONTEXT.md`）。

---

## 1. 触发条件（v1 `overflow.ts`）

```ts
COMPACTION_BUFFER = 20_000
usable = limit.input 存在 ? limit.input - min(20k, maxOutput)
                         : context - maxOutput
isOverflow = count >= usable    // count = total || input+output+cache.read+cache.write
```
- `count` 用 **provider 上报的真实 usage**（含缓存读写），不是估算
- `reserved` 可配置（`compaction.reserved`），预留防溢出窗口
- `compaction.auto=false` 直接禁用；summary 消息自身不再触发（防递归，`prompt.ts:1158`）
- 已知边界：对带独立 input 上限的模型（如开启 prompt caching 的 Claude），headroom 逻辑有差异——仓库内有 3 个标记为 "BUG:" 的复现测试（`compaction.test.ts:455-545`，issue #10634/#8089/#11086/#12621）

## 2. 选段算法：摘要谁、保留谁（`compaction.ts` select，223-269 行）

- **保留预算**：`preserve_recent_tokens ?? clamp(floor(usable × 0.25), 2000, 15000)`
- **turn 粒度倒扫**：从最新 user turn 往回累加估算（懒估算：只估要保留的尾段，成本与尾段成正比），装得下就继续
- **turn 内分裂**（`splitTurn`）：整个 turn 装不下时，在 turn 内按消息边界找能塞进剩余预算的后缀
- **兜底**：连一个 turn 都保不住 → 全量摘要（无尾巴），打 "tail fallback" 日志
- `tail_turns` 配置可限制候选最近 turn 数（=0 时全量摘要）
- **估算口径**：`toModelMessagesEffect` 序列化后的 JSON 长度 / 4（`Token.estimate`，4 chars/token，`util/token.ts`）

## 3. 摘要生成（专用 agent + 结构化模板）

- **agent**：`compaction`，hidden、`tools:{}`（生成摘要时无工具，测试保证）、prompt `compaction.txt`（"你是上下文摘要代理……不要继续对话……用对话同语言回答"）
- **模型**：compaction agent 配置的模型，默认用会话当前模型（`compaction.ts:358-361`）
- **输入序列化**（`serialize`，54-85 行）：`[User]:` / `[Assistant]:` / `[Assistant reasoning]:` / `[Assistant tool call]: name(input)` / `[Tool result]:`（截断 2000 字符）/ `[Tool error]:`；已 prune 的显示 `[Old tool result content cleared]`
- **输出模板**（`core/src/session/compaction.ts:16-46`，v1/v2 共用 `buildPrompt`）：
  - `## Objective` / `## Important Details` / `## Work State`（Completed/Active/Blocked）/ `## Next Move` / `## Relevant Files`
  - 规则：所有小节保留、短横线子弹体、**保留精确路径/符号/命令/错误串**、"不得提及摘要过程"
- **增量合并**（重复压缩）：带 `<prior-summary>` + 新对话 → 合并指令（冲突以新对话为准、未完成项结转）；旧摘要对模型隐藏不参与选段（测试："ignores previous summaries when sizing the retained tail"）
- **摘要输出上限** 4096 tokens（`SUMMARY_OUTPUT_TOKENS`）

## 4. 回注与模型视图重组（关键机制）

**v1 `filterCompacted`**（`message-v2.ts:534-585`）：把消息流重排为模型可见视图

```
[compaction 用户消息, 摘要(assistant), ……保留的尾部原文…, ……尾部之后的新消息…]
```

- compaction 用户消息的 part 转成文本 **"What did we do so far?"**（`message-v2.ts:238-243`）——即模型看到的是「问：我们做了什么？答：<摘要>」的自然问答锚点
- `tail_start_id` 持久化在 compaction part 上（`compaction.ts:461-466`），filter 按其切段；旧头部整体丢弃（存储仍在，模型不可见）
- 每次构建请求都走该函数（`prompt.ts:1092`），保证「摘要 + 尾部 + 新消息」而非全史

**v2**（更彻底）：压缩产出为一等公民 **compaction 消息**（存 `summary` + `recent` 尾部文本，`message-updater.ts:377-390`）；`SessionHistory` 加载时直接**按 seq 截断**——`seq >= latestCompaction.seq` 的消息才可见（`history.ts:31-61`）。压缩同时结束当前「Context Epoch」（provider 前缀缓存的不可变基线），下一个 epoch 重建基线（`context-epoch.ts` / `CONTEXT.md`）。

## 5. 溢出专项处理（`compaction.ts:340-356, 468-549`）

- **重放**：被动溢出（`overflow:true`）时，找压缩提示前最近一条真实用户消息 → 压缩后重建该消息（媒体附件降级为 `[Attached mime: name]` 文本，这正是溢出常见肇因），让 agent 重新回答
- **自动续跑**：压缩完注入 synthetic 用户消息 "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed."（metadata `compaction_continue:true`，插件 `experimental.compaction.autocontinue` 可关）
- **压缩自身放不下**：报 `ContextOverflowError`——"Conversation history too large to compact…" / "Session too large to compact…even after stripping media"
- 压缩全程有事件（`session.compacted`；v2 还有 Started/Ended 带 summary+recent）

## 6. prune 工具输出修剪（`compaction.ts:271-317`）

- 触发：每轮结束 fork 执行，`compaction.prune=true` 才跑（默认关）
- 保护窗口：最近 **2 个 user turn** 整段不碰；再往前，最新 **~40k tokens** 的工具输出保留（`PRUNE_PROTECT`）
- 清除条件：可清量 > **20k tokens**（`PRUNE_MINIMUM`）才批量标记
- 手段：`part.state.time.compacted = Date.now()`——只标记，不改存储；渲染时输出替换为 `[Old tool result content cleared]`
- 例外：`skill` 工具输出永不修剪；遇到 summary 边界停止（幂等）

## 7. 配置与扩展点

```jsonc
// opencode.json
"compaction": { "auto": true, "prune": false, "reserved": 10000,
                "preserve_recent_tokens": 8000, "tail_turns": 3 }
```
- 环境变量：`OPENCODE_DISABLE_AUTOCOMPACT` / `OPENCODE_DISABLE_PRUNE`
- 插件钩子：`experimental.session.compacting`（注入 context 或整体替换压缩提示）、`experimental.compaction.autocontinue`
- v2 迁移映射：`preserve_recent_tokens → keep.tokens`，`reserved → buffer`（`config/v2-compat.ts`）

## 8. 行为保证（测试用例摘录，`compaction.test.ts`）

36+ 用例锁行为，代表性用例：
- "summarizes only the head while keeping recent tail out of summary input"
- "shrinks retained tail to fit preserve token budget" / "retains a split turn suffix…" / "falls back to full summary…"
- "anchors repeated compactions with the previous summary" / "keeps recent pre-compaction turns across repeated compactions"
- "replays the prior user turn on overflow when earlier context exists"
- "adds synthetic continue prompt when auto is enabled" / "allows plugins to disable…"
- "does not allow tool calls while generating the summary"
- "marks summary message as errored on compact result"
- "compacts old completed tool output" / "skips protected skill tool output"

## 9. 对 EchoAgent 的借鉴点

1. **「结构化摘要模板 + 保留精确标识符」**：Objective/WorkState/NextMove/Files 五段式 + "不得提及压缩过程" + 同语言，比自由摘要稳定
2. **摘要 + 尾部原文 双保留**：只留摘要丢近期细节、只保留原文省不了多少；用「预算驱动的 turn 级倒扫 + turn 内分裂」精确控制保留量
3. **视图重组而非删除**：存储不动，只在「构建模型输入」时重排/截断（v1 filterCompacted / v2 seq 截断），可回放、可调试
4. **问答锚点**：把「摘要」伪装成 "What did we do so far?" 的问答对，比裸塞 system 摘要更少干扰模型行为
5. **压缩专用 agent + 无工具 + 4096 上限**：防止压缩过程自身触发工具/爆预算
6. **增量摘要合并**：多轮压缩不丢目标——旧摘要+新对话→新摘要，冲突以新为准
7. **工具输出先 prune 后压缩**：两级渐进（便宜的先做），prune 默认关但开关就绪
8. **溢出后重放用户消息 + 自动续跑**：压缩完 agent 不"卡死"，且不丢用户原始诉求
9. **失败兜底全部显式**：压缩放不下 → 明确报错；压缩事件全程可观测——避免「压缩静默失败、上下文悄悄丢失」类问题（EchoAgent 设计会话压缩时的重点对照项）

## 10. 复现步骤

```bash
git clone --depth 1 https://github.com/anomalyco/opencode.git
cd opencode && git checkout 5d9cd9b   # 调研时快照
# 关键路径见 source-map.md；v1 运行版在 packages/opencode/src/session/
```
