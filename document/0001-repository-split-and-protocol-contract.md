---
id: adr-0001
title: "ADR-0001 仓库拆分与协议契约"
group: 架构决策
x: 2076
y: 216
---
# ADR-0001: 仓库拆分与 echo-protocol 跨仓库契约

状态: accepted

## 问题

EchoAgentCore(后端 agent 服务)与 EchoAgentPanel(TUI 前端)最初是单一仓库。
前端与后端的线协议(`BackendCommand` / `BackendEvent`)由后端 crate 定义,
前端以相对路径依赖引入。这个结构带来两个问题:

1. **前端无法独立演进**:前端每次构建都依赖后端仓库的完整检出,后端仓库的任何
   变更都会进入前端构建,跨仓库耦合无法控制。
2. **协议定义位置不清晰**:协议类型散落在后端 crate 中,前端直接依赖后端实现,
   违反"线契约应独立于任何实现"的原则。

## 决策

将仓库拆分为 `EchoAgentCore` 与 `EchoAgentPanel` 两个独立仓库,并新建
`echo-protocol` crate 作为**前端 ⇄ Core 线契约的唯一来源**:

- `echo-protocol` 只定义 `BackendCommand` / `BackendEvent` / `WsMessage` /
  bridge / 共享枚举(`GateMode` / `ThinkingMode` / `ReasoningEffort`),不依赖
  任何 agent、平台或 UI 代码;serde 表示即线格式。
- 前端仅依赖 `echo-protocol` 中的类型,是纯粹的"协议客户端",不包含任何
  agent 或 QQ 逻辑。
- 后端各 crate 通过 re-export 保持 `echo_agent::…` 路径兼容。
- 线格式向后兼容规则:新增字段必须 `#[serde(default)]`,旧 peer 必须能解码
  新帧;变体/字段名保持稳定。

## 备选方案

- **保持单体仓库**:不解决前端独立构建问题,跨仓库耦合持续存在。
- **前端直接依赖后端 crate**:使前端引入后端实现细节,违背分层。
- **自建 wire 类型(前端复制一份)**:双份类型必然漂移,违背单一事实来源。

## 后果

- 前端与后端可以独立发布、独立演进,仅通过 `echo-protocol` 契约耦合。
- Panel 仓库以相对路径依赖 `echo-protocol`(要求两个仓库并排克隆),或改指
  git 依赖;CI 必须克隆 EchoAgentCore 作为 sibling 检出(跨仓库版本漂移风险,
  见 Phase 6 钉版计划)。
- 协议演进必须两端同步:新增字段向后兼容;语义变更需双端协同合入。
- 后端重构(本仓库的 dsh 化改造)在保持 `echo-protocol` wire 格式稳定期间,
  可以独立进行内部拆分而不破坏前端。
