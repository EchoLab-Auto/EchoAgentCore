---
id: index
title: "EchoAgent 文档总览"
group: 总览
x: 160
y: 80
link: ["architecture | 架构总览", "core | Core 后端 | r>l", "panel | Panel 前端 | r>l", "protocol | 协议与数据流 | r>l", "deploy | 部署与自更新 | r>l", "guide | 开发指南 | r>l"]
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板**框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 模块实例；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、任务、Shell、设置（API/技能/工具/模块/智能体/日志）等界面。

> 本文档群按 ProDoc 文档图组织（每个 `.md` 是画布上一个框，连线表达主干导航），物理目录按「主题 × 阅读距离」分层（`overview → core → modules / panel / protocol / federation → ops / dev`）——放置规则与写作约定见 [文档规范](./documentation-guide.md)。两仓库（core + panel）的文档统一维护在本目录。

## 阅读路径

- **理解系统怎么组成**：本页 → [架构总览](./overview/architecture.md) → [Core 后端](./core/core.md) → [框架（内核）](./core/frame.md) → [模块化设计](./modules/modules.md)
- **运行时子系统**：[多 Agent 与会话](./core/agents.md) → [Agent 循环](./core/agent-loop.md)；[会话记忆](./core/memory.md)、[多模态输入](./core/multimodal.md)、[配置持久化](./core/config-persistence.md)
- **扩展系统**：[模块化设计](./modules/modules.md) → [工具系统](./modules/tools.md) / [技能系统](./modules/skills.md) / [Subagent 模块](./modules/subagent.md)；外部模块作者读 [模块开发指南](./modules/module-authoring.md)
- **安全**：[敏感信息隔离（脱敏服务）](./core/security-redaction.md)——出口卡口与保证边界
- **前端**：[Panel 前端](./panel/panel.md) → [交互定义](./panel/interaction.md) → 各视图文档
- **运维排障**：[部署与自更新](./ops/deploy.md)、[协议与数据流](./protocol/protocol.md)

## 分层导航

**总览层 —— 入口与骨架**

- [architecture.md](./overview/architecture.md) — 架构总览：crate 布局、能力接缝、事件、会话与扩展点地图
- [index.md](./index.md) — 本页：文档总览、阅读路径与分层导航

**核心层 —— 内核机制与运行时子系统**

- [core.md](./core/core.md) — Core 后端入口：组成、进程与运行速览、配置入口
- [frame.md](./core/frame.md) — 框架（内核）：组合装配、服务定位、事件总线、模块宿主、进程结构
- [agents.md](./core/agents.md) — 多 Agent 与会话：persona 装配、会话模型、工作区会话、能力隔离
- [agent-loop.md](./core/agent-loop.md) — Agent 循环：单/并行模式、Turn 状态机与默认驱动 TurnRunner
- [memory.md](./core/memory.md) — 会话记忆：事件溯源日志、可见性边界、投影与切片、压缩归档、显示时间线
- [multimodal.md](./core/multimodal.md) — 多模态输入：媒体库、模型侧还原、token 卫生
- [config-persistence.md](./core/config-persistence.md) — 配置持久化：ConfigStore、门控/会话持久化、原子写入
- [logging.md](./core/logging.md) — 日志与可观测性：出口与格式、打点约定、span 关联、轮转、级别热更新
- [security-redaction.md](./core/security-redaction.md) — 敏感信息隔离：威胁模型、出口卡口与保证边界

**模块层 —— 模块体系与各模块机制**

- [modules.md](./modules/modules.md) — 模块化设计：模块模型、内置清单、能力开关、外部进程模块
- [tools.md](./modules/tools.md) — 工具系统：注册分发、路径约定、超时治理、后台 Shell 会话
- [tool-catalog.md](./modules/tool-catalog.md) — 内置工具目录（**自动生成**：加/改工具后由门禁校验新鲜度）
- [skills.md](./modules/skills.md) — 技能系统：分层、SKILL.md、热重载、系统提示词技能
- [subagent.md](./modules/subagent.md) — Subagent 模块：异步委派、隔离上下文、完成回报
- [qq-gating.md](./modules/qq-gating.md) — QQ 实例门控：五层管道、多实例、消息收发与渲染
- [module-authoring.md](./modules/module-authoring.md) — 模块开发指南：快速上手、装载、协议约定与本地验证
- [federation.md](./federation/federation.md) — 联邦（多机去中心化）：链路协议、安全模型、远程工具与调度

**协议层**

- [protocol.md](./protocol/protocol.md) — 协议与数据流：传输信封、命令/事件、事件流连续性、时间线同步、兼容性规则

**前端层 —— Panel 与视图交互契约**

- [panel.md](./panel/panel.md) — Panel 前端（前端层入口）：仓库布局、中继模型、状态管理与数据流 → 下面三支
- **呈现与参考**：[desktop-layout.md](./panel/desktop-layout.md) — **桌面端布局**（≥768px）：外壳几何、层叠秩序、双侧边栏、卡片、连接生命周期 → [桌面端模态与覆盖层](./panel/desktop-overlays.md)；[mobile-layout.md](./panel/mobile-layout.md) — **移动端布局**（<768px）：顶栏与视图菜单、入口行横滚提示、卡片 Sheet、软键盘与安全区 → [移动端模态与覆盖层](./panel/mobile-overlays.md)；[icons.md](./panel/icons.md) — 图标体系：载体分工、尺寸线宽档位、语义色令牌、防回归守卫；[constants.md](./panel/constants.md) — 关键常量速查 + 协议已定义但 UI 未接线的命令清单
- **功能与视图契约**：[interaction.md](./panel/interaction.md) — **交互定义**（设备无关）：核心原则、视图层级、导航契约、功能模块清单
  - [chat.md](./panel/chat.md) — **会话视图**（总览）：视图构成、三分区职责、组件与数据流 → 消息区（[总览](./panel/chat-messages.md)：[列表与滚动](./panel/chat-list.md) · [历史与实时窗口](./panel/chat-history.md) · [时序与动画](./panel/chat-timeline.md) · [工作行](./panel/chat-work.md) → [工具卡与图标行](./panel/chat-tools.md) / [推理块](./panel/chat-reasoning.md)）、[输入区](./panel/chat-input.md)、[入口行](./panel/chat-entry.md)、[双侧边栏](./panel/chat-rail.md)
  - [settings.md](./panel/settings.md) — 设置视图外壳：八分类菜单、共用契约与界面元素树 → 七篇分类子文档：[API](./panel/settings-api.md) · [技能](./panel/settings-skills.md) · [工具](./panel/settings-tools.md) · [模块](./panel/settings-modules.md) · [智能体](./panel/settings-agents.md) · [Core 与联邦](./panel/settings-cores.md) · [日志与系统](./panel/settings-logs.md)
  - [instance-tasks.md](./panel/instance-tasks.md) — 实例管理·任务·Shell：描述符驱动的模块实例弹层、任务弹层、HTTP 通道
  - [system.md](./panel/system.md) — 系统反馈：Toast 形态与来源、键盘清单、反馈形态的选择

**运维层**

- [deploy.md](./ops/deploy.md) — 部署与自更新：安装、容器化、更新流程、回滚与日常体检

**开发层**

- [guide.md](./dev/guide.md) — 开发指南：仓库结构、关键抽象、编码约定、治理门禁
- [testing.md](./dev/testing.md) — 测试策略：测试分层、Mock 与夹具、质量门禁
- [documentation-guide.md](./documentation-guide.md) — 文档规范：目录分层、单篇体例、模板与放置规则

**调研稿**（`source/research/`，非文档群成员，过程材料不进本目录）

- [system-audit-2026-10](../research/system-audit-2026-10/README.md)、[compat-audit-2026-10](../research/compat-audit-2026-10/core-compat-audit.md)（[panel 侧](../research/compat-audit-2026-10/panel-compat-audit.md)）
- [workspace-conversations-2026-10](../research/workspace-conversations-2026-10/design.md)、[opencode-context-compaction](../research/opencode-context-compaction/README.md)

## 导航地图

```prodoc-flow
graph LR
  Home[文档总览|/index.md]
  Home --> Arch[架构总览|/overview/architecture.md]
  Home --> Core[Core 后端|/core/core.md]
  Home --> Panel[Panel 前端|/panel/panel.md]
  Home --> Proto[协议与数据流|/protocol/protocol.md]
  Home --> Ops[部署与自更新|/ops/deploy.md]
  Home --> Dev[开发指南|/dev/guide.md]
  Core --> Frame[框架（内核）|/core/frame.md]
  Core --> Modules[模块化设计|/modules/modules.md]
  Frame --> Agents[多 Agent 与会话|/core/agents.md]
  Agents --> Loop[Agent 循环|/core/agent-loop.md]
  Frame --> Memory[会话记忆|/core/memory.md]
  Frame --> MModal[多模态输入|/core/multimodal.md]
  Frame --> Persist[配置持久化|/core/config-persistence.md]
  Frame --> Logging[日志与可观测性|/core/logging.md]
  Frame --> Security[敏感信息隔离|/core/security-redaction.md]
  Modules --> Kernel[运行地基|/modules/kernel.md]
  Modules --> Tools[工具集|/modules/tools.md]
  Tools --> ToolCat[内置工具目录|/modules/tool-catalog.md]
  Modules --> ShellModule[Shell 会话|/modules/shell.md]
  Modules --> WsModule[工作区|/modules/workspace.md]
  Modules --> Subagent[Subagent|/modules/subagent.md]
  Modules --> Skills[技能目录|/modules/skills.md]
  Modules --> ProvModule[LLM Provider|/modules/provider-llm.md]
  Modules --> LoopModule[循环|/modules/loop.md]
  Modules --> Sanitize[脱敏|/modules/sanitize.md]
  Modules --> SessModule[会话|/modules/session.md]
  Modules --> TransModule[显示层|/modules/transcript.md]
  Modules --> MgmtModule[管理面|/modules/management-panel.md]
  Modules --> Fed[联邦|/federation/federation.md]
  Modules --> SelfUpd[自更新|/modules/selfupdate.md]
  Modules --> Gating[QQ 实例|/modules/qq-gating.md]
  Modules --> Author[模块开发指南|/modules/module-authoring.md]
  Panel --> Desk[桌面端布局|/panel/desktop-layout.md]
  Desk --> DeskOv[桌面端模态与覆盖层|/panel/desktop-overlays.md]
  Panel --> Mobile[移动端布局|/panel/mobile-layout.md]
  Mobile --> MobileOv[移动端模态与覆盖层|/panel/mobile-overlays.md]
  Panel --> Icons[图标体系|/panel/icons.md]
  Panel --> Consts[关键常量与设计边界|/panel/constants.md]
  Panel --> Inter[Panel 交互定义|/panel/interaction.md]
  Inter --> Chat[会话视图|/panel/chat.md]
  Chat --> ChatMsg[会话消息区|/panel/chat-messages.md]
  ChatMsg --> ChatList[消息列表与滚动|/panel/chat-list.md]
  ChatMsg --> ChatHist[历史快照与实时窗口|/panel/chat-history.md]
  ChatMsg --> ChatTime[主时间线时序与动画|/panel/chat-timeline.md]
  ChatMsg --> ChatWork[工作行|/panel/chat-work.md]
  ChatWork --> ChatTool[工具卡与图标行|/panel/chat-tools.md]
  ChatWork --> ChatRea[推理块|/panel/chat-reasoning.md]
  Chat --> ChatInp[输入区|/panel/chat-input.md]
  Chat --> ChatEnt[入口行|/panel/chat-entry.md]
  Chat --> ChatRail[双侧边栏|/panel/chat-rail.md]
  Inter --> Set[设置视图|/panel/settings.md]
  Set --> SetApi[设置 · API|/panel/settings-api.md]
  Set --> SetSk[设置 · 技能|/panel/settings-skills.md]
  Set --> SetTo[设置 · 工具|/panel/settings-tools.md]
  Set --> SetPl[设置 · 模块|/panel/settings-modules.md]
  Set --> SetAg[设置 · 智能体|/panel/settings-agents.md]
  Set --> SetCo[设置 · Core 与联邦|/panel/settings-cores.md]
  Set --> SetLo[设置 · 日志与系统|/panel/settings-logs.md]
  Inter --> Tasks[实例管理·任务·Shell|/panel/instance-tasks.md]
  Inter --> Sys[系统反馈|/panel/system.md]
  Dev --> Test[测试策略|/dev/testing.md]
  Dev --> DocGuide[文档规范|/documentation-guide.md]
```

## 目录分层

- `overview/` 入口与骨架；`core/` 内核与运行时子系统；`modules/` 模块体系与各模块；`federation/` 联邦；`protocol/` 线协议
- `panel/` 前端及视图契约；`ops/` 部署运维；`dev/` 开发规范
- 根目录只保留 `index.md`（入口）与 `documentation-guide.md`（写作规范）
- 画布参数 `group` 分四个：**总览 / Core / 前端 / 工程**。`core/`、`modules/`、`federation/` 同属 `Core`——框架本身是模块化设计的（`core` 由「框架（内核）」与「模块化设计」两支组成），画布上它们是 Core 组内并列的两条分支列，不是两个并列大类。对应关系与坐标规则见 [文档规范](./documentation-guide.md)

### 文档约定（速览）

- **模块文档**：**当前实现**的权威描述，与代码同步更新
- **开发文档**（dev-*）：面向开发者的速查、规范与测试策略
- **过时内容**：不删除事实，以现状句或状态标注消化

> 原架构决策记录（ADR 0001-0018）已并入对应模块文档；历史演进由 git 提交记录承担，不在文档正文设立变更日志。
