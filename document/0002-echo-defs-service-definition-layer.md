---
id: adr-0002
title: "ADR-0002 echo-defs 定义层"
group: 架构决策
x: 1898
y: 257
---
# ADR-0002: echo-defs 定义层抽取(Service Definition 层)

状态: accepted

## 问题

EchoAgentCore 的所有能力(LLM 适配、工具、技能、平台适配)都是编译期硬编码
装配:消息类型与 `LlmProvider` trait 定义在 `echo-agent::llm` 模块,provider
实现(openai/anthropic/ollama)与定义同 crate;`ToolDefinition` 定义在
`llm` 域却被 `tool` 域反向引用(接缝反了);策略枚举(`GateMode` 等)定义在
`echo-protocol`(线契约 crate),被 `echo-adapter`(自称"协议无关"的适配层)
反向依赖。新增 provider 或平台必须改核心 crate 并全量重编译。

## 决策

新建 `echo-defs` crate 作为 **Service Definition 层**(对齐 dsh 能力接缝的
定义角色),持有全部词汇类型与 trait,零实现、零 harness 依赖:

- `message` / `llm`:`ChatMessage`/`ChatRequest`/`ChatResponse`/`ToolCall`/
  `ChatChunk`/`Usage` + `LlmProvider` trait + 传输无关的 `LlmError`
  (`Http(String)`,provider 把自身传输错误映射进来,定义层不依赖 reqwest)。
- `tool`:`Tool` trait、`ToolError`、`ToolDefinition`(工具 schema 归工具域,
  `ChatRequest` 引用它——修复原 `llm` 域定义被 `tool` 反向引用的接缝)。
- `skill`:`Skill`/`SkillMetadata` + `SkillProvider` trait;文件发现/热重载
  留在 `echo-agent` 的具体 `SkillRegistry`。
- `chat`:平台无关 `ChannelType`/`IncomingMessage`/`MessageTarget`/
  `SendResult`/`AdapterEvent` + `ChatAdapter` trait(生命周期、`send_text`、
  `subscribe`、`get_groups`/`get_friend_list`)。门控词不在此层——归 Phase 4
  策略中间件。
- `mode`:`GateMode`/`ThinkingMode`/`ReasoningEffort` 从 `echo-protocol` 移
  入,`echo-protocol` re-export 保持 wire 路径;`echo-adapter` 改从
  `echo-defs` 引入并移除对 `echo-protocol` 的依赖。
- `token`:纯 token 估算/截断函数。
- `session`:`SessionEvent` + `SessionStore` trait 骨架(事件溯源会话的
  契约,Phase 3 落地实现)。

旧 crate(`echo-agent`/`echo-adapter`/`echo-protocol`)改为 re-export
`echo_defs` 类型,保持 `echo_agent::…` / `echo_adapter::…` 路径兼容。

## 备选方案

- **不抽取,维持现状**:provider 与定义同 crate,新增 provider 改核心。
- **只拆 llm 定义**:不同时处理工具/技能/平台,接缝问题残留。
- **把类型放进 echo-protocol**:线契约 crate 会承担领域词汇,违背"协议只管
  管理契约"的分层。

## 后果

- 依赖方向收敛:`echo-defs ◄ echo-protocol ◄ echo-adapter ◄ echo-agent`;扩展
  插件只依赖定义层。
- `echo-defs` 零实现约束成立(仅 serde/async-trait/tokio 基础依赖),可独立
  测试(`tests/definitions.rs` 含 token 估算与消息 roundtrip 契约)。
- `LlmError::Http` 从 `reqwest::Error` 变为 `String`:错误文本不变
  (`to_string()`),但 provider 内部需 `map_err` 显式转换(10 处)。
- Panel(依赖 `echo-protocol`)构建不受影响;`echo-protocol` 新增对
  `echo-defs` 的 path 依赖,Panel 通过 sibling 检出解析。
- 行为零变化:全量测试(402)在搬迁后保持绿,线协议 wire 格式不变。
