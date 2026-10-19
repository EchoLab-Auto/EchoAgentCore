---
id: index
title: "EchoAgent 文档总览"
group: 总览
x: 1520
y: 100
link: ["architecture | 架构总览", "core | Core 后端 | b>l", "panel | Panel 前端", "protocol | 协议与数据流", "ops-deploy | 部署与自更新 | r>t", "dev-guide | 开发指南 | b>l", "design-index | 设计记录"]
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板**框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 插件实例；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、任务、Shell、设置（API/技能/工具/插件/智能体/日志）等界面。

> 本文档群按 ProDoc 文档图组织（每个 `.md` 是画布上一个框，连线表达主干导航），物理目录按「主题 × 阅读距离」分层（`overview → core → plugins / panel / protocol / federation → ops / dev → design`），且将**长期参考类**（模块文档）与**设计过程类**（计划/提案/记录）分开——放置规则与写作约定见 [文档规范](./documentation-guide.md)。两仓库（core + panel）的文档统一维护在本目录。

## 阅读路径

- **理解系统怎么组成**：本页 → [架构总览](./overview/architecture.md) → [Core 后端](./core/core.md) → [框架（内核）](./core/frame.md) → [插件化设计](./plugins/core-plugins.md)
- **运行时子系统**：[多 Agent 与会话](./core/core-agents.md) → [Agent 循环](./core/core-agent-loop.md)；[会话记忆](./core/core-memory.md)、[多模态输入](./core/core-multimodal.md)、[配置持久化](./core/core-config-persistence.md)
- **扩展系统**：[插件化设计](./plugins/core-plugins.md) → [工具系统](./plugins/core-tools.md) / [技能系统](./plugins/core-skills.md) / [Subagent 插件](./plugins/core-subagent.md)；外部插件作者读 [插件开发指南](./plugins/plugin-authoring.md)
- **安全**：[敏感信息隔离（脱敏服务）](./core/security-redaction-design.md)——出口卡口与保证边界
- **前端**：[Panel 前端](./panel/panel.md) → [交互定义](./panel/panel-interaction.md) → 各视图文档
- **运维排障**：[部署与自更新](./ops/ops-deploy.md)、[协议与数据流](./protocol/protocol.md)

## 分层导航

**总览层 —— 入口与骨架**

- [architecture.md](./overview/architecture.md) — 架构总览：crate 布局、能力接缝、事件、会话与扩展点地图
- [index.md](./index.md) — 本页：文档总览、阅读路径与分层导航

**核心层 —— 内核机制与运行时子系统**

- [core.md](./core/core.md) — Core 后端入口：组成、进程与运行速览、配置入口
- [frame.md](./core/frame.md) — 框架（内核）：组合装配、服务定位、事件总线、插件宿主、进程结构
- [core-agents.md](./core/core-agents.md) — 多 Agent 与会话：persona 装配、会话模型、工作区会话、能力隔离
- [core-agent-loop.md](./core/core-agent-loop.md) — Agent 循环：单/并行模式、Turn 状态机与默认驱动 TurnRunner
- [core-memory.md](./core/core-memory.md) — 会话记忆：事件溯源日志、可见性边界、投影与切片、压缩归档、显示时间线
- [core-multimodal.md](./core/core-multimodal.md) — 多模态输入：媒体库、模型侧还原、token 卫生
- [core-config-persistence.md](./core/core-config-persistence.md) — 配置持久化：ConfigStore、门控/会话持久化、原子写入
- [security-redaction-design.md](./core/security-redaction-design.md) — 敏感信息隔离：威胁模型、出口卡口与保证边界

**插件层 —— 插件体系与各插件机制**

- [core-plugins.md](./plugins/core-plugins.md) — 插件化设计：插件模型、内置清单、能力开关、外部进程插件
- [core-tools.md](./plugins/core-tools.md) — 工具系统：注册分发、路径约定、超时治理、后台 Shell 会话
- [core-skills.md](./plugins/core-skills.md) — 技能系统：分层、SKILL.md、热重载、系统提示词技能
- [core-subagent.md](./plugins/core-subagent.md) — Subagent 插件：异步委派、隔离上下文、完成回报
- [qq-gating.md](./plugins/qq-gating.md) — QQ 实例门控：五层管道、多实例、消息收发与渲染
- [plugin-authoring.md](./plugins/plugin-authoring.md) — 插件开发指南：快速上手、装载、协议约定与本地验证
- [federation.md](./federation/federation.md) — 联邦（多机去中心化）：链路协议、安全模型、远程工具与调度

**协议层**

- [protocol.md](./protocol/protocol.md) — 协议与数据流：传输信封、命令/事件、事件流连续性、时间线同步、兼容性规则

**前端层 —— Panel 与视图交互契约**

- [panel.md](./panel/panel.md) — Panel 前端：仓库布局、中继模型、状态管理与数据流
- [panel-interaction.md](./panel/panel-interaction.md) — **交互定义**（设备无关）：核心原则、视图层级、导航契约、功能模块清单（前端总入口）
- [panel-desktop-layout.md](./panel/panel-desktop-layout.md) — **电脑端布局**（≥768px）：外壳几何、层叠秩序、双侧边栏、卡片、连接生命周期
- [panel-chat.md](./panel/panel-chat.md) — 会话视图：消息列表、工具卡、推理块与输入区
- [panel-modals.md](./panel/panel-modals.md) — 模态与覆盖层：模态清单、分支/上下文弹层、Agent 配置弹层
- [panel-settings.md](./panel/panel-settings.md) — 设置视图：API、资源工作区、技能安装、Core 连接与联邦、日志
- [panel-instance-tasks.md](./panel/panel-instance-tasks.md) — 实例管理·任务·Shell：描述符驱动的插件实例弹层、任务弹层、HTTP 通道
- [panel-mobile-layout.md](./panel/panel-mobile-layout.md) — **移动端布局**（<768px）：顶栏与视图菜单、入口行横滚提示、卡片 Sheet、软键盘与安全区
- [panel-system.md](./panel/panel-system.md) — 系统交互：Toast、键盘清单、设计边界与关键常量
- [panel-icons.md](./panel/panel-icons.md) — 图标体系：载体分工、尺寸线宽档位、语义色令牌、防回归守卫

**运维层**

- [ops-deploy.md](./ops/ops-deploy.md) — 部署与自更新：安装、容器化、更新流程、回滚与日常体检

**开发层**

- [dev-guide.md](./dev/dev-guide.md) — 开发指南：仓库结构、关键抽象、编码约定、治理门禁
- [dev-testing.md](./dev/dev-testing.md) — 测试策略：测试分层、Mock 与夹具、质量门禁
- [documentation-guide.md](./documentation-guide.md) — 文档规范：目录分层、单篇体例、模板与放置规则

**设计层 —— 计划 / 提案 / 设计记录（过程性材料，正文标注状态）**

- [design/index.md](./design/index.md) — 设计层索引：这一层收什么、写作约定与阅读顺序
- [decoupling-plan.md](./design/decoupling-plan.md) — 完全解耦推进计划（规划）：验收口径、分阶段计划与风险
- [instance-plugin.md](./design/instance-plugin.md) — 实例插件化设计：P1 已落地、P2+ 规划
- [adapter-genericization/index.md](./design/adapter-genericization/index.md) — 插件实例泛化设计系列：分册设计（01/02/03）、证据包、对账表与进度台账
- [agent-humanlike-style.md](./design/agent-humanlike-style.md) — 拟人化方案（规划）：语气与回复节奏
- [federation-port-merge.md](./design/federation-port-merge.md) — 联邦·管理端口合并（设计记录，已实施）
- [panel-performance-audit.md](./design/panel-performance-audit.md) — Panel 浏览器性能审计：测量方法与修复前后实测

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
  Home --> Ops[部署与自更新|/ops/ops-deploy.md]
  Home --> Dev[开发指南|/dev/dev-guide.md]
  Core --> Frame[框架（内核）|/core/frame.md]
  Core --> Plugins[插件化设计|/plugins/core-plugins.md]
  Frame --> Agents[多 Agent 与会话|/core/core-agents.md]
  Agents --> Loop[Agent 循环|/core/core-agent-loop.md]
  Frame --> Memory[会话记忆|/core/core-memory.md]
  Frame --> MModal[多模态输入|/core/core-multimodal.md]
  Frame --> Persist[配置持久化|/core/core-config-persistence.md]
  Frame --> Security[敏感信息隔离|/core/security-redaction-design.md]
  Plugins --> Tools[工具系统|/plugins/core-tools.md]
  Plugins --> Skills[技能系统|/plugins/core-skills.md]
  Plugins --> Subagent[Subagent 插件|/plugins/core-subagent.md]
  Plugins --> Gating[QQ 实例门控|/plugins/qq-gating.md]
  Plugins --> Fed[联邦（多机）|/federation/federation.md]
  Plugins --> Author[插件开发指南|/plugins/plugin-authoring.md]
  Home --> Design[设计记录|/design/index.md]
  Design --> Plan[完全解耦推进计划|/design/decoupling-plan.md]
  Design --> AdapterPlugin[实例插件化设计|/design/instance-plugin.md]
  Design --> Style[拟人化方案|/design/agent-humanlike-style.md]
  Design --> FedMerge[联邦·管理端口合并|/design/federation-port-merge.md]
  Design --> Perf[Panel 性能审计|/design/panel-performance-audit.md]
  AdapterPlugin --> Gen[插件实例泛化（设计系列）|/design/adapter-genericization/index.md]
  Panel --> Inter[Panel 交互定义|/panel/panel-interaction.md]
  Inter --> Desk[电脑端布局|/panel/panel-desktop-layout.md]
  Inter --> Chat[会话视图|/panel/panel-chat.md]
  Inter --> Modals[模态与覆盖层|/panel/panel-modals.md]
  Inter --> Set[设置视图|/panel/panel-settings.md]
  Inter --> Tasks[实例管理·任务·Shell|/panel/panel-instance-tasks.md]
  Inter --> Mobile[移动端布局|/panel/panel-mobile-layout.md]
  Inter --> Sys[系统交互|/panel/panel-system.md]
  Inter --> Icons[图标体系|/panel/panel-icons.md]
  Dev --> Test[测试策略|/dev/dev-testing.md]
  Dev --> DocGuide[文档规范|/documentation-guide.md]
```

## 目录分层

物理目录将**长期参考类**与**设计过程类**分开：

- `overview/` 入口与骨架；`core/` 内核与运行时子系统；`plugins/` 插件体系与各插件；`federation/` 联邦；`protocol/` 线协议
- `panel/` 前端及视图契约；`ops/` 部署运维；`dev/` 开发规范；`design/` 计划、提案与设计记录（已实施的设计记录亦在此，正文已标注状态）
- 根目录只保留 `index.md`（入口）与 `documentation-guide.md`（写作规范）
- 画布参数 `group` 暂保留原分组（总览/框架/插件/前端/工程/规划），与目录对应关系见 [文档规范](./documentation-guide.md)

### 文档约定（速览）

- **模块文档**：**当前实现**的权威描述，与代码同步更新
- **开发文档**（dev-*）：面向开发者的速查、规范与测试策略
- **设计文档**（design/）：路线图、提案与设计记录，正文首段标注状态；已实施记录不删，供追溯
- **过时内容**：不删除事实，以现状句或状态标注消化

> 原架构决策记录（ADR 0001-0018）已并入对应模块文档；历史演进由 git 提交记录承担，不在文档正文设立变更日志。
