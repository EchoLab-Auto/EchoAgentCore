//! # echo-plugin-api — 插件协议（冻结契约）
//!
//! P0 阶段产物（见 `document/decoupling-plan.md`）。本 crate 是宿主（kernel）
//! 与插件之间的**唯一线上契约**：同一组消息覆盖全部运输层——
//!
//! - **inproc**：类型直连（零序列化，插件与宿主同进程编译）；
//! - **subprocess**：4 字节 LE 长度前缀 + JSON（默认）/ msgpack（feature），
//!   经 `tokio-util::LengthDelimitedCodec` 编解码；
//! - dylib / wasm（实验/可选轨）：JSON 载荷。
//!
//! ## 兼容性规则（对齐 `echo-federation::FedFrame` 既有惯例）
//!
//! - adjacently-tagged enum（`{"type": …, "payload": …}`）；变体名与字段名是
//!   线上契约，**只增不改**；
//! - 新增字段一律 `#[serde(default)]`（旧端点可解码新帧、新端点可解码旧帧）；
//! - Hello/Welcome 握手时比对 [`PROTOCOL_NAME`] 与 [`PROTOCOL_VERSION`]：
//!   主版本不一致拒绝连接；
//! - 协议变更必须过 conformance 测试套件（`echo-plugin-host/tests/conformance.rs`）
//!   并按需 bump `PROTOCOL_VERSION`。

pub mod contribution;
pub mod protocol;

pub use contribution::{
    Contribution, EventContribution, ServiceContribution, SkillContribution, ToolContribution,
};
pub use protocol::{
    compatible, Cancel, Drain, Emit, EventNotification, Failure, Hello, HostToPlugin, Invoke,
    InvokeContext, InvokeOutcome, LogLevel, LogRecord, PluginToHost, Register, Welcome,
    PROTOCOL_NAME, PROTOCOL_VERSION,
};

#[cfg(test)]
mod tests;
