---
id: panel-interaction
title: "Panel 交互定义"
group: 前端
x: 2720
y: 500
link: ["panel-desktop-layout | 电脑端布局 | b>t", "panel-mobile-layout | 移动端布局 | b>t", "panel-chat | 会话视图 | b>t", "panel-modals | 模态与覆盖层 | b>t", "panel-settings | 设置视图 | b>t", "panel-instance-tasks | 实例管理·任务·Shell | b>t", "panel-system | 系统交互 | b>t", "panel-icons | 图标体系 | b>t"]
---

# Panel 交互定义

> **定位**：Panel 前端的**设备无关交互契约**——有哪些功能、每个功能的状态与动作、反馈与失败语义、导航关系。这里不写像素、不写列、不写抽屉:凡「在宽屏该怎么摆」「在手机上该怎么收」都归布局文档（[电脑端](./panel-desktop-layout.md) / [移动端](./panel-mobile-layout.md)）。读者：要判断「某操作该不该确认」「某状态该显示什么」的开发者；实现位置以 `web/src/` 相对路径标注；渲染基元来自 `@echolab-auto/ui-frame`。

## 一、核心原则

1. **Core 是状态的唯一事实来源**——前端不持有业务真相，一切展示由 WS 事件流驱动（本地时间线缓存只是加速副本，用于刷新加速）；断连重连后按缓存游标增量补齐、缓存失效时由 Core 回退全量；唯一例外是**取消任务的乐观中断**（先本地把运行中态置为已取消，再由 Core 事件确认）
2. **实时优先**——推理/工具/回答按事件到达顺序即时渲染，不缓冲等待（规范见 [会话视图](./panel-chat.md)「主时间线时序与动画规范」）
3. **危险操作显式确认**——删除/清理走确认（两步确认或原生 confirm）；升级类操作不可静默执行
4. **编辑即生效**——实例门控、启停开关等操作无草稿态，点击立即下发命令
5. **加载 / 空 / 错误三态区分**——「（暂无…）」空态只在**确认无数据**时展示；请求等待期由 `pending.ts` 统一跟踪（`sendPending` 登记请求 → 响应事件销账 → 150ms 延迟显示 / 6s 慢提示 / 20s 超时重试，详见 [Panel 前端](./panel.md)「状态管理与数据流」），各面板以 `LoadHint` 呈现等待与错误；断连（最高优先态）由视图判断 `state.connected`，优先于加载态
6. **首屏优先、重内容按需**——传输与渲染预算优先保证首帧：骨架屏先行（内联于 `index.html`，JS/CSS 传输期间即反馈）、ui-frame 组件子路径按需导入、设置视图 / 右栏卡片 / Markdown 渲染等重内容分包延迟加载；首屏引导只发有消费方的命令（技能/工具目录推迟到设置页打开或区域切换时拉取）；后台标签页暂停轮询（详见 [Panel 前端](./panel.md)「前端分包与首屏」与[电脑端布局](./panel-desktop-layout.md)「连接生命周期」）

## 二、视图层级

三个主视图，**同一时间只渲染一个**：

```prodoc-flow
graph TB
  Root[Panel 主视图] --> Chat[会话视图]
  Root --> Sets[设置视图]
  Root --> ShellV[Shell 视图]
  Sets --> Api[API 设置]
  Sets --> Skills[技能]
  Sets --> Tools[工具]
  Sets --> Plugins[插件]
  Sets --> Teams[智能体]
  Sets --> Logs[日志]
```

| 视图 | 职责 | 进入方式 |
| --- | --- | --- |
| 会话 | 消息时间线、输入、入口行、实例/任务/工作区弹层 | 默认视图；顶栏「聊天」（桌面）或视图菜单（移动端） |
| 设置 | API / 技能 / 工具 / 插件 / 智能体 / Core 连接 / 联邦 / 日志 | 顶栏「设置」或视图菜单 |
| Shell | 单条 Shell 会话的终端详情（输入、输出流、停止） | 侧栏 Shell 卡「详情」**或**视图菜单（v2：移动端新增入口） |

- **模态与覆盖层**（Branch / Agent 配置 / 上下文）浮于全部视图之上，见[模态与覆盖层](./panel-modals.md)
- **Shell 视图的空态是显式契约**：没有任何 shell 会话时，视图渲染说明与「回到聊天」出口，不留白（v2 起；此前该视图只能经 Shell 卡进入、必然带目标会话，故无空态）

## 三、导航契约

```prodoc-flow
graph LR
  Chat[会话视图] -->|切视图| Sets[设置视图]
  Chat -->|切视图| Shell[Shell 视图]
  Sets -->|切视图| Chat
  Shell -->|返回| Chat
  Side[点选会话项] -->|强制回到| Chat
```

1. **当前视图按标签页持久化**（`echo-panel-view`：本标签页 `sessionStorage` 权威 + `localStorage` 作新标签默认；非法值回退 `chat`）
2. **点选会话项强制切回会话视图**（`App.vue::selectSession` → `setActiveSession`）——会话是「看着某个上下文说话」的动作，停在设置页没有意义
3. **Shell 视图无目标会话时自动落到第一个**（当前 core 下），全部没有则显示空态
4. **视图切换不销毁状态**：会话视图用 `KeepAlive` 缓存（切到设置/Shell 再回来时保留输入草稿、附件、滚动位置与工具组展开态）

## 四、功能模块清单

每个模块给**状态 / 动作 / 反馈**三列；布局细节见对应文档。

| 模块 | 状态 | 动作 | 反馈 |
| --- | --- | --- | --- |
| 消息时间线 | 六类事件（用户/Agent/工具/推理/系统/分支）、流式增量 | 上滑加载、回到底部 | 流式 40ms 合批、`AgentOutput` 全量收敛替换；见[会话视图](./panel-chat.md) |
| 工具调用 | running / succeeded / failed 三态 | 展开看入参与输出 | 连续工作条目合并为 chip 行，点击展开 |
| 推理块 | 实时投影 + 会话日志双写 | 展开/折叠 | 思考条目刷新后不重排（历史回填与实时同形） |
| 输入区 | 文本 + 附图（粘贴/选择）+ 草稿 | 发送、取消任务 | 发送后乐观显示；取消走乐观中断 |
| 入口行 | 8 个潜在控件（Agent 切换、配置、上下文、清单、会话、工作区、任务、实例），按可用性 `v-if` | 点开对应弹层 | 弹层为 `PanelPopover`；能力不可用时入口不渲染 |
| 实例门控 | 门控模式 / 白名单 / 黑名单 / owner / 登录态 | 即时下发 `InvokeInstanceOp` | 编辑即生效；运营态经 `InstanceState` 回推 |
| 任务与清单 | 任务列表（按会话过滤）、清单进度 | 查看、跳转 | 只读视图；空态区分「无任务」与「加载中」 |
| Shell | 会话列表、单会话输出流 | 新建、输入、停止 | 输出流式追加；会话列表来自 `ShellSessionsList` |
| 设置 | 六个分类 + Core 连接/联邦/日志 | 编辑、保存、安装、重载 | 脏检查（切换分类前确认是否放弃）；安装进度实时可见 |
| 视图导航 | 当前视图、当前会话、Agent 选择 | 切换 | 按标签页持久化；点会话强制回会话视图 |

## 五、交互模块树

```prodoc-flow
graph TB
  Root[Panel 交互定义|/panel/panel-interaction.md] --> L[电脑端布局|/panel/panel-desktop-layout.md]
  Root --> Mob[移动端布局|/panel/panel-mobile-layout.md]
  Root --> C[会话视图|/panel/panel-chat.md]
  Root --> M[模态与覆盖层|/panel/panel-modals.md]
  Root --> Set[设置视图|/panel/panel-settings.md]
  Root --> QT[实例管理·任务·Shell|/panel/panel-instance-tasks.md]
  Root --> Sys[系统交互|/panel/panel-system.md]
  Root --> Ico[图标体系|/panel/panel-icons.md]
  L --> L1[外壳几何与层叠]
  L --> L2[双侧边栏与卡片]
  L --> L3[Agent 切换器]
  L --> L4[连接生命周期]
  Mob --> M1[顶栏与视图菜单]
  Mob --> M2[入口行横滚与提示]
  Mob --> M3[卡片 Sheet]
  Mob --> M4[软键盘与安全区]
  C --> C1[消息列表与流式]
  C --> C2[工具卡与推理块]
  C --> C3[输入区与入口行]
  M --> M1b[分支详情]
  M --> M2b[上下文弹层]
  M --> M3b[Agent 配置弹层]
  Set --> S1[API 与技能]
  Set --> S2[工具与插件]
  Set --> S3[Core 连接与联邦]
  QT --> Q1[实例管理]
  QT --> Q2[任务弹层]
  QT --> Q3[Shell 列表与详情]
  Sys --> Y1[Toast]
  Sys --> Y2[键盘与常量]
```

## 六、设备无关的状态契约

| 契约 | 内容 | 实现 |
| --- | --- | --- |
| 视图持久化 | 当前视图按标签页；新标签回退全局默认 | `tab-prefs.ts` 的 `readPref` / `writePref` |
| 会话持久化 | 当前会话**仅**本标签页（避免多窗口串台） | `tab-prefs.ts` 的 `readTabOnly` |
| Agent 持久化 | 记住上次使用的 Agent（含归属 core） | `echo-panel-active-team(-core)` |
| 断连优先 | 断连态压过一切加载/空态 | 视图读 `state.connected` |
| 事件扇出校验 | 消费方校验事件序号连续性，缺号触发重连补拉 | `event-seq.ts` |
| 多窗口 | 每个标签页一条 WS、一份内存 `state`；动作本身全局生效 | 见[电脑端布局](./panel-desktop-layout.md)「多窗口语义」 |

> 本节点是前端文档群的总入口；两篇布局文档描述同一套交互契约在不同视口下的落地形态，**内容不重复**：交互定义讲「做什么」，布局文档讲「摆在哪」。
