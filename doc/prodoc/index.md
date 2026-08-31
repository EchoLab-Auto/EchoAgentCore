---
id: index
title: "EchoAgent 文档总览"
order: 0
x: 40
y: 40
group: 总览
link: ["core | Core 框架", "panel | Panel 前端", "protocol | 协议与数据流", "agents | 多 Agent 与会话", "plugins | 插件系统", "deploy | 部署与自更新"]
---

# EchoAgent 文档总览

EchoAgent 是运行在本机的 **Agent 核心服务 + Web 管理面板** 框架：Core 负责 LLM agent 循环、工具调用、多 agent 人格、QQ 适配器；Panel 是 Vue 3 前端，通过 WebSocket 与 Core 通信，提供聊天、资源、任务、日志、适配器管理等界面。

> 本文档群按 ProDoc 规范组织：每个 `.md` 为图上一个框，`link` 表达导航关系，点击画布上的框即可进入对应文档。

## 阅读入口

- **新人起步**：先读本页 → 协议与数据流，再读 Core 框架
- **日常开发**：Core 框架 / Panel 前端 按需查阅
- **排障运维**：直接读 部署与自更新 与 插件系统

## 文档图导航

画布上每个框代表一份文档，框间箭头为导航链路：

- **总览**：本页（入口）
- **架构**：Core 框架、Panel 前端、协议与数据流
- **机制**：多 Agent 与会话、插件系统
- **运维**：部署与自更新

## 与其他文档的关系

仓库原有 `doc/architecture.md`、`doc/decisions/*` 为重构期设计记录；本页所述内容为**当前实现**的权威描述，两者如有出入以本页为准。
