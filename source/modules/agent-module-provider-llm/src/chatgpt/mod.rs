//! ChatGPT 订阅通道（OAuth 登录 + Responses API）。
//!
//! 两个子模块的分工：
//!
//! - [`auth`]：拿令牌（PKCE 登录 / 刷新 / 落盘）。**不知道**请求长什么样。
//! - [`responses`]：用令牌发请求（Responses API 客户端）。**不知道**令牌
//!   从哪来、怎么续。
//!
//! 这条边界是刻意的：端点/参数会变，令牌管理也会变，但两者的变化不同步。
//! 混在一个模块里会让"改登录"和"改请求"互相牵连。
//!
//! # 使用方式
//!
//! 1. `echo-agent-core login-chatgpt` 登录一次（令牌存
//!    `~/.local/share/echo-agent-core/chatgpt-auth.json`，0600）
//! 2. 在 API 配置里加一个 `provider = "openai-chatgpt"` 的 profile，
//!    `model` 填 Codex 可用模型（如 `gpt-5-codex`），`api_key` **留空**
//!
//! profile 的 `api_key` 留空不是"忘了填"：这个 provider 的凭据是登录态，
//! 不是密钥。填了也不会被用（见 `responses::ChatGptProvider`）。

pub mod auth;
pub mod responses;

pub use auth::{ChatGptAuth, PendingLogin};
pub use responses::ChatGptProvider;
