//! 插件实例（adapter）的**入站桥**（泛化 S5，2026-10）。
//!
//! ## 这里只剩入站桥，实例命令域已迁走（全插件化 S3）
//!
//! 此前本目录有三样东西：
//!
//! | 原来 | 现在 |
//! |---|---|
//! | `commands`：`InvokeInstanceOp` 分发 / 管理通道 / 缺省实例回退 / 写方法广播（429 行） | **插件** `echo-plugin-tools::instance_ops`（连 `StartInstance` 等 6 条命令一起） |
//! | `bridge`：`AgentMessageHook`（把平台消息变成 agent turn） | **留宿主**——它是核心入站编排，不是 adapter 能力 |
//! | `test_support`：命令域的 mock | 跟着命令域搬走（插件侧自带测试） |
//!
//! ## 为什么 `bridge` 不走
//!
//! 它叫 "bridge"，实质是**入站入口**：会话键构造、单/并行模式的展示会话
//! 选择、`SessionUpdated` / `MessageReceived` 事件、turn 启动、脱敏——每一件
//! 都是核心编排。真正的 adapter 能力（平台协议、收发消息）在
//! `echo-plugin-adapter` 与独立仓库的 `echo-adapter-qq` 里，早已出进程。
//!
//! 强行把它搬进插件只会得到"插件反向调用宿主入站流程"的接口，那比现在
//! 更难读、也没有替换收益（谁会替换掉"平台消息进 agent"这条路？）。

pub mod bridge;

/// 入站桥测试用的 mock 实例（仅测试）。
///
/// 命令域的 mock 已随命令域搬进 `echo-plugin-tools`（那边自带），这里留着的
/// 这一份只服务 `bridge` 的 7 项测试。
#[cfg(test)]
pub(crate) mod test_support;
