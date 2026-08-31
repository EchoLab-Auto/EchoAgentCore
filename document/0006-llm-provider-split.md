---
id: adr-0006
title: "ADR-0006 LLM provider 拆分"
group: 架构决策
link: ["adr-index | 决策索引"]
x: 1468
y: 48
---
# ADR-0006: LLM provider 拆分(echo-llm-* Service Provider crates)

状态: accepted

## 问题

三个 LLM provider 实现(OpenAI 兼容、Anthropic Messages、Ollama)与定义层
同住在 `echo-agent::llm` 模块内。虽然类型与 `LlmProvider` trait 已在 Phase 1
迁入 `echo-defs`,但实现仍与 agent 框架同 crate——新增 provider 必须修改
agent 核心,provider 无法独立演进/发布,依赖(如 reqwest)也随 agent 打包。

## 决策

把三个 provider 拆为独立的 Service Provider crate,只依赖定义层:

- `echo-llm-openai`:OpenAI 兼容端点 provider(SSE 流式、reasoning 配置)。
- `echo-llm-anthropic`:Anthropic Messages API provider(thinking 块、
  tool_use 合并)。
- `echo-llm-ollama`:薄包装,复用 `echo-llm-openai` 指向 `{base}/v1`。

每个 crate 只依赖 `echo-defs`(消息词汇/trait/策略枚举/token 纯函数)与
传输依赖(reqwest/futures/tokio);`echo-agent` 的 `create_provider` 工厂改为
引用这些 crate,内嵌的 provider 文件删除。provider 的 schema/执行保持原样,
仅导入路径变化。

`echo-loop` 的工具执行管道同步强化为支持异步 execute(`Box::pin` future),
为后续把真实工具循环收敛到 TurnRunner 铺路。

## 备选方案

- **保持 provider 与 agent 同 crate**:新增 provider 改核心,无法独立发布。
- **全部 provider 合一个 crate**:Ollama 依赖 OpenAI 是合理的(复用),但
  OpenAI/Anthropic 变化速率不同,拆分更符合能力接缝"按变化速率拆"。

## 后果

- Provider 独立演进/测试(各 crate 自带单测:14+4+14);新增 provider =
  一个新 crate 实现 `echo_defs::LlmProvider` + 注册。
- `echo-agent` 只保留工厂与装配,不再承载 provider 实现;依赖收敛。
- 行为零变化:462 项测试全绿,`create_provider` 的路由逻辑不变。
- 后续(Phase 4 后半)把工厂字符串 match 替换为 `ctx.llm` 注册表查找,
  provider 即可运行期热替换。
