---
id: chat-messages
title: "会话消息区"
group: 前端
x: 4800
y: 1060
link: ["chat-list | 列表与滚动", "chat-history | 历史与实时窗口", "chat-timeline | 时序与动画", "chat-work | 工作行"]
---

# 会话消息区

> **定位**：消息区的**渲染与数据机制**契约——消息行由谁渲染、时间线里有哪几种角色、气泡几何如何决定、渲染对象怎样缓存（用户手势本身见[消息列表与滚动](./chat-list.md)的跟随规则与[输入区](./chat-input.md)的发送/取消）；往下分四篇子文档：列表的滚动与容量、历史与实时窗口的合并、主时间线时序、以及**工作行**（工具 / 推理 / 子代理的 chip 行——它是消息列里的一种行，不是独立分区）。读者：维护 `ChatView` / `MessageItem` / `WorkRunGroup` 的开发者。上游：[会话视图](./chat.md)；相关：[交互定义](./interaction.md)。
>
> **工作行为何在此**：`ChatView` 的 `messageRows` 在**构建行列表时**就地合并相邻工作项（`ChatRow{kind:'work-group'}`），工作行随后与普通消息行共用同一个滚动容器、同一套渲染窗口切片、同一份历史合并与缓存。它没有自己的滚动、分页或落位规则——这些全部由本篇与三篇子文档承担，因此工作行只是消息区的**行类型扩展**，其契约挂在消息区之下。

## 消息角色与渲染

| 角色 | 渲染 |
|---|---|
| `user` | 用户气泡（含来源元数据：平台/用户/群） |
| `backend` | Agent 气泡（Markdown） |
| `reasoning` | **Panel 扩展角色**：并入工作行组（`WorkRunGroup` 的 reasoning 工作项，正文层 `ReasoningText`）；库的 ChatRole 无此角色，不拦截会被误渲染为 Agent 气泡 |
| `tool` | **连续调用合并**为 `WorkRunGroup` 的 tool 工作项（一排圆角矩形图标）；单发调用（前后无相邻 tool）也走同一行渲染 |
| `subagent` | **Panel 扩展角色**：子代理委派行——派发位置一行「子代理运行中/完成/失败 + 任务摘要」，点击展开结论 |
| `system` | 系统提示行 |
| `branch` | 分支合并卡角色（当前 reducer 已不产生——分支内容实时进主时间线；此角色仅为适配保留，面板无对应渲染） |

补充来源：定时器触发以 system 消息插入主时间线（`⏰ 定时器触发 · {task}`，task 截断 160 字符）。

**渲染分工**：行模型与组合归面板——消息行 `MessageItem`（**气泡外壳是 ui-frame `NeumorphismCard` 凸起档**（2026-10-29），角色语义（对齐 / 色调）留在 `MessageItem` 自有样式，圆角四角等值由卡片 `radius=16` 决定；原库 `ChatBubble` 已随 ui-frame 精简解散，其壳样式曾于 2026-10-13 吸收进面板、2026-10-29 起材质交回卡片组件；正文用 `MarkdownLazy`——doc 模块的 Markdown 渲染**延迟加载**：chunk 未达时以纯文本回退呈现（不阻塞首屏、无空白帧），到达后增强为完整 Markdown；无 Agent 消息的会话完全不加载 doc 模块）；**连续的工具 / 推理 / 子代理统一由 `WorkRunGroup` 渲染**（三者合并为同类「工作项」，`WorkRunItem.kind` 区分 `tool`/`reasoning`/`subagent`；合并粒度是**任意相邻工作项**，中间夹着别的角色就断开成组；推理正文层为 `ReasoningText`；契约见[工作行](./chat-work.md)）；ui-frame 的 chat 模块如今只剩**纯原子层**（`ChatComposer` / `ChatFold` / `ChatCopyButton` + `ChatMessage` 类型），吸底滚动是 headless 的 `useChatScroll`——`ChatBubble` / `ChatTray` 均已解散。库的 chat 组合组件（`ChatMessageList` / `ChatMessageItem` / `ChatToolCallBlock` / `ChatReasoningBlock` / `ChatBranchMergeBlock`）已从 ui-frame 移除（原因：聊天组合属产品语义）。

## 气泡几何

**气泡宽度**：宽度完全动态——短消息贴内容收紧（实测最短 186px），长消息/代码/表格撑满消息列可用宽度（随左右侧栏卡片显隐变化，实测 1600px 窗口下 1276px）；`.chat-view__item--msg` 为 flex 列容器（user 靠右 / agent 靠左 / system 居中，依赖库 `align-self`）。面板**不设百分比上限**：普通消息 `max-width: 100%`、system 消息 `92%`（`MessageItem.vue` 自绘，无库默认值需要覆盖）；并隐藏 Markdown 标题锚点
（其 -22px 绝对定位会撑出横向溢出）。

**气泡材质（2026-10-29 起）**：外壳是 ui-frame `NeumorphismCard` 的**凸起档**（`elevation=1`、
`radius=16`、`no-padding`）——阴影/圆角/裁剪全归库的台阶高度模型，面板**不再手写 `box-shadow`**
（否则会以更高特异性压掉 `.nm-card--elevation-*`）。面板侧只剩角色语义两件事：

| 维度 | user | agent | system |
|---|---|---|---|
| 对齐 | `align-self: flex-end` | `flex-start` | `center` |
| 色调 | `--nm-chat-user-bg`（主色 `#cdfa4e` 与表面色 mix：亮 22% / 暗 14%） | `--nm-chat-agent-bg`（`--nm-surface-raised`，比托盘亮一档） | `--nm-chat-system-color`，无气泡 |

**圆角四角等值（16px）**：卡片的 `border-radius: var(--nm-card-radius, 24px)` 取组件
`radius=16` 下发的值，面板**不覆盖任何一个角**。2026-10-29 之前曾有「非对称圆角开向」
（靠内一侧收成 `--nm-border-radius-xs` = 4px，意图是给方向一个不依赖颜色的形状线索），
当日按用户要求撤销：卡片就该是四角同半径，且角色方向已由对齐（左/右）+ 色调（染色 /
抬升）表达，第三个线索属重复编码。回归断言：`MessageItem.test.ts` 检查面板样式块里
**没有任何 `border-radius` 声明**（去掉注释后匹配），即圆角所有权完全在库侧。

`elevation=1` 的实际配方：ambient `0 1px 2px`(`--nm-shadow-ambient-sm`) + 暗侧 `4px 4px 10px`
+ 亮侧 `-2px -2px 6px`。选 1 档而非 2/3 档的理由是**几何接近原手写阴影**（`3px 3px 10px`），
且多条消息连续排布时 2 档（`8px/20px`）的软影会彼此糊在一起。`no-padding` 是因为库 body 留白
24px 对气泡过大——8px 14px 的 padding 属版面，留在面板侧。`role` / `aria-label` 经 attrs
透传到卡片根节点，无障碍语义不变。

## 渲染对象缓存

数据链（`sessionMessagesFor` 的快照浅拷贝 + `toChatMessage`）每次求值都产出**全新对象**；若直接拿它做 `v-for` 的 `:message`，任意一条变化（流式 delta 每几十毫秒一次）都会让上千个 `MessageItem` 逐个 re-render，而 DOM 几乎不需要改。`chat-message-cache.ts` 按消息 key 缓存渲染产物，把「数据变化」与「组件更新」解耦：

- **按 key 复用**：`snapshotOf` 记下影响渲染的字段值，`reuseChatMessage` / `rememberChatMessage` 命中即沿用上次的 `ChatMessageWithImages` 引用（子组件 props 恒等 → Vue 跳过），未命中才走 `toChatMessage`；`retainChatMessages(alive)` 按当前存活 key 剪枝，会话切换与 FIFO 淘汰不会让缓存无界增长
- **判等必须基于值快照，不能基于对象引用**（`RenderSnapshot` 值快照 + `reuseChatMessage` / `sameForRender` 判等）：`state.ts` 的 reduce 大量**原地深变异**——`msg.tool.status = 'succeeded'`、`msg.content += delta` 都不换对象。只按对象引用判等（外加只比 `tool` 引用）时，缓存会**永远命中**：工具行卡在 `running`（`WorkRunGroup` 的 `hasRunning` 恒真、50ms ticker 一直读秒）、流式正文不增长，两者都只有刷新（清空模块级缓存）才恢复。字符串本身不可变、存引用即快照；`tool.*`、`reasoning` / `images` 数组、`branch.msgs` 在 `snapshotOf` 里逐项拷值
- **增量对账同样要穿透缓存**：Core 对运行中工具条目就地更新后带新 seq 重投完成态，`patchToolMessage` 在原 `DisplayMessage` 上替换 `tool`（消息对象身份不变）——只按身份判等的缓存会吞掉完成态，于是「实时 ToolResult 丢失、靠 seq 补拉找回」的场景仍然一直读秒
- **推理行不走缓存**：其 `animate` 由有状态的 `claimReasoningAnimation` 决定（同一 key 仅首次可能命中、此后恒 false），缓存首次结果会改变语义；推理每回合通常一条，保持原路径
- **回归契约**：`__tests__/render-cache-mutation.test.ts` 锁定「原地改 `tool.*` / `content` / `reasoning` / `branch.msgs`、浅拷贝外层 + 原地改内层都必须失效，未变消息仍命中同一引用」，并覆盖 TrunkTimeline 增量对账穿透缓存的路径；`chat-message-cache.test.ts` 锁字段级判等与剪枝
