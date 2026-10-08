---
id: core
title: "Core 后端"
group: 框架
x: 320
y: 820
link: ["frame | 框架（内核） | r>l", "plugins | 插件化设计 | b>t"]
---

# Core 框架

> **定位**：本文是 **Core 后端**的入口页——Core 是什么、由哪两支组成、进程与运行速览、配置从哪读。读者：初次接触 Core 的开发者。
> 组成细节：[框架（内核）](./frame.md)、[插件化设计](./core-plugins.md)；运维见 [部署与自更新](./ops-deploy.md)，配置机制见 [配置持久化](./core-config-persistence.md)。

## Core 的组成

Core（EchoAgentCore）是 Agent 后端核心服务，Rust 实现。组合根在 `source/core/src/main.rs`；核心库为 `source/backend/echo-agent`（框架核心在 `agent/`，各插件实现按包分目录在 `packages/`），协议定义在 `source/protocol/echo-protocol`。

Core 由两支组成：

- [框架（内核）](./frame.md)：让一切插件都能挂上去的最小机制集——组合与装配、服务定位、事件总线、插件宿主、进程结构；运行时子系统见 [多 Agent 与会话](./core-agents.md)、[Agent 循环](./core-agent-loop.md)、[会话记忆](./core-memory.md)、[多模态输入](./core-multimodal.md)、[配置持久化](./core-config-persistence.md)
- [插件化设计](./core-plugins.md)：全部能力以插件装载——内置插件清单、能力开关、外部进程插件、Package 门控

## 进程与运行速览

- 由 systemd 用户服务运行；Cargo 二进制名为 `echo-agent-core`（`source/core/Cargo.toml`），`install.sh` 安装后落盘为 `$LIBEXEC_DIR/echo-agent-core-bin`（libexec 文件名，非 Cargo bin 名）
- 组合根负责：加载配置、构建 LLM provider、装配工具/技能/插件、创建多 agent 监督器、启动 QQ 适配器与 management WS
- Panel 经 management WebSocket 与 Core 通信——所有人格事件直投进程级汇聚点（`EventSink`），单连接即可看到全部活动；运维命令与服务管理见 [部署与自更新](./ops-deploy.md)

## 配置入口

配置为 `~/.config/echo-agent-core/core.toml`，主要分段：`[agent]`（provider/model 与 api profile 池、输出与记忆预算、工具超时、skills_dir）、`[agent.teams.*]`（人格定义与能力白名单）、`[adapters.qq]`、`[plugins.system_prompt]` 等。持久化机制与 provider 取值见 [配置持久化](./core-config-persistence.md)。
