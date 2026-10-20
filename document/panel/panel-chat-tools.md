---
id: panel-chat-tools
title: "工具卡与图标行"
group: 前端
x: 5600
y: 1620
---

# 工具卡与图标行

> **定位**：工具调用在主时间线上的投影与呈现——状态机与配对、就地修补与回合收尾、连接自愈，以及图标行（`WorkRunGroup` 里的 chip 行：一排 30×26 圆角矩形，工具 / 思考 / 子代理共用）的折叠态 / 运行动画 / 展开态。读者：维护 `state_domains/timeline.ts`、`ToolIcon.vue`、`WorkRunGroup.vue` 的开发者。上游：[工作行](./panel-chat-work.md)。

## 工具卡生命周期

```prodoc-flow
graph LR
  Call[ToolCall 事件] -->|同 id 幂等建档| Running[running: chip 呼吸 + 图标动画]
  Running -->|ToolResult 配对（id 精确 → 名字兜底）| Done{succeeded / failed}
  Running -->|回合结束仍无结果| Unknown[unknown: 未收到回执]
  Done -->|timed_out 或 error: 前缀| Failed[failed: error 标签]
  Done -->|否则| Ok[succeeded: success 标签]
```

- 输入显示为 `key=value` 摘要（单值截断 120 字符，非 JSON 兜底截 200 字符）；输出上限 4000 字符——这是**实时事件**路径（`state_domains/helpers.ts::summarizeArguments`、`timeline.ts` 的 `bounded(…, 4000)`）。**历史回放**走 Core 的 `summarize_timeline_value`，口径更紧：输入 160 字符、JSON 单值 48 字符、输出 200 字符

## 配对、修补与收尾

- **配对判据**（`state_domains/timeline.ts::findRunningTool`）：`tool_call_id` 精确配对优先；id 缺失（旧 Core / 供应商不回 id）或对不上时按**工具名**兜底，同名多条并发 running 时不猜（返回 null 走追加，宁可重复不配错）；`unknown` 也参与配对，迟到的结果能把它修正回来。找不到 running 条目（如恢复后的时间线）时追加一条已完成工具卡而非丢弃（`finishToolMessage`）
- **就地修补**（`patchToolMessage` / `findToolEntryIndex`）：Core 对运行中条目就地更新后带新 seq 重投完成态，按 `tool_call_id` 修补已有行而不是追加，避免重复行；无 id 的条目只有**完成态**增量才按名字修补——按名字把运行中行换成另一条运行中行毫无意义，还会抹掉 `startedAtMs` 让计时跳变
- **幂等**（`state.ts` 的 `ToolCall` 分支 + `repositionCompletedTool`）：同 id 的 ToolCall 已在时间线里就不再建第二条 running 行——重放 / 重连补发 / 结果先到的乱序都不会留下「一条已完成 + 一条永远读秒」；结果先到时把它挪回时序位置
- **回合收尾**（`state_domains/timeline.ts::settleRunningTools`）：任一 `ToolResult` 丢失（连接重建窗口、中继抖动、标签页后台节流、id 不匹配）都会让工具行永久 running，而 `AgentCompleted` / `Error` 是「本轮不会再有工具结果」的确定信号——该会话残留的 running 行收尾为 `unknown`：不再读秒、不染红、不谎报失败，耗时按开始时间回算，输出写明「未收到执行回执」。有并发分支/任务在跑时跳过（子代理与 reply 分支有独立会话与结束事件）
- **连接自愈**（`connection.ts` 的 `heal` / `WATCHDOG_IDLE_MS`）：连接半死（`readyState` 仍是 OPEN 却收不到任何帧，既不触发 `onclose` 也等不到 `visibilitychange`）是同一症状的另一来源——页面可见且 45s 无下行帧就走探测（幂等只读命令），5s 无响应主动重连，重连后按游标增量对账

- **中断标记**：历史回放时 `output == null` 且未失败的工具恢复为 running（`state_domains/timeline.ts::toDisplayMessage` 的 `toolStatusOf`）；「已中断」标记完全由 core 侧重启清理写入（`session.rs`，output = 「[已中断] Core 服务重启导致本次调用未返回，可重试」），panel 自身不做兜底标记

## 图标行：折叠态

**连续的工具消息在 ChatView 合并为一组 `WorkRunGroup`**（`kind: 'tool'` 工作项；原为库 `ChatToolCallBlock` 折叠卡，ui-frame 的 chat 组合组件移除后归面板自绘；历史上叫 `ToolRunGroup`，与推理/子代理合并后更名）；其余消息逐条渲染；列表由 `.chat-view__list` 自身滚动，吸底滚动契约不变。

- **折叠态**：一排圆角矩形毛玻璃 chip（30×26，间距 4px；flex-wrap——多个工具连续调用时横向扩展，横向空间不足自动下移一行）；运行中的 chip 右下角带实时耗时角标（`fmtCompact`：`39ms` / `1.2s` / `2m03s`），完成即消失；图标按工具分派不同 SVG（`ToolIcon.vue`：bash 终端符 / 文件 / 代码括号 / 清单对勾 / 计算器 / 搜索放大镜 / 子代理放射图 / 编排（后台·并行任务） / 消息气泡 / 插件实例齿轮 / 工作区文件夹 / 默认扳手）；失败图标染 error 色并带右上角红点
- **开合策略**：运行中不自动展开（行保持紧凑）；用户点过的条目**手动优先**，自动逻辑不再覆盖它；其余由 `WorkRunGroup` 的 `reconcile` 决定——播放中的最后一个思考 → 组内无 running 且存在失败时的首个失败项（工具失败 / 子代理失败）→ 收起（详见[工作行](./panel-chat-work.md)「工作行模型」）
- 打开项被移出列表（会话清空等）时自动收起；条目状态信号变化会触发 `reconcile`（`WorkRunGroup.vue` 的 `watch` 兜底）

- **加载/运行动画**（任一工具 chip 的 `status === 'running'` 时整组挂上 `work-run-group--running`，但**只有工具 chip 自身呼吸**）：
  - **工具专属 SVG 动画**：运行中的图标播放该工具图标本体路径的专属动画（`ToolIcon` 的 `loading` prop，只动既有路径元素、不叠加任何轨道元素）——bash 下划线光标闪烁 / 文件折角透明度脉动 / 代码尖括号交替内收 / 清单对勾反复描画 / 计算器按键波浪点亮 / 搜索放大镜小幅巡游 / 子代理两条连线依次描画（任务下发感）/ 消息气泡弹出回弹（发送感）/ 设置齿轮匀速转动 / 工作区文件夹呼吸 / 编排四点依次点亮 / 默认扳手小幅拧动；位移/旋转类动画均用 `transform-box: view-box` 锁定以图标中心为原点
  - **chip 呼吸**：运行中的工具 chip 播放 1.6s 呼吸脉冲（`.work-run-group--running .work-run-group__chip--tool` → `work-run-pulse`），仅 box-shadow 在常态与 `var(--panel-accent)` 24% 发光之间往复，不位移、不缩放——运行感来自光晕而非抖动，避免图标行在消息流里"跳动"；同排的思考/子代理 chip 不参与（它们各自有活动态呼吸）
  - **展开反馈**：点击展开详情卡时卡片以 0.18s `work-run-enter`（透明度 0→1 + 上移 4px）浮现；chip 本体 hover 上浮 1px
  - 运行态动画纯视觉（`status` 驱动），不参与逻辑；减少动态偏好（`prefers-reduced-motion`）暂未单独降级

## 图标行：展开态与委派标签

- **展开态**：点击某个 chip，该行下方浮现该调用的具体内容卡片（图标 + 名称（`spawn_subagent` 显示任务摘要）+ 时间 + 耗时 + 状态胶囊（running / succeeded / failed / unknown「未收到回执」）+ 输入/输出 `<pre>`，`max-height: 220px` 内滚动）；再点或点其他 chip 切换/收起；一次只展开一项
- **委派标签**：`spawn_subagent` 图标行/详情头部显示**任务摘要**而非工具名（解析 Core 时间线摘要串 `{task=… · timeout_secs=…}` 与原始 JSON 两种形态，80 字符截断）——「这次派了什么活」一眼可见
