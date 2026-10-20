---
id: design-index
title: "设计记录（索引）"
group: 规划
x: 960
y: 3000
link: ["decoupling-plan | 完全解耦推进计划 | l>t", "plugin-framework-extraction | 插件框架外迁（真框架） | b>t", "instance-plugin | 实例插件化设计", "agent-humanlike-style | 拟人化方案", "federation-port-merge | 联邦·管理端口合并", "panel-performance-audit | Panel 性能审计 | r>t", "entry-identity-p1 | 条目身份化（P1） | r>b", "mcp-import | MCP 导入通道评估 | b>r", "full-plugin-ization | 全插件化（dsh 对齐） | b>t"]
---

# 设计记录（索引）

> **定位**：设计层的入口——这里收的是计划、提案与已实施的设计记录（过程性材料，每篇正文首段标注状态）。读者：要查「某个方案当初怎么定的、为什么这么定、落地到哪一步」的维护者；长期参考类文档见 [文档总览](../index.md) 的其他分层。

## 这一层收什么

- **方案与计划**：还没做、或正在做的设计（[完全解耦推进计划](./decoupling-plan.md)、[插件框架外迁](./plugin-framework-extraction.md)、[实例插件化设计](./instance-plugin.md)、[拟人化方案](./agent-humanlike-style.md)、[联邦·管理端口合并](./federation-port-merge.md)）
- **审计与实测记录**：为决策提供依据的测量（[Panel 浏览器性能审计](./panel-performance-audit.md)）、由测量结论推出的模型改造提案（[条目身份化 EntryId + rev + Upsert](./entry-identity-p1.md)）
- **选型评估**：接不接、怎么接的结论与依据（[MCP 作为工具插件导入通道](./mcp-import.md)——结论：先桥接、按需原生）
- **实施录像**：一次改造的完整设计包（[插件实例泛化设计系列](./adapter-genericization/index.md)）——分册设计、证据包、对账表与进度台账

## 写作约定

- 首段标注状态（规划中 / 已完成 / 已被取代），读者据此判断能不能照它做
- 正文允许带日期、基线提交号与行号——这类材料是**当时的快照**，不是随代码演进的参考；需要长期有效的结论请落到主题目录（`core/`、`plugins/`、`panel/` 等）
- 与实现不一致时以代码为准，并在文中补一句「现状」

## 延伸阅读

- 长期参考：[架构总览](../overview/architecture.md)、[框架（内核）](../core/frame.md)、[插件化设计](../plugins/core-plugins.md)
- 落地校验：[开发指南](../dev/dev-guide.md)、[测试策略](../dev/dev-testing.md)
