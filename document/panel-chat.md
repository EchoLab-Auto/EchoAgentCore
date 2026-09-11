---
id: panel-chat
title: "Panel 会话视图"
group: 前端模块
x: 1186
y: 1301
---

# Panel 会话视图

会话（聊天）视图的全部交互契约：消息列表与角色渲染、工具卡生命周期、推理块与活动浮条、输入区（发送/图片/取消任务）、入口行按钮与弹出层。

## 七、会话视图（聊天）

### 7.1 消息列表

- **自动滚动**：仅当用户处于底部 120px 阈值内时跟随新内容；上翻后出现「回到底部 ↓」按钮（平滑滚动，500ms 后解除锁定）。滚动区内 padding-bottom 190px、按钮抬升至 bottom 180px——为悬浮玻璃输入区让位
- **入场动画**：0.28s 淡入 + 上移 6px，仅实时消息（`animate: true`）播放；历史回放不播
- **容量**：主时间线上限 1024 条，溢出裁最旧
- **会话过滤**：「全部消息」（global）显示当前 Agent 全部会话的消息；具体会话仅显示该会话；从全局视图发送会路由到本地会话 `local:tui::local_user`（`ChatView.vue:176-182`）
- **空列表占位**：全局视图与具体会话文案不同（`（会话 X 暂无消息…）`，`ChatView.vue:184-188`）
- **图片**：历史消息中的图片渲染在气泡上方，最大 260×200px（`ChatView.vue:385-393, 634-642`）；正文经 `chat-adapter.ts::compactForDisplay` 压过——附带的 data URI → `[图片#n]`、其余长内联 base64 → `[图片数据已省略]`，与 Core 侧占位符一致，避免整屏 base64
- **复制**：消息气泡与工具输出均有复制按钮（库内置）；无任何右键菜单
- **Markdown**：仅 Agent 消息渲染 Markdown
- **时间格式**：zh-CN 24 小时制

### 7.2 消息角色与渲染

| 角色 | 渲染 |
|---|---|
| `user` | 用户气泡（含来源元数据：平台/用户/群） |
| `backend` | Agent 气泡（Markdown） |
| `reasoning` | **Panel 扩展角色**：`#message` slot 拦截渲染 ReasoningBlock（库的 ChatRole 无此角色，不拦截会被误渲染为 Agent 气泡） |
| `tool` | ChatToolCallBlock 折叠卡 |
| `system` | 系统提示行 |
| `branch` | ChatBranchMergeBlock 分支合并卡（当前 reducer 已不产生——分支内容实时进主时间线，此角色保留适配） |

补充来源：定时器触发以 system 消息插入主时间线（`⏰ 定时器触发 · {task}`，task 截断 160 字符）；`adapter_name === 'background'` 的消息走 hook 解析、不进时间线（`state.ts:802-807`，`helpers.ts:26-36`）。

### 7.3 工具卡生命周期

```prodoc-flow
graph LR
  Call[ToolCall 事件] -->|按 tool_call_id 建档| Running[running: spinner]
  Running -->|ToolResult 同 id 配对| Done{succeeded / failed}
  Running -->|旧 core 无 id: 按名回填最新 running| Done
  Done -->|timed_out 或 error: 前缀| Failed[failed: error 标签]
  Done -->|否则| Ok[succeeded: success 标签]
```

- 输入显示为 `key=value` 摘要（单值截断 120 字符，非 JSON 兜底截 200 字符）；输出上限 4000 字符；输入+输出非空才可展开
- **配对兜底**：ToolResult 到达时主时间线找不到 running 条目（如恢复后的时间线）——追加一条已完成工具卡而非丢弃（`timeline.ts:44-53`）；增量重投递按 tool_call_id **就地修补**已有工具卡，避免 core 带新 seq 重投完成态时出现重复行（`timeline.ts:131-146`）
- **中断标记**：历史回放时 `output==null` 且未失败的工具恢复为 running（`timeline.ts:58-61`）；「已中断」标记完全由 core 侧重启清理写入（`session.rs`，output = 「[已中断] Core 服务重启导致本次调用未返回，可重试」），panel 自身不做兜底标记

### 7.4 推理块（ReasoningBlock）

- **实时**：打字机逐字——24ms/tick，每 tick ≥2 字符，总时长约 6s 封顶（按文本长度自适应提速）；紫色左边框 + 「思考」标签 + 输入中提示「正在推演…」+ 闪烁光标 + 呼吸点
- **历史**：一次性全量显示，无动画
- 动画纯视觉层（`animate` 元数据不参与逻辑）
- **折叠（2026-09，默认折叠）**：头部可点击开合（`aria-expanded` + 标题提示 + 折叠态字数徽标「N 字」）——静止态（历史回放 / 推演结束）折叠为一行头部；**推演中自动展开**（用户手动操作过则尊重其选择，不再自动开合）
- **推演滚动跟随**：思考框体为**限高滚动窗口**（max-height 180px），打字机每次推进后（`flush: 'post'`）滚动钉底（`scrollTop = scrollHeight`）——推演中始终可见最后一行，不撑爆时间线；推演结束后手动展开时滚动复位到顶部（从头阅读）

### 7.5 活动浮条

输入区上方的胶囊浮条（仅当前会话忙碌时可见）：旋转 spinner（0.8s）+ 相位文案——`正在思考：{detail}…` / `正在调用工具 {detail}…` / `子代理工作中…`（320px 省略截断）。

### 7.6 输入区

- **发送**：Enter（IME 组合输入守卫，`isComposing`/229 不触发）；Shift+Enter 换行；空文本（trim 后）不发送；发送后清空
- **文本域**：1 行起自适应撑高，上限 `calc(8em + 20px)` 后内部滚动
- **断连禁用**：`disabled` + placeholder `未连接到 Core，暂时无法发送`
- **图片**：粘贴监听挂在整个会话视图容器（剪贴板带文件即 `preventDefault`，`image/*` 过滤在入队时，`ChatView.vue:226-246, 261`）或附件按钮多选；统一 canvas 缩放至最长边 1600px 后重编码——PNG 仅在源码体积 ≤1.1MB 时保持 `image/png`（保透明），其余转 JPEG q0.85，编码结果超 `MAX_ATTACHMENT_CHARS`（1.5M 字符）则逐级缩小（×0.65，下限 320px；多模态端点的请求体积与文本 token 都受不了数 MB 的无损 PNG）；待发附件 64×64 缩略图 + `×` 移除；随 `SendMessage.images` 发送，发后清空
- **取消任务**：仅当前会话忙碌时出现在操作区。点击 = **先本地乐观中断、再下发命令**（`App.vue:131-143`）：
  1. 本地 `cancelSessionWork`（`state.ts:868-909`）立即生效——活动相位 → completed（浮条/取消按钮即时消失）；该会话 running 任务及其分支 → cancelled；主时间线与分支 tab 内 running 工具卡 → failed 并写入「（已取消）」
  2. 下发 `CancelRequestedWork{session_id, all:false, team_id: 当前Agent}`——`team_id` 必须携带，否则命令路由到默认 Agent，self-coding 场景下"取消 0 个任务"（2026-09-02 修复）
  3. 库组件自带的取消按钮不渲染（`cancelable: false`，`@cancel` 仅作转发）
- 发送按钮为库 `#actions` slot 自绘（替换默认按钮，保持与输入区新拟态风格一致）

### 7.7 入口行按钮与弹出层

入口行位于输入区上方（`bottom = 输入区高度 + 24px`，ResizeObserver 跟踪）。**按钮组件化（2026-09）**：四个入口按钮统一使用 ui-frame `NeumorphismButton`（`variant=glass` 磨砂半透明、`shape=pill` 胶囊、`size=small`）；清单数字徽标使用 `NeumorphismBadge`（右上角、`showZero=false`、0 时隐藏）；模板 ref 取组件实例的 `$el` 定位弹层（`btnElement()` 辅助）。按钮只保留布局微调（图标 14px）；作用域内覆盖玻璃令牌使按钮更透（`.entry-row` 内 `--nm-glass-bg` 降至 45% 不透明度、`--nm-glass-blur` 提至 24px）：

| 按钮 | 行为 |
|---|---|
| Agent 切换 | 见 §六 |
| 配置 | 打开 AgentConfigModal（当前 Agent 的能力配置）；无激活 Agent 时回退 teams[0]（`ChatView.vue:146-151`） |
| 上下文 | 打开 ContextView 弹层（trunk 上下文明细；几何与「配置」弹层一致，点遮罩关闭，无返回按钮） |
| 清单 | **仅当当前 persona 的 `echo-agent.checklist` 插件与 `checklist` 工具均可用时显示**；弹出清单卡（徽标 = 清单数）：内嵌 SidebarCard 折叠卡（默认展开、计数徽标），各清单进度条 + ☑/☐ 项（完成项加粗），只读；插件热卸载后入口、浮层与徽标立即移除并清空本地清单状态；空态「（暂无清单）」 |
| 适配器 | 弹出 QQ 管理面板（§十）；仅当前 Agent 启用适配器插件时显示；切换 Agent 强制关闭 |

弹出层宽 `min(520px, 82vw)`，锚定按钮上方 8px、相对输入区水平居中。**清单/适配器弹出层没有全屏遮罩**——是 fixed 定位的内容尺寸面板，仅点到弹层自身 padding 空白（`@click.self`）才关闭，点弹层外的聊天区不关闭（`ChatView.vue`）。**「配置」「上下文」是会话区内锚定的磨砂玻璃弹层**（上/左/右 12px，底边 = 入口行高 + 42px），点弹层外遮罩关闭；上下文弹层无返回按钮（2026-09 起与配置弹层同几何）。

