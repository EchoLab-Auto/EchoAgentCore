---
id: panel-chat-history
title: "历史快照与实时窗口"
group: 前端
x: 5120
y: 1340
---

# 历史快照与实时窗口

> **定位**：消息列的两个数据来源（实时窗口与权威快照）如何合并、去重与落位，条目身份 `entry` 如何把启发式判据换成精确命中，渐进式分页与常驻上限，以及刷新首屏的缓存还原与落盘成本。读者：维护 `state.ts` / `state_domains/timeline.ts` / `trunk-cache.ts` 的开发者。上游：[会话消息区](./panel-chat-messages.md)。

## 两个副本

消息列有两个来源，合并规则决定「推理/工具方块落在哪」（全局视图直接读实时窗口，不做合并）：

| 副本 | 来源 | 特征 |
|---|---|---|
| 实时窗口 `state.trunk` | WebSocket 事件的即时投影（`AgentReasoning*` / `ToolCall` / `AgentOutput` …） | 顺序即真实时序；**不带条目级 seq**（事件只有帧级序号）；实时消息播入场动画 |
| 权威快照 `state.sessionHistory` | Core 从事件日志派生（`history.rs::display_history_for_session`），按 `(core, team, session)` 键缓存 | 条目自带 seq；`baseSeq` 优先取 Core 下发的 `max_seq`（派生水位线，已排除推理），缺失时回退按窗口最大 seq；快照消息不播动画 |

## 落位与去重

- **落位（插进时序，不追加到末尾）**：`state.ts::sessionMessagesFor` 先铺快照，再按实时窗口顺序合并——工具行命中则就地修补并把插入游标移到其后，命中快照已有条目只前移游标，新条目 `splice` 到游标处；游标尚未建立（前驱条目已被 1024 条窗口挤出）时按时间兜底插入（`timeInsertIndex`；快照里 backend 的时间来自 Core 的 carry-forward，可能偏早，故只作兜底）。一律 append 的结果就是推理/工具方块全落到主回复后面，刷新重建才恢复
- **水位线只对非推理生效**：`seq > 0 && seq <= baseSeq` 才视为「快照已覆盖」。推理**永不**按 seq 过滤——旧日志没有 `SessionEvent::Reasoning`（派生不含推理，却在算 `baseSeq` 时统计了推理条目的 seq，实时推理会自己把自己判成已覆盖而整条消失），且线上瘦身让两侧文本无法「完全相等」。Core 侧 `history_watermark` 同样排除 `reasoning` kind，两处判据刻意互不依赖（双保险）
- **去重判据（宁可重复显示，不丢内容）**：`indexCoveredBySnapshot` 要求**同角色 + 同时间 + 内容兼容**——推理剥掉截断标记后比前缀（`reasoningCompatible`）、backend 允许互为前缀（流式副本是终值的前缀）、工具要求同名且快照已是完成态；水位线内的条目走 `strict` 只认完全相等（防「内容更全的新回答」被前缀判据吞掉），推理不参与 strict
- **权威条目认领实时孪生**：`state_domains/timeline.ts::mergeTimelineEntry` 让 TrunkTimeline 增量先按 `tool_call_id` 修补工具行，否则认领 trunk 里的实时孪生（seq=0、同角色、时间 ±2s、内容兼容）就地升级并**保留 key**（Vue keyed 身份与入场动画不跳），都失败才追加。认领时**不复制 `incoming.seq`**：seq 只表示「来自权威序列」，补给实时推理会让它被 `baseSeq` 判成已覆盖而整条消失（思考 chip 不可见）；保持 0 = 「实时副本、快照未覆盖」，去重交给内容判据
- **回归契约**：`__tests__/timeline-dedupe.test.ts`（快照已含的思考/工具不再追加、权威条目认领孪生保留 key）、`reasoning-visibility.test.ts`（高 `baseSeq` 下实时推理仍可见、带 seq 的非推理条目仍按水位线过滤）、`reasoning-order.test.ts`（推理插在两次工具调用之间）

## 条目身份

- Core 在事件帧**信封层**下发 `entry`（`{"type":"event","payload":{…},"seq":N,"entry":"…"}`，与 `seq` 同一手法）。语义 = 同一条逻辑条目的所有事件共享同一 id：一段流式输出的全部增量与它的终值同 id，工具调用与它的结果同 id。Panel 用它把三处**启发式**换成精确命中——流式增量按 `entry` 找当前行（`appendStreamDelta`）、终值按 `entry` 收敛（`settleStream`）、工具结果按 `entry` 配对（`finishToolMessage`）；`entry` 缺失（旧版 Core）时全部回退原判据（灰度双读，`DisplayMessage.entry` 为 `undefined`）。修掉的场景：同一 branch 下工具前的中间叙述与工具后的正文曾被并进同一行；`AgentOutput.branch_id` 缺失时"取最新流式行"在并行分支下写错行；`tool_call_id` 缺失时"按工具名猜最后一个 running"在同名并发下配错。详见 [条目身份化](../design/entry-identity-p1.md)

## 渐进式分页与常驻上限

- 激活会话首拉 **300** 条（渲染窗口 200 + 余量），用户点「加载更早」把窗口扩到已加载量之外时再 +300（`ChatView` 的 `HISTORY_STEP` / `historyLimit`）；入口在「本地还有未渲染」**或**「Core 侧还有更早」（`SessionHistory.total > 已加载`，见 `olderInCore`）时都显示——后者保证用户永远够得到更早的历史（此前固定 1000 条，而渲染只用 200，切会话白拉 5 倍量，实测 33 ms）。会话历史快照常驻上限 **12** 份，写入方视为「最近使用」、淘汰最旧的**非本次**会话（`state.ts::pruneSessionHistory`）
- **时间线分页**：首屏只收最新 **200** 条（`RequestTrunkTimeline { limit: TIMELINE_INITIAL_WINDOW }`；实测全量 1024 条的转换 + 传输约 7 ms 且随条目增长），响应 `TrunkTimeline.has_more` 落到 `state.timelineHasMore[teamKey]`。用户点到「加载更早」且**本地已渲染完**、会话历史也到顶时，按**已加载最老条目的 `ord`**（位置序号，push 时分配且永不改变）发 `RequestTimelineEarlier { before_ord, limit }`；`TimelineEarlier` 的 reducer 把该页**前插**到 `teamTimelines[key].msgs`（当前 team 同时前插 `state.trunk`），并**不动同步游标**（`teamTimelines[key].seq` 仍指最新进度——写成老条目的 seq 会让下次增量同步重复拉整段）。前插自带去重（只收比已加载最老条目更早的），因此重连重放/重复点击不会产生重复行。只对当前查看的 team 更新 `trunk`
- **其他常驻集合上限**：`sessions` 500 条（按 `last_active` 保留最近的）、`browseDirectories` 32 条（目录选择器的浏览结果缓存）

## 刷新与落盘

- **周期落盘的空闲零成本**：`trunk-cache` 每 10 s 检查一次，但先用廉价签名（每 team 的 `key|seq|条目数`）判断内容是否变化——未变直接返回，连 `JSON.stringify` 都不做。实测：内容变化时一次 8.4 ms（2 team × 1024 条），**内容不变时 0 次序列化 / 0 次写盘**（改前每 10 s 固定 24 ms）。落盘前还会剥掉内嵌 data URI（一次粘贴截图数百 KB，既挤配额又拖慢序列化），保留轻量的 `/media/<id>` 引用
- **刷新首屏**：页面加载先还原磁盘缓存（`trunk-cache.ts`，挂载前 hydrate——`state.teamTimelines` 按 team 快照 + 条目级游标），引导请求带 `since_seq` 只拉缺口（无缓存/窗口滚出/游标超前时 Core 自动回退全量）；缓存 10s 周期检查（写盘放 `requestIdleCallback`，不挤占渲染帧）+ 页面隐藏/卸载时立即落盘（配额满静默放弃）；**还原时就格式升级**（旧缓存里 `<subagent_event>` 用户气泡 → 子代理委派行，避免「刷新后仍是旧样式」）；**重连不清空时间线**——保留缓存并按 `since_seq` 增量补齐，响应 `full` 标志时整体替换。配套 Core 侧对发往前端的快照做**推理瘦身**：仅最近 40 条推理保留全文，更早的截断到 240 字 + 标注（持久化不改，`timeline.rs::elide_reasoning_for_wire`）
