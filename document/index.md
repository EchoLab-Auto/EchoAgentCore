---
group: 总览
link: ["architecture | 架构总览", "core | Core 框架", "panel | Panel 前端", "protocol | 协议与数据流", "ops-deploy | 部署与自更新", "dev-guide | 开发指南", "adr-index | 架构决策"]
x: 1468
y: 384
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板** 框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 适配器；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、资源、任务、日志、适配器管理等界面。

> 本文档群按 ProDoc 规范组织（文档图模型）：每个 `.md` 为画布上一个框，`group` 围合分组、`link` 表达导航，点击框即可进入对应文档；两仓库（core + panel）的文档统一维护在本目录。

## 阅读入口

- **新人起步**：本页 → [架构总览](./architecture.md) → [协议与数据流](./protocol.md)
- **日常开发**：[Core 框架](./core.md) / [Panel 前端](./panel.md) / [开发指南](./dev-guide.md)
- **排障运维**：[部署与自更新](./ops-deploy.md)
- **设计理由**：[架构决策（ADR 0001-0017）](./adr-index.md)

## 导航地图

```prodoc-flow
graph LR
  Home[文档总览|/index.md]
  Home --> Arch[架构总览|/architecture.md]
  Home --> Core[Core 框架|/core.md]
  Home --> Panel[Panel 前端|/panel.md]
  Home --> Proto[协议与数据流|/protocol.md]
  Home --> Ops[部署与自更新|/ops-deploy.md]
  Home --> Dev[开发指南|/dev-guide.md]
  Home --> ADR[架构决策|/adr-index.md]
  Core --> Loop[Agent 循环与工具|/core-agent-loop.md]
  Core --> Agents[多 Agent 与会话|/core-agents.md]
  Core --> Plugins[插件系统|/core-plugins.md]
  Core --> Tasks[后台任务|/core-background-tasks.md]
  Core --> Persist[配置持久化|/core-config-persistence.md]
  Core --> Gating[QQ 门控|/adapter-qq-gating.md]
  Proto --> Core
  Panel --> Proto
  Ops --> Drain[ADR-0015 优雅排空|/0015-graceful-drain.md]
```

## 文档分层约定

- **模块文档**（core-* / panel / protocol / ops-deploy）：**当前实现**的权威描述，与代码同步更新
- **架构决策**（0001-0017）：决策当时的历史记录，只增不改；与模块文档冲突时以模块文档为准
- **开发指南**（dev-*）：面向开发者的速查与测试策略
