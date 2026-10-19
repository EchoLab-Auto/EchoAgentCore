---
id: entry-identity-p1
title: "条目身份化（EntryId + rev + Upsert）"
group: 规划
x: 1600
y: 3360
link: ["panel-performance-audit | Panel 性能审计 | l>t"]
---

# 条目身份化：P1 协议草案与 Panel 双读骨架

> **状态：P1 第一步已落地（2026-10-19）**。P0（渲染窗口 / 适配前切片 / 时间标签
> memo / 工作行缓存 / 流式纯文本 / 写时复制 / 合并索引化）已落地并实测达标，
> 见 [Panel 性能审计](./panel-performance-audit.md)。P1 的**条目身份**已进代码：
> Core 侧 `core/src/entry_identity.rs` + 信封层 `entry` 字段，Panel 侧已消费
> （流式行 / 终值对账 / 工具配对三处改为精确命中，旧判据保留为回退）。
> 本文余下章节（归一化存储、游标分页、协议收敛）仍是提案，落地时以代码为准。

---

## 为什么渲染优化不够

P0 把**渲染**成本与历史长度解耦（常数 200 行窗口），但**数据**层仍是
「三份副本 + 四套启发式」：

| 现状 | 位置 | 代价 |
| --- | --- | --- |
| 同一批条目存三份 | `state.trunk`（实时窗口 1024）、`state.teamTimelines[key]`（每 agent 永久）、`state.sessionHistory[key]`（每会话 ≤1000，最多 64 份） | 内存随「逛过的会话数 × 长度」增长；重连/切会话要整份替换 |
| 第四份在磁盘 | `trunk-cache.ts` 全量 `JSON.stringify`（预算 2.2 M 字符，每 10 s） | 周期主线程尖峰，历史越长越重 |
| 对齐靠猜 | `indexCoveredBySnapshot`（role+time+内容前缀互包含）、`findToolEntryIndex`（tool_call_id，缺失退名字）、`WIRE_ELISION_MARK`（解析 Core 塞进正文的截断标记）、`baseSeq` 水位线 + `timeInsertIndex` 兜底 | `state.ts` 里 ~60 行纯启发式，每个边界场景都出过 bug |
| 深响应式 | `store.ts` 的 `reactive(initialState())`，全仓 0 处 `markRaw` | 上万条消息全在 Proxy 图里；读字段过陷阱、内存翻倍 |

**根因**：Core 的实时事件**没有身份**。`TimelineMessage` 有条目级 `seq`
（`event.rs:221`），但 `AgentContentDelta` / `ToolCall` / `ToolResult` /
`AgentOutput` 都没有——Panel 收到一条 `ToolResult` 时，无从知道它属于哪一行。
于是只能靠「同角色 + 同时间 + 内容兼容」去猜，猜不准的每个场景都变成一次
线上事故：

| 历史修复 | 实际根因 |
| --- | --- |
| `d8e862d` 工具行一直读秒 | 渲染缓存与原地变异冲突（值快照 vs 引用） |
| `798228b` 思考/工具块重复出现在回复后 | 实时副本与权威副本去重失败 |
| `33f46d1` 思考 chip 消失 | 认领实时孪生时误复制 `seq`，被水位线过滤 |
| `a680d71` 推理堆到正文后面 | 按时间兜底落位失败 |
| `362ca27` elision 兼容判据 | 线上瘦身让"同一段推理"两侧文本不等 |
| `05ca918` + `741a307` + 半死连接看门狗 | 丢帧后无法增量补齐，只能靠序号跳号探测 + 全量重拉 |

这些不是「判据不够聪明」，而是**没有身份就必然要做非法推断**。

---

## 已落地：信封层的条目身份

实施要点与设计稿的差异：**没有**给 `BackendEvent` 各变体加字段（那会让 70+
处构造点都要改），改为与 `seq` 同一手法——id 作为**顶层字段**附加在帧上：

```json
{"type":"event","payload":{"AgentContentDelta":{…}},"seq":142,"entry":"backend:s1:b1:7"}
```

| 位置 | 内容 |
| --- | --- |
| Core `core/src/entry_identity.rs` | `EntryTracker`：把"同一逻辑条目的多个事件"绑到同一 id |
| Core `management.rs` | broker 在**持锁投递之前**推进追踪器（与 `seq` 同序），随帧下发 |
| Core `bridge.rs` | `serialize_event_identified(seq, entry, ev)`：顶层插键；`None` 时不写该键 |
| Panel `connection.ts` | 解析帧顶层 `entry` → `dispatch(event, core, entry)` |
| Panel `state.ts` | `DisplayMessage.entry`；流式增量 / 终值 / 工具结果全链透传 |
| Panel `timeline.ts` | `appendStreamDelta` / `settleStream` / `finishToolMessage` 均优先按 `entry` 精确命中 |

id 规则（`EntryTracker`）：

| 事件 | entry |
| --- | --- |
| `ToolCall` / `ToolResult`（`tool_call_id` 非空） | `tool:<session>:<tool_call_id>` |
| 同上但 `tool_call_id` 为空 | 不分配（Panel 回退"同名最后一个 running"） |
| `AgentContentDelta` / `AgentOutput` | `backend:<session>:<branch>:<n>` |
| `AgentReasoningDelta` / `AgentReasoning` | `reasoning:<session>:<branch>:<n>` |

`<n>` 是进程级单调计数（全局递增，不按会话维护计数器表——避免会话数增长带来
的无界状态）。语义约束只有三条：

1. 一段流式输出的所有增量与它的**终值共享同一个 n**（Panel 据此收敛到同一行）；
2. `ToolCall` 出现即**封口**该会话的正文/推理条目（与 Panel 的 `freezeStreams`
   一致：工具开始后该步叙述不会再有终值）；
3. 工具类 id 由 `tool_call_id` 决定，与到达顺序无关（`ToolResult` 先到也不会错配）。

修掉的三类历史问题（Panel 侧 `entry-identity.test.ts` 逐条锁定）：

| 场景 | 旧启发式 | 有了 entry |
| --- | --- | --- |
| 同一 branch 下，工具前的中间叙述 + 工具后的正文 | 只比 branch → 并进同一行 | 各自 entry → 两行 |
| `AgentOutput.branch_id = null`（后端路径） | "取该会话最新流式行" → 并行分支下写错行 | 按 entry 精确收敛 |
| `tool_call_id` 缺失的工具结果 | "按工具名猜最后一个 running" → 同名并发配错 | 按 entry 精确配对 |

**灰度双读**：`entry` 缺失（旧版 Core）时三条路径全部回退到原判据，行为不变。

## 协议

### 条目身份

```rust
// echo-protocol/src/event.rs（新增）
/// 条目身份：`<node_id>:<ulid>`。跨 Core / 跨中继全局唯一，重启不变。
pub struct EntryId(pub String);

/// 条目类别（渲染分派的**唯一**依据）。
pub enum EntryKind { User, Backend, Reasoning, Tool, Subagent, System }

/// 条目状态。
pub enum EntryState {
    /// 流式输出中（内容仍会增长；Panel 走纯文本路径）。
    Streaming,
    /// 已收敛（终值；Panel 渲染 Markdown）。
    Sealed,
    Failed,
    Cancelled,
}

/// 条目类别专属载荷（tagged）。
pub enum EntryBody {
    User { source: Option<TimelineSource>, images: Vec<String> },
    Backend { images: Vec<String> },
    Reasoning,
    Tool(TimelineTool),          // 复用现有结构（name/input/output/failed/tool_call_id/计时）
    Subagent { task: String, result: String, success: bool, running: bool },
    System,
}
```

### 唯一的写入事件：Upsert

```rust
pub enum BackendEvent {
    // …既有事件保留，见 §2.5 迁移
    /// 条目新增或修订。**幂等**：同一 (entry, rev) 重复投递无副作用。
    EntryUpsert {
        /// 归属会话（键的一部分，必须先于 entry 解码可用）
        session_id: String,
        #[serde(default)] team_id: Option<String>,
        entry: String,          // EntryId
        /// 同一 entry 内单调（0 = 创建，之后每次变更 +1）
        rev: u64,
        kind: EntryKind,
        state: EntryState,
        /// **当前全量**正文（不是增量）——丢掉所有"增量拼接"状态
        content: String,
        body: EntryBody,
        /// epoch 毫秒（毫秒精度，显示排序不再依赖它，仅作展示/回放）
        at_ms: i64,
        /// 呈现提示（Core 侧就知道哪些条目属于「同一步」）
        #[serde(default)] block: Option<BlockHint>,
    },
}

/// 呈现提示：Core 明确告诉 Panel「这几条属于同一步工作」，
/// Panel 只做 kind → 组件映射，不再每次渲染全表反推分组。
pub struct BlockHint {
    /// 同一步的组键（如 turn/step 的 ULID）
    pub group: String,
    /// 默认折叠（Core 认为这组属于"过程"而非"结论"）
    pub collapsed: bool,
}
```

三条关键语义：

1. **流式 = 同一 `entry` 的多次 upsert**，不是「新开一行 + 终值对账」。Panel 的
   规则退化成 `if (prev.rev >= incoming.rev) drop;` —— 幂等、乱序安全、
   多窗口广播安全（不再需要 `state.ts:1222` 那种 `seq floor` 硬过滤）。
2. **工具调用与结果是同一 entry 的两个 rev**（`running → Sealed/Failed`）。
   Panel 侧 `findRunningTool` / 名字兜底 / `settleRunningTools` 的"回合收尾猜
   僵尸行"全部删除。
3. **`content` 永远是全量**：Panel 不做字符串拼接，也不做"前缀判等"去重。
   相应的，`elide_reasoning_for_wire`（`timeline.rs:273`）与 Panel 侧的
   `WIRE_ELISION_MARK` 一并删除——瘦身只影响显示长度，不再影响身份。

### 读取：游标分页取代全量

```rust
pub enum BackendCommand {
    /// 取窗口：`before` 为空 = 最新一页；否则取该条之前的一页（向上翻）
    RequestWindow { team_id: Option<String>, session_id: String,
                    before: Option<String>, limit: usize },
    /// 订阅：此后只推 entry 序位 > since 的 upsert
    Subscribe { team_id: Option<String>, session_id: String, since: Option<String> },
    // RequestTrunkTimeline / RequestSessionHistory 标 deprecated，保留两个版本
}

pub enum BackendEvent {
    EntryWindow {
        session_id: String,
        #[serde(default)] team_id: Option<String>,
        entries: Vec<EntryUpsertSnapshot>,  // 与 Upsert 同构（rev 为当前值）
        has_more: bool,
        oldest: Option<String>, newest: Option<String>,
    },
}
```

`since` 用**条目身份**而不是进程内 `seq`：Core 重启、游标回退、跨中继重放都
不会错位（现在的 `seq` 是 `EventBroker` 的进程内计数器，
`core/src/management.rs:33`，重启归 1）。

### 呈现排布：从反推到声明

现在 Panel 每帧都在做一件协议层的事：把连续的 tool / reasoning / subagent
合并成一组 chip（`ChatView.vue::messageRows`，每帧全数组重算）。身份化后由
**Core 声明分组**（`BlockHint.group`，写入时间线时就知道哪几条属于同一步），
Panel 只做 `kind → 组件` 的注册表映射：

```ts
// web/src/entry-kinds.ts（新增）
export const ENTRY_RENDERERS: Record<EntryKind, Component> = {
  user: MessageItem, backend: MessageItem, system: SystemLine,
  reasoning: ReasoningRow, tool: ToolRow, subagent: SubagentRow,
};
```

`messageRows` 那 30 行 + 每 delta 的 `workRowOf` 分组遍历消失。

### 迁移与兼容

- **Core**：先「加字段不改旧字段」——`ToolCall` / `ToolResult` /
  `AgentContentDelta` / `AgentOutput` 各加 `entry: Option<String>` +
  `rev: Option<u64>`；同时新增 `EntryUpsert` 双发。旧 Panel 忽略新字段。
- **Panel**：`entry-store.ts` 双读——事件带 `entry` 走 Map，不带则回退现有
  `state.ts` 合并路径。灰度期两套并存，用 `state.entryProtocol: boolean`
  （首个 `EntryUpsert` 到达即置 true）判定。
- **退役**（P4）：删 `AgentContentDelta`（并入 upsert）、`ToolCall`+`ToolResult`
  双事件（并入 upsert 的两个 rev）、`elide_reasoning_for_wire`、
  `sessionMessagesFor` 的启发式、`chat-message-cache.ts`（Map 按 key 的天然
  细粒度依赖取代它）。

---

## Panel 侧目标形态

```ts
// 归一化：一张 Map + 每会话一个有序 id 索引
interface EntryRecord { id: string; rev: number; kind: EntryKind; state: EntryState;
                        content: string; body: EntryBody; atMs: number; block?: BlockHint }

const entries = new Map<string, EntryRecord>();            // 值 markRaw（正文不进响应式图）
const order = shallowReactive<Record<string, string[]>>({}); // 只整体替换

/** 唯一写入口（幂等 + 乱序安全）。 */
function apply(u: EntryUpsert) {
  const prev = entries.get(u.entry);
  if (prev && prev.rev >= u.rev) return;                   // ① 丢弃旧版本
  entries.set(u.entry, markRaw(toRecord(u)));
  if (!prev) insertByCursor(order[key], u.entry);           // ② 顺序由 Core 游标给
  bumpVersion(key);                                         // ③ 触发该行重渲染
}
```

渲染分层订阅，让 Vue 的 Map 依赖**按 key** 生效：

```vue
<!-- 父：只在「新增/删除条目」时失效（delta 不失效） -->
<EntryRow v-for="id in visibleIds" :key="id" :entry-id="id" />
<!-- 子：只订阅自己那一个 key -->
const rec = computed(() => entries.get(props.entryId));
```

改写 entry A → 只有 A 的子组件重渲染。这直接消灭 `chat-message-cache.ts`
存在的理由（200 行 + 23 个回归用例可以删），也消灭「数组级 computed」这个
一切性能问题的放大器。

派生值（`timeLabel`、分组、来源标签）在 **ingest 时算一次**并随记录存，
渲染阶段零 Intl、零 `JSON.parse`。

---

## 落地路线与验收

| 阶段 | 内容 | 改动 | 工作量 | 验收 |
| --- | --- | --- | --- | --- |
| **P0**（已完成） | 渲染窗口 200、`formatTime` memo、工作行 WeakMap 缓存、流式纯文本、写时复制 | Panel 5 文件 | ~1 天 | 2000 行 4× CPU：0 掉帧；单 delta ≤1.5 ms；DOM/堆恒定 |
| **P1（第一步已完成）** | 信封层 `entry`（`EntryTracker`）+ Panel 三处精确命中 | Core `entry_identity.rs` / `management.rs` / `bridge.rs`；Panel `connection` / `state` / `timeline` | 1 天 | 双读回归（`entry` 缺失时行为不变）+ 同名工具并发不配错 |
| **P1（余下）** | `EntryUpsert` 新增事件 + 时间线条目带 `entry` + `RequestWindow`/`Subscribe` 游标 | Core 协议 + 写入端；Panel 新增 `entry-store.ts` | 3–4 天 | 两套路径同屏输出一致（对账测试）；旧 Panel 不受影响 |
| **P2** | 删启发式与渲染缓存，`sessionHistory`/`teamTimelines` 并为 `entry-store` | `state.ts` −400 行、`timeline.ts` −200 行、`ChatView` 重构 | 4–6 天 | 1 万条历史：单 delta <2 ms、堆 <60 MB 且与长度无关；`chat-message-cache` 测试组随之删除 |
| **P3** | `RequestWindow`/`Subscribe` 游标；向上分页 + 锚定；重连纯增量 | Core `commands.rs`/`history.rs`；Panel `connection.ts`/`ChatView` | 3–4 天 | 断线重连帧数 = 新增条数；显示历史可放到 4 万条 |
| **P4** | delta 合批（50–80 ms 窗口）、streaming 纯文本下沉、删 `elide_reasoning_for_wire` 与双事件 | 两侧 | 2 天 | 回答期 Markdown 解析次数 = 1；协议面收敛 |

**P2 完成即测的硬指标**：1 万条历史、4× CPU、流式回答 —— 60fps、
单帧 p95 < 4 ms、堆 < 60 MB、刷新首屏 < 100 ms。

---

## 元素排布的归属

现状是 Panel 从扁平日志**反推**排布（连续工作条目合并），这是把协议层的
语义塞进了渲染层。身份化后协议可以显式携带呈现意图，而**不需要** Core 懂布局：

| 决策 | 归属 | 协议字段 |
| --- | --- | --- |
| 这一条是什么 | Core | `EntryKind` + `EntryBody` |
| 它属于哪一步、默认折叠吗 | Core | `BlockHint { group, collapsed }` |
| 这一行长什么样（气泡 / chip / 终态） | Panel | `ENTRY_RENDERERS` 注册表 |
| 一屏显示多少、何时加载更早 | Panel | 渲染窗口（P0 已落地） |
| 侧栏卡片位置、tab 顺序 | Panel | `rail-layout.ts` / `tab-prefs.ts`（本地偏好） |

唯一的「服务端驱动 UI」通道仍然只有实例面板（插件 `InstanceContribution.ui`
描述符 → `InstanceState` → `InstanceUiRenderer`），它与会话消息流无关。

---

## 风险与回退

| 风险 | 处置 |
| --- | --- |
| 双发期协议面变大 | 加字段不改旧字段；旧 Panel 完全忽略；`protocol-sync` 测试锁线格式 |
| `rev` 乱序导致界面回退 | Panel 侧 `prev.rev >= rev` 丢弃；Core 侧每 entry 单写者（同一 turn 串行） |
| 中断式 streaming（用户取消） | `EntryState::Cancelled` 收敛，不再靠 `settleStream` 猜 |
| 多窗口广播 | Upsert 幂等，天然安全（现有 `seq floor` 过滤可删） |
| 回退 | P1 双读开关 `state.entryProtocol`；Core 侧 `[panel] entry_upsert = false` 一键回到旧事件流 |
