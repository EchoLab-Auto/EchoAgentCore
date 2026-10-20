---
id: panel-chat-work
title: "工作行（工具 / 推理 / 子代理）"
group: 前端
x: 5760
y: 1340
link: ["panel-chat-tools | 工具卡与图标行", "panel-chat-reasoning | 推理块与活动浮条"]
---

# 工作行（工具 / 推理 / 子代理）

> **定位**：消息列里「工作行」这一行类型的契约——连续的工具 / 推理 / 子代理消息为什么合并成同类「工作项」、chip 与小圆角排布、开合策略与读秒从何而来，以及子代理委派行的完整语义。读者：维护 `WorkRunGroup.vue` 与 `state_domains/timeline.ts` 的开发者。
>
> **位置**：工作行是[会话消息区](./panel-chat-messages.md)的一种行——合并发生在 `ChatView` 的 `messageRows` 里，滚动、窗口切片、历史合并与缓存都沿用消息区机制（本页不重复）。两种子类型见[工具卡与图标行](./panel-chat-tools.md)与[推理块与活动浮条](./panel-chat-reasoning.md)。

## 工作行模型

**连续的工具 / 推理 / 子代理消息合并为同类「工作项」并由 `WorkRunGroup` 渲染**：`WorkRunItem.kind` 区分 `tool` / `reasoning` / `subagent`，三类共用同一排 chip 与同一张详情卡（一次只展开一项），正文层各自不同（工具 = 名称/耗时/入参出参，推理 = `ReasoningText` 正文层，子代理 = 任务摘要 + 结论）。其余消息逐条渲染；列表由 `.chat-view__list` 自身滚动，吸底滚动契约不变（见[消息列表与滚动](./panel-chat-list.md)）。

- **开合策略**（`WorkRunGroup.vue` 的 `reconcile`，单一打开项）：自动顺序 = **用户手动选中的项优先**（点过的条目进 `userToggled`，此后自动逻辑不再覆盖它，直到它被移出列表）→ **仍在播放的最后一个思考**（打字机未播完 / 真流式未结束）→ 组内已无 running 且存在失败项时**首个失败项**（工具失败或子代理失败）→ 收起。工具运行中不自动展开，行保持紧凑
- **读秒**：`WorkRunGroup` 的 `now` ref + 50ms ticker——仅当组内存在 running 项时开定时器，完成即停；耗时优先用 Core 上报的 `elapsedMs`，运行中才按 `startedAtMs` 实时回算
- **chip 规格**：三类统一 30×26、圆角 8px、间距 4px、`flex-wrap` 换行；配色区分种类——工具 = 中性灰（失败染 error 色）、思考 = 紫（`--panel-reasoning`）、子代理 = 青（`--panel-subagent`）
- **状态角标**：运行中的工具 chip 右下角显示实时耗时（紧凑格式 `39ms` / `1.2s` / `2m03s`，50ms 刷新）；播放中的思考 chip 显示紧凑字数（`1.2k`）；子代理无角标，只有右上角转圈点
- **行距**：工作行（推理块 / 工具行 / 子代理行）保持 6px 紧排且自身不再带外边距；消息气泡用 `chat-view__item--msg` 的 3px 边距补回呼吸（气泡间仍约 12px）
- **`spawn_subagent` 的图标行/详情头部显示任务摘要**而非工具名（解析 Core 时间线摘要串与原始 JSON 两种形态，80 字符截断）——「这次派了什么活」一眼可见

## 子代理委派行

子代理（`spawn_subagent`）一份委派对应**一枚 chip**、全程可见（此前完成回报以原始 JSON 直接渲染成 user 气泡——一条动辄数千字符、占满半屏；2026-10 起与工具/思考同排，不再单独占一整行）：

- **派发**：`SubagentStarted` → 在派发位置插入一条 `subagent` 工作项（青色 chip）——运行中 chip 呼吸（`work-run-pulse-subagent`）+ 右上角青色转圈点；任务摘要不铺在行上，进悬停提示（`title`）与详情头（单行省略）
- **完成合并**：`<subagent_event>` 钩子到达 → **就地改写该条目**（`running` 转 false、填入 `result` / `success`，保留派发位置），展开后显示结论全文（`.work-run-group__pre` 限高 **220px** 内滚动）；未命中（条目已滚出窗口 / 刷新后先到钩子）则由调用方追加一条——刷新后的历史视图里它落在钩子位置
- **匹配语义**：同一会话内按「最老的运行中条目」先来先配（FIFO，`finishSubagentRun` 按 `sessionId` 过滤后取 trunk 里第一条 `running`），与 Core `SubagentCompleted` 的完成/取消语义一致；多子代理并发时各占一枚 chip
- **失败态**：chip 图标染 error 色 + 右上角红点（`work-run-group__dot--failed`）；详情头状态胶囊「失败」、悬停提示「子代理失败」，结论体同样可展开
- 实现：`state_domains/timeline.ts::parseSubagentEvent`（实时与历史回放共用解析）、`startSubagentRun`/`finishSubagentRun`；组件 `WorkRunGroup.vue`（subagent 工作项）

## 子契约与实现

| 工作项 | 渲染 | 契约 |
|---|---|---|
| `tool` | `ToolIcon` chip 行 + 详情卡 | [工具卡与图标行](./panel-chat-tools.md) |
| `reasoning` | `ReasoningText` 正文层 | [推理块与活动浮条](./panel-chat-reasoning.md) |
| `subagent` | 青色 chip + 结论展开 | 本文「子代理委派行」 |

实现：`WorkRunGroup.vue`（行容器 `work-run-group` + chip 行 `__row`、开合决策 `reconcile`、读秒 ticker）、`ChatView.vue` 的 `messageRows` / `buildWorkRow`（工作项投影与相邻合并、`workRowCache` WeakMap 复用）、`state_domains/timeline.ts`（时间线投影、配对与收尾）、`ToolIcon.vue`（工具图标与运行动画）。同一组件也被**分支详情弹层**复用——`BranchModal.vue` 把分支里的工具调用与思考组装成同样的 `WorkRunItem[]`（`kind: 'work-group'` 行），两处的 chip 行视觉与开合语义因此一致。
