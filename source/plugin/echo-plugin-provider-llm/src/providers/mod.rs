//! 各 LLM 协议的客户端实现（OpenAI 兼容 / Anthropic Messages / Ollama）。
//!
//! ## 为什么它们是本 crate 的私有模块（2026-11）
//!
//! 这三份实现此前是**三个独立 crate**（`echo-llm-openai` / `-anthropic` /
//! `-ollama`），躺在 `source/llm/` 下并被 `deps-policy.txt` 归入「中间层」。
//! 但「中间层」的定义是**被多方复用的底座**，而它们各自只有
//! `echo-plugin-provider-llm` 一个消费者：
//!
//! ```text
//! grep -rl 'echo-llm-openai'    source/*/*/Cargo.toml
//!   → llm/echo-llm-openai（自身）、llm/echo-llm-ollama、plugin/echo-plugin-provider-llm
//! grep -rl 'echo-llm-anthropic' source/*/*/Cargo.toml
//!   → llm/echo-llm-anthropic（自身）、plugin/echo-plugin-provider-llm
//! ```
//!
//! 仓外同样是零引用（`echo-adapter-qq` / `plugin-framework` /
//! `echo-hpc-platform` 都不依赖它们）。也就是说：**三个 crate 边界换来的不是
//! "可替换点"，只是目录层级** —— 协议差异靠的是 `LlmProvider` trait（在
//! `echo-defs`），不是这三个 crate 之间的抽象。
//!
//! 合并后这条线就干净了：
//!
//! ```text
//! echo-defs::llm::LlmProvider        契约（谁都能实现）
//!         ▲
//! echo-plugin-provider-llm           实现（OpenAI / Anthropic / Ollama 客户端）
//!         ▲
//! echo-agent::llm::create_provider   宿主 glue（配置投影 + 指标出口）
//! ```
//!
//! ## 判据（给下一个"该不该独立成 crate"的决定用）
//!
//! | 消费者数 | 结论 |
//! |---|---|
//! | 1 | 它是那个消费者的**实现细节** → 合并进来 |
//! | > 1 | 它是**共用底座** → 独立成 crate（如 `echo-sanitize`：被 `echo-agent` / `echo-tool` / 插件三处用） |
//!
//! ## 三者的关系
//!
//! `ollama` **复用** `openai`：Ollama 暴露 OpenAI 兼容的 `/v1` 端点，所以它
//! 是一个 82 行的薄包装（把 base_url 归一到 `/v1` 后整体委托）。这条边原来是
//! `echo-llm-ollama → echo-llm-openai` 的 Cargo 依赖，现在是同一 crate 内的
//! `super::openai`。

pub mod anthropic;
pub mod ollama;
pub mod openai;
