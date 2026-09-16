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
- **日常开发**：[Core 框架](./core.md) / [Panel 前端](./panel.md) / [开发指南](./dev-guide.md)
- **排障运维**：[部署与自更新](./ops-deploy.md)

## 导航地图

```prodoc-flow
graph LR
  Home[文档总览|/index.md]
  Home --> Arch[架构总览|/architecture.md]
  Home --> Core[Core 框架|/core.md]
  Home --> Panel[Panel 前端|/panel.md]
  Panel --> Interaction[交互定义|/panel-interaction.md]
  Home --> Proto[协议与数据流|/protocol.md]
  Home --> Ops[部署与自更新|/ops-deploy.md]
  Home --> Dev[开发指南|/dev-guide.md]
  Core --> Loop[Agent 循环|/core-agent-loop.md]
  Core --> Tools[工具系统|/core-tools.md]
  Core --> Skills[技能系统|/core-skills.md]
  Core --> Agents[多 Agent 与会话|/core-agents.md]
  Core --> Plugins[插件化设计|/core-plugins.md]
  Core --> Persist[配置持久化|/core-config-persistence.md]
  Plugins --> Gating[QQ 门控|/adapter-qq-gating.md]
  Proto --> Core
  Panel --> Proto
```

## 文档分层约定

- **模块文档**（core-* / panel / protocol / ops-deploy）：**当前实现**的权威描述，与代码同步更新
- **开发指南**（dev-*）：面向开发者的速查与测试策略

> 原架构决策记录（ADR 0001-0018）已于 2026-09 全部并入对应模块文档，不再单独立卷；设计理由随模块文档维护。
