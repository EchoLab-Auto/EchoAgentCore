---
id: chat-input
title: "输入区（ChatComposer）"
group: 前端
x: 5120
y: 1060
---

# 输入区（ChatComposer）

> **定位**：会话视图底部输入区的交互——发送、文本域、图片附件、取消任务的乐观中断，以及文本域下方的运行状态行（会话生效 API + 相位文案）。读者：维护 `ChatView` 输入侧与库 `ChatComposer` 集成的开发者。上游：[会话视图](./chat.md)；上方的功能条与弹层见[入口行](./chat-entry.md)。

## 发送

- **键位**：Enter 发送；Shift+Enter 换行。IME 组合输入期间不触发（判 `isComposing` 与 `keyCode === 229`——中文/日文输入法按 Enter 选词时**不能**被当成发送）
- **准入**：`trim()` 后为空不发；`disabled`（未连接 Core）时不发
- **发送后**：清空文本、清空待发附件；会话区滚到底部
- **按钮**：库 `#actions` slot 自绘（替换库默认发送按钮，保持与输入区新拟态风格一致）；`canSubmit` 为假时 `disabled`

## 文本域

- **自适应高度**：1 行起随内容撑高，上限 `calc(8em + 20px)`（≈5 行 ×1.6em 行高 + 上下 padding 10×2）后转为内部滚动——上限同时写在 `max-height` 与 `overflow-y: auto` 上，缺一不可
- **字号**：桌面 14px；**iOS 上 16px**（<16px 聚焦时 Safari 会自动放大整个页面，属框架级对策，见[移动端布局](./mobile-layout.md)）
- **高度联动**：`--chat-control-h = 内边距×2 + 字号×1.6` 是「单行内框高」的唯一事实来源，附件/发送/取消三个按钮与它等高，字号一变自动跟随（与入口行无关，纯输入区内部几何）
- **断连占位**：placeholder 换成 `未连接到 Core，暂时无法发送`，同时 `disabled`

## 外壳：ui-frame 卡片

输入区的外壳是一个 **`NeumorphismCard`（`glass` 档 + `no-padding`）**，不再自绘描边层与玻璃层：

```
.chat-composer-float            定位层（absolute；pointer-events: none）
├── __halo                      投影补层（仅移动端 clip 生效时显示，见下）
└── NeumorphismCard.__card      卡片：材质 / 圆角 / 向外环境投影（glass="float"）
    └── __body                  内层：padding + 内容（附件 / ChatComposer / 状态行）
```

- **卡片负责**：半透明底（`--nm-glass-bg`）、`backdrop-filter`、**`glass="float"` 档的向外环境投影**（`0 4px 16px` ambient + `0 2px 4px` 加深）—— 视觉上是一块**浮在消息之上的磨砂托盘**（悬浮，不是凹槽）。
- **轮廓也归卡片**：不再由面板写 `clip-path`，而是声明库的两个 prop —— 移动端传 `:clip-path`（设备屏角的偏移曲线），桌面/直角屏传 `:squircle="2.5"`（按卡片生效的 `border-radius` 生成等半径超椭圆）。超椭圆几何内核在 `@echolab-auto/ui-frame/utils`（`sampleCornerOffset` / `squircleClipPath` / `pointsToPathData` …），本仓库的 `src/utils/squircle.ts` 只剩 Panel 特有的组合 `composerOutline` / `composerGeometry` / `composerCornerSpan`。
- **移动端四角等大**（用户报「上两个圆角和下两个圆角不相等」）：轮廓的下角是屏幕角偏移曲线，上角是它关于水平中轴的**镜像**，两角共用同一个「跨度」`R − 3vw`（留白 = 屏宽 3%）。跨度放不下时**加厚外壳内边距**（`--composer-padding = max(10px, (2×跨度 − 输入行 − 状态行) / 3)`），而不是让上角吃掉剩余高度 —— 于是四角同形同大（镜像偏差 0）且下角间隙仍恒定（波动 ≤0.06px），输入行与按钮保持在 45.6 的触控基准。详见 [移动端布局](./mobile-layout.md)。
- 面板仍负责圆角**档位**：`--composer-radius`（桌面 24）+ `--composer-corner-r`（窄屏 = 实际跨度，供内层派生同心圆角），选择器带 `.chat-composer-float` 前缀压过卡片自己的 `--nm-border-radius-*`。
- **`ChatComposer` 侧另有一处内辉光**：`focus-effect="glow"` —— 聚焦时主色光从输入槽**内部**亮起（`inset 0 0 18px` 光晕 + 1px 内描边，抑制默认外环）。
- **`backdrop-filter` 降到 10px**（库默认 20px + saturate）：用 `--nm-glass-blur` 变量覆盖，沿用拖选性能实测结论。聚焦/拖选期间 `.chat-composer-float--busy` 直接关掉 blur 并换成不透明底色。

### 悬浮：库的 float 档 + 一层投影补层

外壳的悬浮感来自库的 `NeumorphismCard glass="float"`（向外环境投影 + 半透明底 + `backdrop-filter`），面板侧不写 `box-shadow`。

**但移动端必须补一层**（`__halo`），原因是 `clip-path`：

> clip 裁的是元素**整个渲染结果**，连同它自己的外阴影一起裁掉。实测注入 6px 外投影红环：clip 生效只剩 955 个红像素、关掉 clip 是 47939 —— 差 50 倍。更进一步：**关掉 halo 与关掉卡片 box-shadow 两张截图逐像素相同**（差 ≤1），所以移动端 `glass="float"` 的向外投影贡献为 0，外部明暗 100% 来自 `__halo`。

所以手机上（贴 iPhone 超椭圆屏角、clip 生效）投影画不出来，只能由未裁剪的**兄弟节点**补。它是**两层**（因「边框有点下宽上窄」而拆分）：

| 层 | 职责 | 实现 |
| --- | --- | --- |
| `__halo` | 柔化（两层的模糊一起做） | `filter: blur(10px)`，本体**不再位移** |
| `__halo::before` | 环境层：四周均匀，给上边补边缘定义 | `background: var(--composer-halo-ambient)`（.08）+ 同一 `clip-path` |
| `__halo::after` | 落地层：保留「光从上方来」的方向感 | `background: var(--composer-halo)`（.12）+ `translateY(4px)` + 同一 `clip-path` |

**为什么必须两层**：单层 `translateY(12px) + blur(16px)` 而底部只有 12px 余量（间距 = 屏宽 3%），位移 + 模糊 = 28px 被视口硬切，底边挂一条浓暗带（峰值暗化 Δ29 vs 顶部 Δ13）；卡片又是 65% 半透明 + backdrop-filter，影子还透过玻璃把**卡片内部**下半区压暗（Δ13 vs Δ8）。人眼锚定「轮廓旁暗带的宽度和对比度」，于是读成「下宽上窄」—— 而折线本身的镜像偏差其实是 0.00px。

改成双层 + 位移降到 4px 后：顶/底峰值 **1.17×**（原 2.2×）、暗化积分 **1.39×**（原 3.6×）、卡片内部上下差 1（原 5）。

- **默认 `display: none`**，只在 `.chat-composer-float--clipped` 下开 —— 桌面没有 clip，卡片自带投影已够，叠一层就是双重阴影。
- **绝不能挂 `backdrop-filter`**：`filter` 会创建新的 backdrop root，卡片的毛玻璃会失效。
- 深色主题两层各自调：`--composer-halo-ambient: rgba(0,0,0,.28)`（环境层，避免四周发闷）、`--composer-halo: rgba(0,0,0,.40)`（落地层）。

### 输入槽的聚焦内辉光

输入槽自己另有一处内辉光：`<ChatComposer focus-effect="glow">` —— 聚焦时主色光从槽**内部**亮起（`inset 0 0 18px 1px` 光晕 `--nm-primary-color` 34% + `inset 0 0 0 1px` 内描边 72%，并抑制默认外环）。

所以整体是「**悬浮的磨砂外壳 + 聚焦时内部发光的输入槽**」：外壳的悬浮与输入槽的聚焦反馈是两件事，互不冲突。

- **没有独立描边层**：`--composer-border` 归零，1px 边由卡片的 `inset 0 0 0 1px` 承担。

## 状态行

文本域（含其右侧操作行）**下方**的一行运行态提示，两段：

```
[ deepseek / deepseek-flash ]  ◌ 正在思考…
└────── API 徽标 ──────┘        └─ 相位文案 ─┘
```

**整行常驻**（用户要求）：只要拿得到 API 信息，`idle` 时也显示 API 徽标 ——
「随时能看出这条回答是哪个 API 产出的」比"空闲时省 23px"重要。右侧相位文案仍只在
忙碌时出现；只有两者都为空（`apiLabel` 与 `activityText` 皆空）时整行才不渲染。

高度在两种状态下**完全一致**（`.chat-status { min-height: 17px }`）：否则开始回答
的瞬间输入区会突然长高 23px、入口行跟着跳一下。代价是输入区常驻 91px（原 68px），
消息区 `padding-bottom` 的余量仍够（402 宽实测占用 108 ≤ 130 − 8）。

- **位置**：`.chat-composer-float__body`（卡片内层）的最后一项——即在输入区自己的圆角轮廓里、文本域之下（实测文本域底 823 → 状态行顶 829，间隙 6px）。**不在[入口行](./chat-entry.md)里**：入口行是 `display:flex` 的横滚工具条，插入/移除子项会推着 Agent 切换器到「模型」整排按钮左右移动，文案变长还会抖动；搬进输入区后入口行排布恒定

### 三处等距

```
┌─ 外壳（卡片）──────────────────────────┐
│              ↑ P                        │  ① 输入框上边框 → 外壳上边框
│  ┌─ 输入框 ─────────────┐               │
│  └──────────────────────┘               │
│              ↕ P                        │  ② 输入框下边框 → API 徽标
│  deepseek / deepseek-flash              │
│              ↕ P                        │  ③ API 徽标 → 外壳下边框
└─────────────────────────────────────────┘
```

三处都等于 `P = --composer-padding`（桌面 12 / 窄屏 10）。它们分属三个不同的盒子：

| # | 间距 | 由谁决定 |
| --- | --- | --- |
| ① | 输入框上边框 → 外壳上边框 | `__body` 的 `padding-top` |
| ② | 输入框下边框 → API 徽标 | 状态行的 `margin-top: var(--composer-padding)` |
| ③ | API 徽标 → 外壳下边框 | `__body` 的 `padding-bottom`，**前提是徽标盒高 = 状态行行高** |

③ 是唯一有陷阱的一条：状态行是 `display:flex; align-items:center`，若徽标盒比行矮，行内会剩一点居中留白，量出来就小于 P。所以：

- 行高由 `--chat-status-h`（17px）单一来源给出（`min-height`）；
- 徽标的 `line-height` 取**同一个变量**（配 `display:inline-flex; align-items:center` 居中字形），盒子正好铺满行高，上下各 0 余量。

三处都由 `--composer-padding` / `--chat-status-h` 派生，所以「改内边距就三处一起变」是结构性的。**有布局断言守着**（`layout:mobile` 的「三处间距都等于外壳内边距」）：5 个视口实测 `P=10 g1=10 g2=10 g3=10`，最大偏差 0px；桌面档 P=12 时文本域高度是小数（45.598），g1 带 0.39px 亚像素偏移，断言容差 0.6px。

### API 徽标（左）

显示**本会话实际生效**的 `provider / model`（`ChatView.vue` 的 `apiLabel`），解析顺序与会话弹层的「会话级 API」下拉严格同源：

1. 会话级覆盖 `state.sessionApiOverrides[sessionId]`
2. persona 级 `state.api.active_api`
3. 顶层默认配置（`state.api.provider` / `state.api.model`）

为什么必须按这个顺序而不是直接读[入口行](./chat-entry.md)的「模型」按钮：那个按钮显示的是 **persona 级** `active_api`，而工作区对话可以单独指定 profile（会话级覆盖），两者会不一致——看按钮会看错「这条回答是谁在产出」。悬停 `title` 补两层信息：`来源：本会话覆盖 / persona 激活 / 顶层默认配置 · profile「名字」`。

呈现：**纯文字**（用户要求去掉外框）——不设 border / 背景 / 胶囊，静态信息不需要第二层容器，套壳反而和输入区卡片抢层次。与右侧相位文案靠色阶区分：标签 `--nm-text-primary` + `opacity: .75`（实），相位文案 `--nm-text-secondary`（淡）。

几何：`flex: 0 0 auto`（不参与收缩，避免被相位文案挤掉）+ `max-width: 55%`（超长 provider 名内部省略）。55% 不是随手取的——`deepseek / deepseek-flash` 在 320 宽下需要 ~148px，45% 会吃掉最后一两个字符。

### 相位文案（右）

`ChatView.vue` 的 `activityText`，按 `state.activities[key].phase` 三分支：

| phase | 文案 | 由谁置位（`state.ts`） |
|---|---|---|
| `thinking` | `正在思考：{detail}…`（无 detail 时「正在思考…」） | `LlmRequest`（发出请求、等模型）；`ToolResult`（工具跑完、等模型继续，detail 为空）；`SubagentCompleted`（detail = 「正在整合 subagent 结果」/「subagent 失败，正在恢复」） |
| `tool` | `正在调用工具 {detail}…`；该工具对当前 persona 已禁用时降级为「正在处理…」 | `ToolCall`（`tool_name` 非 `run_subagent`） |
| `subagent` | `子代理工作中：{detail}…`（无 detail 时「子代理工作中…」；detail 截 72 字符） | `ToolCall`（`run_subagent`）或 `SubagentSpawn` |

`completed` / `failed` 不产生文案 —— 整行卸载。

- **截断**：单行长 `text-overflow: ellipsis`；窄屏 `max-width: 62vw`——子代理任务名上限 72 字符，不截断会撑高输入区、吃掉消息区可视高度
- **无障碍**：live region 只包相位文案（`.chat-status__live`，`role="status"` + `aria-live="polite"`）——API 名是静态信息，重复播报只会吵；spinner 为装饰，`aria-hidden`
- **带来的一次位移**：状态行出现/消失让输入区高 68 → 91px，入口行按 `entryBottom = 输入区高 + 24` 同步上移（ResizeObserver），二者间隙恒定（桌面 7px / 移动端 10px，均等于 `24 − 屏幕留白`）。消息区 `padding-bottom` 是固定值（桌面 190 / 移动 130），状态行只占 23px，余量足够

## 图片附件

- **图片**：粘贴监听挂在整个会话视图容器（剪贴板带文件即 `preventDefault`，`image/*` 过滤在入队时，`ChatView.vue` 的 `onPaste`）或附件按钮多选；统一 canvas 缩放至最长边 1600px 后重编码——PNG 仅在源码体积 ≤1.1MB 时保持 `image/png`（保透明），其余转 JPEG q0.85，编码结果超 `MAX_ATTACHMENT_CHARS`（1.5M 字符）则逐级缩小（×0.65，下限 320px；多模态端点的请求体积与文本 token 都受不了数 MB 的无损 PNG）；待发附件 64×64 缩略图 + `×` 移除；随 `SendMessage.images` 发送，发后清空

## 取消任务

- 仅当前会话忙碌时出现在操作区。点击 = **先本地乐观中断、再下发命令**（`App.vue` 的 `cancelWork`）：
  1. 本地 `cancelSessionWork`（`state.ts::cancelSessionWork`）立即生效——活动相位 → completed（状态行/取消按钮即时消失）；该会话 running 任务及其分支 → cancelled；主时间线与分支 tab 内 running 工具卡 → failed 并写入「（已取消）」
  2. 下发 `CancelRequestedWork{session_id, all:false, team_id: 当前Agent}`——`team_id` 必须携带，否则命令路由到默认 Agent，self-coding 场景下"取消 0 个任务"；后端取消成功即回 `AgentCompleted` 对齐状态
  3. 库组件自带的取消按钮不渲染（`cancelable: false`，`@cancel` 仅作转发）
