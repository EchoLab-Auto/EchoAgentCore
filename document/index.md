---
group: 总览
x: -53
y: 637
link: ["architecture | 架构总览", "core | Core 框架 | r>l", "panel | Panel 前端 | r>l", "protocol | 协议与数据流", "ops-deploy | 部署与自更新", "dev-guide | 开发指南"]
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板** 框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 适配器；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、任务、Shell、设置（API/技能/工具/插件/智能体/日志）等界面。

> 本文档群按 ProDoc 规范组织（文档图模型）：每个 `.md` 为画布上一个框，`group` 围合分组、`link` 表达导航，点击框即可进入对应文档；两仓库（core + panel）的文档统一维护在本目录。

## 阅读入口

- **新人起步**：本页 → [架构总览](./architecture.md) → [协议与数据流](./protocol.md)
- **Agent 子系统**：[Agent 循环](./core-agent-loop.md) → [多 Agent 与会话](./core-agents.md) → [会话记忆](./core-memory.md) / [多模态输入](./core-multimodal.md)
- **日常开发**：[Core 框架](./core.md) / [Panel 前端](./panel.md) / [开发指南](./dev-guide.md)
- **排障运维**：[部署与自更新](./ops-deploy.md)

## 导航地图

```prodoc-flow
graph LR
  Home[文档总览|/index.md]
  Home --> Arch[架构总览|/architecture.md]
  Home --> Core[Core 框架|/core.md]
  Core --> Agents[多 Agent 与会话|/core-agents.md]
  Core --> Loop[Agent 循环|/core-agent-loop.md]
  Core --> Memory[会话记忆|/core-memory.md]
  Core --> Multimodal[多模态输入|/core-multimodal.md]
  Core --> Plugins[插件化设计|/core-plugins.md]
  Core --> Persist[配置持久化|/core-config-persistence.md]
  Plugins --> Tools[工具系统|/core-tools.md]
  Plugins --> Skills[技能系统|/core-skills.md]
  Plugins --> Subagent[Subagent 插件|/core-subagent.md]
  Plugins --> Gating[QQ 门控|/adapter-qq-gating.md]
  Arch --> Federation[联邦（多机）|/federation.md]
  Home --> Panel[Panel 前端|/panel.md]
  Panel --> Interaction[交互定义|/panel-interaction.md]
  Interaction --> Layout[布局与导航|/panel-layout.md]
  Interaction --> Chat[会话视图|/panel-chat.md]
  Interaction --> Modals[模态与覆盖层|/panel-modals.md]
  Interaction --> Set[设置视图|/panel-settings.md]
  Interaction --> QQTasks[QQ管理·任务·Shell|/panel-qq-tasks.md]
  Interaction --> Sys[系统交互|/panel-system.md]
  Home --> Proto[协议与数据流|/protocol.md]
  Proto --> Core
  Panel --> Proto
  Home --> Ops[部署与自更新|/ops-deploy.md]
  Home --> Dev[开发指南|/dev-guide.md]
  Dev --> Testing[测试策略|/dev-testing.md]
```

## 文档分类

Core 框架文档按**子系统域**分四组（与画布分组一致）：

- **Agent 运行时**——agent 本身如何运转：[多 Agent 与会话](./core-agents.md) / [Agent 循环](./core-agent-loop.md) / [会话记忆](./core-memory.md) / [多模态输入](./core-multimodal.md)
- **扩展系统**——如何扩展 agent 能力：[插件化设计](./core-plugins.md) / [工具系统](./core-tools.md) / [技能系统](./core-skills.md) / [Subagent 插件](./core-subagent.md)
- **核心设施**——进程级基础：[Core 框架](./core.md) / [配置持久化](./core-config-persistence.md)
- **集成与适配**——对外连接：[QQ 适配器门控](./adapter-qq-gating.md) / [联邦（多机）](./federation.md)

其余分组：**前端**（panel-*）、**协议**（protocol）、**运维**（ops-deploy）、**开发指南**（dev-*）。

- **模块文档**（core-* / panel / protocol / ops-deploy）：**当前实现**的权威描述，与代码同步更新
- **开发指南**（dev-*）：面向开发者的速查与测试策略

> 原架构决策记录（ADR 0001-0018）已于 2026-09 全部并入对应模块文档，不再单独立卷；设计理由随模块文档维护。
