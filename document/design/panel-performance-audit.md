---
id: panel-performance-audit
title: "Panel 浏览器性能审计"
group: 规划
x: 3200
y: 2050
---

# EchoAgent Panel 浏览器性能报告

> 测量时间：2026-10-18 · 被测版本：`index-PalZG3AF.js`（main `c0009a0`）
> 测量方式：Chrome for Testing 153 headless + CDP 直连（无 playwright 依赖），
> 真实渲染（从 Vue 应用实例取 store 注入消息），非 DOM 克隆估算。
> 机器：本机（evence-4090-24），视口 1600×1000。

---

## 一、结论摘要

| 维度 | 评价 | 关键依据 |
|---|---|---|
| **首屏加载** | 🟢 **优秀** | gzip 125 KiB / FCP 176ms / LCP 732ms；4× CPU + 4Mbps 下 FCP 288ms、LCP 1012ms |
| **首屏 JS 执行** | 🟢 **好** | 正常仅 1 个 long task（50ms）；4× 节流下 2 个（135+58ms） |
| **静态渲染（空会话）** | 🟢 **优秀** | 269 DOM 节点，滚动 60fps |
| **满载渲染（1024 条）** | 🟡 **可接受但有代价** | 10759 节点；一次性 116ms（1×）/ 466ms（4×）；滚动仍 60fps |
| **满载 + 流式输出** | 🟢 **已修复**（P0 落地后） | 原 1× 40.5 / 4× 11.7 FPS → **均 60.8**；见「四·补」 |
| **内存** | 🟢 **正常** | 1024 条满载 87–110 MB；无增长趋势 |

**一句话**：Panel 的首屏与静态渲染都做得很好（已经有过多轮加载优化），
但**消息量满载时的流式渲染存在一个明确的性能缺陷**——每次流式增量会让
**全部 1028 个消息组件重新 render**，在低端设备上把 FPS 拖到 11.7（应为 60）。

---

## 二、实测数据

### 2.1 首屏加载

| 指标 | 无节流（10Mbps/40ms） | 4× CPU + 4Mbps/60ms |
|---|---|---|
| DOMContentLoaded | 248 ms | 518 ms |
| load | 292 ms | 585 ms |
| FCP | **176 ms** | **288 ms** |
| LCP | **732 ms** | **1012 ms** |
| CLS | 0.032 | — |
| 首屏请求数 | 3（index.html + 1 JS + 1 CSS） | 同 |
| 首屏传输（gzip） | **124.9 KiB** | 同 |
| Long Task | 1 个 / 50 ms | 2 个 / 193 ms |

**首屏传输构成**：

| 资源 | raw | gzip |
|---|---|---|
| `index-*.js`（应用入口） | 195.7 KiB | 71.7 KiB |
| `vue-*.js`（Vue 运行时） | 83.7 KiB | 33.1 KiB |
| `index-*.css` | 127.7 KiB | 20.1 KiB |
| **合计** | 407.1 KiB | **124.9 KiB** |

其余 25+ 个 chunk（SettingsView 67.9K / ApiSettings 39.7K / AdapterPanel 32K /
purify 28.3K / …）都是**按需加载**，不进首屏。`dist` 总量 1.0 MiB（JS+CSS）。

> 已有优化痕迹：ui-frame 子路径按需导入、`MarkdownLazy` 异步 chunk、
> `index.html` 内联骨架屏、`trunk-cache` 首屏 hydrate。这些都在生效。

### 2.2 满载渲染（逐步灌入消息，测真实 Vue 渲染）

**1× CPU**：

| 累计消息 | 本次渲染耗时 | DOM 节点 | 滚动 FPS |
|---|---|---|---|
| 106 | 29.2 ms | 1297 | 60 |
| 406 | 52.4 ms | 4363 | 60 |
| 1030 | **116.2 ms** | 10759 | 60 |

**4× CPU（模拟中低端设备）**：

| 累计消息 | 本次渲染耗时 | DOM 节点 | 滚动 FPS |
|---|---|---|---|
| 106 | 93.8 ms | 1297 | 60 |
| 406 | 206.3 ms | 4363 | 60 |
| 1030 | **466.5 ms** | 10759 | 60 |

**滚动性能与消息数无关**（60fps 恒定）——CSS 只有 transform/opacity 动画、
消息气泡结构简单，浏览器合成器扛得住。**瓶颈不在滚动。**

### 2.3 满载 + 流式输出（核心问题）

1024 条消息，模拟流式 delta（改最后一条 `content`），同时用 rAF 采样真实帧率：

| 指标 | 1× CPU | 4× CPU |
|---|---|---|
| **FPS** | **40.5** | **11.7** |
| 帧间隔 p50 | 16.8 ms | 16.8 ms |
| 帧间隔 p95 | 33.4 ms | **166.7 ms** |
| 帧间隔 max | 33.4 ms | **199.9 ms** |
| 掉帧数（>25ms） | 39 / 79 | 39 / 79 |
| 单次 delta 耗时 | 49.6 ms | **173.2 ms** |
| 堆内存 | 92.9 MB | 110.6 MB |

### 2.4 瓶颈归因（决定性实验）

4× CPU + 1024 条消息，三组对照（各 15 次迭代）：

| 实验 | 平均耗时 | 说明 |
|---|---|---|
| **A. 改 state 里最后一条 `content`** | **176.9 ms** | 走完整 Vue 响应式链 |
| **B. 什么都不改，只等 2 个 rAF** | **33.1 ms** | 纯基线开销 |
| **C. 直接改 DOM 文本（绕过 Vue）** | **32.9 ms** | ≈ 基线，DOM 改动几乎免费 |

**A − B = 143.8 ms 是纯 Vue 开销。**

同期计数器：

| 计数器 | 15 次 delta 的增量 |
|---|---|
| `LayoutCount` | **3** |
| `RecalcStyleCount` | **7** |
| `ScriptDuration` | 3 ms（主线程采样未完整覆盖） |
| `TaskDuration` | **2605 ms** |

**结论**：143.8 ms 的阻塞里，DOM 只布局了 3 次、样式重算 7 次 —— 也就是说
**浏览器几乎没做 DOM 工作**，时间全花在 JS 里创建 vnode、diff、调用组件
render 函数上。这是典型的「组件全量 re-render 但产出无变化」的浪费。

---

## 三、根因

### 3.1 数据链每次返回全新对象

```ts
// state.ts::sessionMessagesFor —— 第 1 次拷贝
const out = hist.messages.map((m) => ({ ...m }));   // 每条都新对象

// ChatView.vue::messages —— 第 2 次拷贝
return list.map((m) => {
  const cm = toChatMessage(m);                       // 全新对象（含 tool/branch/source 嵌套）
  ...
});

// ChatView.vue::messageRows —— 第 3 次遍历
for (const m of messages.value) { ... rows.push(...) }
```

`toChatMessage`（`chat-adapter.ts`）每次都创建完整新对象，嵌套的 `tool` /
`branch` / `source` 也各创建一次，`branch.entries` 还调用 `mergeReasoning()`
做一次遍历。

**结果**：只要**任意一条**消息变化，`messages` computed 就返回
**1024 个全新对象引用** → 所有 `MessageItem` 的 `:message` prop 都「变了」
→ Vue 认为 1028 个子组件都需要 re-render → 每个都跑一遍 render 函数、
创建 vnode、做 diff，最后发现 DOM 几乎不需要改。

### 3.2 量化

- 143.8 ms ÷ 1028 个组件 ≈ **0.14 ms/组件**
- 其中真正需要更新的只有 **1 个**（最后那条）
- **≈ 99.9% 的 re-render 是纯浪费**

### 3.3 触发频率

流式输出时 Core 以 delta 推送，实测一个中等长度回答可产生
**20–60 个 delta**。在 1× CPU 满载下每次 49.6ms → 相当于整个回答期间
主线程有 ~2–3 秒被无效工作占满；4× CPU 下则是 10 秒以上。

---

## 四、优化建议（按 ROI 排序）

### P0 · 让未变消息复用对象引用 —— 预期消除 99% 浪费

**做法**：在 `ChatView` 里为 `messages` 加一层「按消息 key 缓存」，
只有内容真正变化的条目才创建新对象：

```ts
// 伪代码
const chatCache = new Map<string, ChatChatMessageWithImages>();
const messages = computed(() => {
  const list = /* sessionMessagesFor(...) */;
  const next = new Array(list.length);
  for (let i = 0; i < list.length; i++) {
    const m = list[i];
    const prev = chatCache.get(m.key);
    next[i] = prev && isSameMessage(prev, m) ? prev : toChatMessage(m);
  }
  // 清理已不在列表里的 key（避免无界增长）
  ...
  return next;
});
```

`isSameMessage` 比较真正影响渲染的字段（`content` / `reasoning` /
`tool.output` / `streamState` / `time` …），或直接比较 `DisplayMessage`
的几个标量。因为 `sessionMessagesFor` 也做浅拷贝，可以进一步用
`DisplayMessage` 的引用相等 + 字段比对跳过它（或把该层缓存也做上）。

**预期收益**：delta 耗时 176.9 → **≈33 ms**（只剩 rAF 基线），FPS 11.7 → **60**。

**风险**：低。语义等价（对象内容相同），只要缓存失效条件写对。

**工作量**：约 40–60 行 + 单测，1 人日内。

---

### P1 · `sessionMessagesFor` 去掉不必要的浅拷贝

```ts
const out = hist.messages.map((m) => ({ ...m }));   // 每条消息一次拷贝
```

注释说「浅拷贝：补丁不得污染快照」。但实际上只有 `patchToolMessage`
会改对象。可以改成**惰性拷贝**：仅当某条确实被 patch 时才拷贝那一条。

**预期收益**：1024 条时省掉 1024 次对象分配（配合 P0 后收益变小，但仍是白拿的）。

---

### P2 · 虚拟滚动（可选，收益看使用场景）

当前 1024 条 = 10759 DOM 节点。滚动 60fps 说明**浏览器扛得住**，所以
虚拟滚动的收益主要在：

- 低端设备首次渲染（466ms → 可降到 ~50ms）
- 内存（10759 节点 → 约 2000）

**但成本高**：要处理 variable-height 消息（Markdown、图片、代码块、工具卡
展开）、吸底滚动、`Ctrl+F` 查找、锚点跳转。**建议先做 P0/P1，观察后再决定。**

替代方案（成本低得多）：**限制渲染窗口**——只渲染最近 N 条（如 300），
更早的显示「加载更早的消息…」按钮。消息本来就有 1024 条 FIFO 上限，
用户极少需要一屏翻完 1000 条。

---

### P3 · `TrunkTimeline` 全量替换时的对象重建

`state.ts` 里全量分支 `state.trunk = [...converted]` 会重建所有消息对象。
配合 P0 的缓存可以吸收，但若 bootstrap 频繁（每次重连）仍有一次全量成本。
建议在 P0 完成后复查。

---

### 不建议做的优化

| 项 | 为什么不做 |
|---|---|
| **LTO / codegen-units=1** | 那是 Rust 侧，与浏览器无关 |
| **手写虚拟 DOM / 换框架** | 收益不抵风险 |
| **CDN / 预加载全部 chunk** | 首屏已经 125 KiB，再预载会让慢网首屏变慢 |
| **减少 1024 条上限** | 上限本身不是瓶颈（滚动 60fps），砍掉会丢历史 |
| **`v-memo`** | Vue 3.2+ 的 `v-memo` 可用于列表项，但需要稳定引用才能生效——本质和 P0 同一件事，P0 更彻底 |

---

## 四·补 · P0 落地后的实测对比（2026-10-18）

P0（按 key 复用渲染对象，`web/src/chat-message-cache.ts`）已实现并部署
（`index-ByTrThOh.js`）。同一套测量脚本复测：

| 指标（1024 条消息 + 流式） | 修复前 | 修复后 | 变化 |
| --- | --- | --- | --- |
| **4× CPU** FPS | 11.7 | **60.8** | **+420%**（达到 60fps 上限） |
| 4× CPU 单次 delta | 173.2 ms | **34.4 ms** | **−80%** |
| 4× CPU 帧间隔 p95 | 166.7 ms | **16.7 ms** | **−90%** |
| 4× CPU 掉帧（>25ms） | 39 / 79 | **0 / 79** | 消除 |
| 4× CPU 堆内存 | 110.6 MB | **23.4 MB** | −79% |
| **1× CPU** FPS | 40.5 | **60.8** | +50% |
| 1× CPU 单次 delta | 49.6 ms | **32.7 ms** | −34% |
| 1× CPU 帧间隔 p95 | 33.4 ms | **16.7 ms** | −50% |
| 1× CPU 掉帧 | 39 / 79 | **0 / 79** | 消除 |
| 1× CPU 堆内存 | 92.9 MB | **26.3 MB** | −72% |

修复后的 34.4 ms / 32.7 ms 已贴近「只等两个 rAF」的基线（33.1 ms）——
**增量渲染开销基本归零**。

堆内存的下降是附带收益：缓存复用后不再每次 delta 创建 1024 个新对象
（旧对象成为垃圾需要 GC），峰值分配量大幅减少。

回归防护：新增 `web/src/__tests__/chat-message-cache.test.ts`（23 例），锁三层
不变量——① 影响渲染的 16 个字段任一变化都必须判为"不同"（漏判会导致界面
不更新）；② 未变消息必须返回**同一对象引用**（性能收益的唯一来源）；
③ 移出列表的 key 必须被清理（防无界增长）。

---

## 五、性能要求评估

「性能要求」没有官方定义，按通用 Web 体验基线（Core Web Vitals + 交互延迟）：

| 基线 | 目标 | 实测 | 判定 |
|---|---|---|---|
| LCP（good） | < 2.5 s | 0.73 s（1×）/ 1.01 s（4× 慢网） | ✅ 通过 |
| CLS（good） | < 0.1 | 0.032 | ✅ 通过 |
| INP（good） | < 200 ms | 流式时单帧 **177 ms**（4×）/ 50 ms（1×） | ⚠️ 1× 勉强通过，4× **不通过** |
| 滚动流畅 | 60 fps | 60 fps（含满载） | ✅ 通过 |
| 首屏传输 | < 200 KiB gzip | 125 KiB | ✅ 通过 |

**结论**：桌面浏览器上「能用」，但**低端设备 + 长会话 + 正在输出**这个组合下
体验明显受损（FPS 11.7，输入和滚动都会卡）。这是一个**有明确修法、收益显著**
的缺陷，值得优先处理。

---

## 六、附：测量脚本

放在 `/tmp`（未入库，因为依赖 playwright 浏览器缓存路径）：

| 脚本 | 用途 |
|---|---|
| `perf-probe.mjs` | 首屏导航 / FCP / LCP / CLS / long task / 请求清单 |
| `perf-lowend.mjs` | 4× CPU + 4Mbps 节流下的首屏与静态渲染 |
| `perf-vue2.mjs` | 真实 Vue 渲染（从 `__vue_app__` 取 store 注入消息） |
| `perf-stream.mjs` | 满载 + 流式 delta 的真实 FPS 采样 |
| `perf-delta-proof.mjs` | A/B/C 三组对照，证明开销来自 Vue 而非 DOM |
| `perf-micro.mjs` | 数据处理链 micro-benchmark（结论：纯 JS 只需 0.042 ms，非瓶颈） |

补充 micro-benchmark 结果（说明为何不能只看 JS 计算）：

| 环节（1024 条） | 耗时 |
|---|---|
| `sessionMessagesFor` 等价实现 | 0.028 ms |
| `toChatMessage` 等价实现 | 0.016 ms |
| `messageRows` 等价实现 | 0.012 ms |
| **三者串联** | **0.042 ms** |
| `trunk-cache` 序列化（616 KiB payload） | 0.57 ms |

→ **纯 JS 计算 0.042 ms，实际渲染 143.8 ms**。差 3400 倍——瓶颈完全在
Vue 组件层的 re-render，不在数据变换。这条数据也是「不要盲目优化
`messageRows` 算法」的依据。
