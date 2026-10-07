---
id: index
title: "EchoAgent 文档总览"
group: 总览
x: 1520
y: -400
link: ["architecture | 架构总览 | l>r", "core | Core 后端 | l>r", "panel | Panel 前端 | r>l", "protocol | 协议与数据流 | r>l", "ops-deploy | 部署与自更新 | r>l", "dev-guide | 开发指南 | r>l"]
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板**框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 适配器；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、任务、Shell、设置（API/技能/工具/插件/智能体/日志）等界面。

> 本文档群按 ProDoc 文档图组织（每个 `.md` 是画布上一个框，连线表达主干导航），并按「总览 → 框架 → 子系统 → 细节」四层拉开阅读距离——分层定义与写作约定见 [文档规范](./documentation-guide.md)。两仓库（core + panel）的文档统一维护在本目录。

## 阅读路径

- **理解系统怎么组成**：本页 → [架构总览](./architecture.md) → [Core 后端](./core.md) → [框架（内核）](./frame.md) → [插件化设计](./core-plugins.md)
- **运行时子系统**：[多 Agent 与会话](./core-agents.md) → [Agent 循环](./core-agent-loop.md)；[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)
- **扩展系统**：[插件化设计](./core-plugins.md) → [工具系统](./core-tools.md) / [技能系统](./core-skills.md) / [Subagent 插件](./core-subagent.md)；外部插件作者读 [插件开发指南](./plugin-authoring.md)
- **前端**：[Panel 前端](./panel.md) → [交互定义](./panel-interaction.md) → 各视图文档
- **运维排障**：[部署与自更新](./ops-deploy.md)、[协议与数据流](./protocol.md)

## 导航地图

```prodoc-flow
graph LR
  Home[文档总览|/index.md]
  Home --> Arch[架构总览|/architecture.md]
  Home --> Core[Core 后端|/core.md]
  Home --> Panel[Panel 前端|/panel.md]
  Home --> Proto[协议与数据流|/protocol.md]
  Home --> Ops[部署与自更新|/ops-deploy.md]
  Home --> Dev[开发指南|/dev-guide.md]
  Core --> Frame[框架（内核）|/frame.md]
  Core --> Plugins[插件化设计|/core-plugins.md]
  Frame --> Agents[多 Agent 与会话|/core-agents.md]
  Agents --> Loop[Agent 循环|/core-agent-loop.md]
  Frame --> Memory[会话记忆|/core-memory.md]
  Frame --> MModal[多模态输入|/core-multimodal.md]
  Frame --> Persist[配置持久化|/core-config-persistence.md]
  Plugins --> Tools[工具系统|/core-tools.md]
  Plugins --> Skills[技能系统|/core-skills.md]
  Plugins --> Subagent[Subagent 插件|/core-subagent.md]
  Plugins --> Gating[QQ 适配器门控|/adapter-qq-gating.md]
  Plugins --> Fed[联邦（多机）|/federation.md]
  Plugins --> Author[插件开发指南|/plugin-authoring.md]
  Plugins --> Plan[完全解耦推进计划|/decoupling-plan.md]
  Agents --> Style[拟人化方案（规划）|/agent-humanlike-style.md]
  Panel --> Inter[Panel 交互定义|/panel-interaction.md]
  Inter --> Layout[布局与导航|/panel-layout.md]
  Inter --> Chat[会话视图|/panel-chat.md]
  Inter --> Modals[模态与覆盖层|/panel-modals.md]
  Inter --> Set[设置视图|/panel-settings.md]
  Inter --> Tasks[QQ管理·任务·Shell|/panel-qq-tasks.md]
  Inter --> Sys[系统交互|/panel-system.md]
  Dev --> Test[测试策略|/dev-testing.md]
  Dev --> DocGuide[文档规范|/documentation-guide.md]
```

## 文档分类

画布分为六组，阅读距离从左到右（框架 → 细节）：

- **总览**——入口与骨架：本页、[架构总览](./architecture.md)
- **框架**——内核机制与运行时子系统：[Core 后端](./core.md)、[框架（内核）](./frame.md)、[多 Agent 与会话](./core-agents.md)、[Agent 循环](./core-agent-loop.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)
- **插件**——插件体系与各插件机制文档：[插件化设计](./core-plugins.md)、[工具系统](./core-tools.md)、[技能系统](./core-skills.md)、[Subagent 插件](./core-subagent.md)、[QQ 适配器门控](./adapter-qq-gating.md)、[联邦（多机）](./federation.md)、[插件开发指南](./plugin-authoring.md)
- **前端**——Panel 与各视图交互契约：`panel.md`、`panel-interaction.md` 及各 `panel-*` 视图文档
- **工程**——协议、运维与开发规范：[协议与数据流](./protocol.md)、[部署与自更新](./ops-deploy.md)、[开发指南](./dev-guide.md)、[测试策略](./dev-testing.md)、[文档规范](./documentation-guide.md)
- **规划**——路线图与提案：[完全解耦推进计划](./decoupling-plan.md)、[拟人化方案](./agent-humanlike-style.md)

### 文档约定（速览）

- **模块文档**：**当前实现**的权威描述，与代码同步更新
- **开发文档**（dev-*）：面向开发者的速查、规范与测试策略
- **规划文档**：路线图与未落地提案，正文明确标注状态

> 原架构决策记录（ADR 0001-0018）已并入对应模块文档；历史演进由 git 提交记录承担，不在文档正文设立变更日志。
