---
id: adr-0016
title: "ADR-0016 Web 面板无状态中继"
group: 架构决策
x: 2360
y: 1392
---
# ADR-0016: Web 面板 = 无状态字节级 WS 中继 + 静态托管

状态: accepted

> 原 echo-agent-panel 仓库 `doc/decisions/0001-web-relay-architecture.md`，
> 文档群合并时重编号为 0016（决策内容未改动）。

## 问题

TUI 迁移到 EchoAgentTui 后，EchoAgentPanel 需要重做为 Web 面板。前端必须
对接 Core 的 management WS 协议（命令 / 事件 / sudo 密码帧），并且不能引入
任何新的安全弱点——尤其是 sudo 密码帧。

## 决策

后端（`source/echo-web-server`）是**无状态字节级中继**：

- 每个浏览器 WS 连接对应一条到 Core management WS 的专用连接；
- 帧按原文转发，**不解析、不记录负载**（sudo 密码帧因此与日志、命令队列
  完全隔离，与 TUI 的专用通道语义一致）；
- HTTP 侧只做静态托管（构建后的 `web/dist`）与 `/ws` 升级；
- 前端是纯 WS 客户端（React + TS + Vite），协议类型镜像在
  `web/src/protocol.ts`。

## 备选方案

- **后端做协议层转发 / 会话管理**：后端需要维护完整状态（会话、分支、
  配置），等于重写 Core 的桥接层，且 sudo 密码帧需要额外的安全处理——
  否决。
- **前端直连 Core :3132**：跨域 + 暴露 Core 的 management 端口给浏览器，
  且无法注入 sudo 通道的安全处理——否决。

## 后果

- 协议演进只需同步 `EchoAgentCore/source/protocol/echo-protocol` 与
  `web/src/protocol.ts`，后端零改动；
- 后端极薄（一个 crate、约 200 行），可独立测试（`tests/proxy.rs` 用假
  Core 验证双向帧透传，含 sudo 帧）；
- 多标签页 = 多条 Core 连接（Core 的 management 已支持多 Panel）；
- 已知限制：无会话恢复（刷新页面从 `RequestState` / `RequestTrunkTimeline`
  重新拉取）。
