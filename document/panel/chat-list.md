---
id: chat-list
title: "消息列表与滚动"
group: 前端
x: 4800
y: 1340
---

# 消息列表与滚动

> **定位**：消息列表的行为契约——跟随与「回到底部」、容量与渲染窗口、流式与重算成本、行距与入场动画、空态与内容呈现细节。读者：维护 `ChatView` 列表与滚动逻辑的开发者。上游：[会话消息区](./chat-messages.md)；数据来源与合并见[历史与实时窗口](./chat-history.md)。

## 滚动与跟随

- **自动滚动**：仅当用户处于底部 120px 阈值内时跟随新内容（判据在库的 headless `useChatScroll`，默认 `threshold: 120`）；上翻后浮现**圆形上箭头按钮**（36px、`aria-label="跳转到最新消息"`，自组样式），点击走 `scrollToBottom('smooth')`——平滑滚动期间（500ms）暂停贴底判定，避免滚动过程中的中间位置反复触发按钮闪烁。滚动区内 padding-bottom 190px（**窄屏另算**：`190 + 安全区 + 键盘位移`，见[移动端布局](./mobile-layout.md)「底部两带」）。**按钮落位**：`right = trayPadRight + 16px`、`bottom = entryBottom + 45px`（消息列右缘、右侧栏列左侧 16px，纵向与边栏列底对齐；贴右下角会被右侧栏卡片完全压住、hit test 命中 rail 卡片，故移至此处）；入口行浮层 / Agent 配置 / 上下文弹层打开时按钮隐藏（`.chat-view--overlay-open` 规则，浮层从入口行向上展开、最高 68vh，窄视口下必盖住）

## 容量与渲染窗口

- 主时间线上限 1024 条，溢出裁最旧（数据侧）；**渲染侧只画尾部 200 条**（`ChatView.vue` 的 `WINDOW_INITIAL`/`renderWindow`），更早的显示「加载更早的消息（还有 N 条）」入口，点击按 200 扩窗（`loadEarlier`：扩窗后按"距底部距离"补偿 `scrollTop`，并在 nextTick / 下一帧 / 300 ms 后各补偿一次——前插的 200 行里延迟加载的 Markdown 与图片会在插入后继续改高度，一次性补偿会残留约 100 px 偏移）。**切片在适配之前**——`sessionMessagesFor` 只取尾部窗口喂给 `toChatMessage`/缓存判等，因此一次 delta 的成本只与**窗口大小**相关、与历史长度无关（实测：200 条窗口下单次 delta 的适配耗时与总条数无关）。切会话 / 切 agent 把窗口复位到 200

## 流式与重算成本

- **流式中纯文本**：`streamState === 'streaming'` 的 Agent 消息走纯文本渲染（`MessageItem` 的 `streamingPlain`），终值到达后才交给 `MarkdownLazy`——Markdown 渲染器对 `content` 是"每次变化全量 `marked.parse` + DOMPurify + `innerHTML` 替换"，逐 delta 全量重解析的成本是 `O(deltas × 长度)`（实测 4 万字正文单次 16.9 ms、8 万字 33.3 ms）；改为终值一次成型后是 `O(长度)`，副产品是正文立即可见
- **时间标签 memo**：`formatTime` 按"秒"缓存 `toLocaleTimeString('zh-CN')` 结果（`chat-adapter.ts`，FIFO 上限 8192）——实测单次 51 µs，`messageRows` 每次重算都会给每条工作行调一次，在 2000 行会话里占单次 delta 的 44%

## 行距与入场动画

- **行距**：滚动容器 `gap` 6px；消息气泡用 `chat-view__item--msg` 的 3px 边距补回呼吸（气泡间仍约 12px），工作行（推理块 / 工具行 / 子代理行）保持 6px 紧排且自身不再带外边距——一回合里推理与工具交替十几次，屏幕不被空白吃掉

- **入场动画**：0.28s 淡入 + 上移 6px，仅实时消息（`animate: true`）播放；历史回放不播

## 内容呈现与空态

- **hook 输入展示剥离**：`<xxx_hook>{json}</xxx_hook>` 结构化输入在会话里只显示 payload 的 `content` 字段（`displayContentOf`，实时事件与历史回放共用同一剥离；非法 JSON/非 hook 原样兜底）——QQ 消息投递到默认会话后显示干净正文而非整段 hook JSON

- **会话过滤**：「全部消息」（global）显示当前 Agent 全部会话的消息；具体会话仅显示该会话；从全局视图发送会路由到本地会话 `local:tui::local_user`（`App.vue` 的 `send`：全局视图回落到默认本地会话）
- **空列表占位与加载三态**：消息列为空时区分——未连接（居中 spinner +「正在连接 Core…」）/ 已连接但本次连接尚未收到时间线（「正在加载时间线…」）/ 收到过快照后的真空白（全局视图与具体会话两种文案，`ChatView.vue` 的 `emptyText` / `timelineLoading` computed）。判定标志 `state.timelineArrivedOnce`（`onopen` 重置；见 [桌面端布局](./desktop-layout.md)§四）
- **图片**：历史消息中的图片渲染在气泡上方，最大 260×200px（`ChatView.vue` 的 `imagesOf` 与 `.chat-view__images` 模板）；Core 侧 `/media/<id>` 引用直接 `<img loading=lazy decoding=async>` 渲染（同源、强缓存），遗留 data URI 仍兼容；正文经 `chat-adapter.ts::compactForDisplay` 压过——附带的 data URI → `[图片#n]`、其余长内联 base64 → `[图片数据已省略]`，与 Core 侧占位符一致，避免整屏 base64
- **复制**：消息气泡有复制按钮（库内置，悬停出现）；工具行与工具输出**无**复制按钮；无任何右键菜单
- **Markdown**：仅 Agent 消息渲染 Markdown
- **时间格式**：zh-CN 24 小时制
