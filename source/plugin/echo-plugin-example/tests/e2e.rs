//! echo-plugin-example 端到端集成测试（Phase 3 试点）。
//!
//! 用 `echo-plugin-host` 的 [`StdioTransport`] + [`PluginSupervisor`] 启动**真实
//! 插件二进制**（`env!("CARGO_BIN_EXE_echo-plugin-example")`），覆盖：
//!
//! 1. 握手 + 贡献注册（calculator / web_search，schema 非空）；
//! 2. calculator 调用往返（`(2+3)*4` → `"20"`）；
//! 3. calculator 缺参 → `Error{code:"invalid_arguments"}`；
//! 4. 未知工具名 → `Error`；
//! 5. Drain → 插件干净退出（无超时）。
//!
//! 注：web_search 的真实网络调用不放进测试（离线环境会挂）；网络路径由 crate
//! 单测中的纯函数（`parse_rss` / `filter_results` 等）覆盖。

use std::time::{Duration, Instant};

use echo_plugin_host::api::{Contribution, InvokeContext, InvokeOutcome};
use echo_plugin_host::stdio::StdioTransport;
use echo_plugin_host::supervisor::{PluginHandle, PluginState, PluginSupervisor};
use echo_plugin_host::transport::PluginSpec;
use serde_json::json;

const PLUGIN_ID: &str = "echo-plugin-example";

fn spec() -> PluginSpec {
    PluginSpec {
        plugin_id: PLUGIN_ID.to_string(),
        config: json!({}),
    }
}

/// 经 `ECHO_PLUGIN_ID` 演示插件 id 覆盖（SDK `plugin_id()`）。
fn transport() -> StdioTransport {
    StdioTransport::new(env!("CARGO_BIN_EXE_echo-plugin-example"))
        .with_env("ECHO_PLUGIN_ID", PLUGIN_ID)
}

async fn start_plugin() -> PluginHandle {
    PluginSupervisor::new()
        .start(spec(), Box::new(transport()))
        .await
        .expect("handshake with echo-plugin-example should succeed")
}

fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

/// 断言 1：握手后 contributions 含 calculator 与 web_search（schema 非空）。
#[tokio::test]
async fn handshake_registers_pilot_tools_with_schemas() {
    let handle = start_plugin().await;
    assert_eq!(handle.state(), PluginState::Ready);
    assert_eq!(handle.plugin_id(), PLUGIN_ID);

    let contributions = handle.contributions();
    assert_eq!(
        contributions.len(),
        2,
        "expected calculator + web_search: {contributions:?}"
    );

    for name in ["calculator", "web_search"] {
        let tool = contributions
            .iter()
            .find_map(|c| match c {
                Contribution::Tool(tool) if tool.name == name => Some(tool),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing tool contribution {name:?}"));
        assert!(!tool.description.is_empty(), "{name} description empty");
        let properties = tool
            .parameters
            .get("properties")
            .and_then(|value| value.as_object())
            .unwrap_or_else(|| {
                panic!(
                    "{name} schema should declare properties: {:?}",
                    tool.parameters
                )
            });
        assert!(!properties.is_empty(), "{name} schema properties are empty");
        assert!(
            tool.parameters
                .get("required")
                .and_then(|value| value.as_array())
                .is_some_and(|required| !required.is_empty()),
            "{name} schema should declare required fields"
        );
    }

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should succeed");
}

/// 断言 2：invoke calculator `(2+3)*4` → Ok text "20"。
#[tokio::test]
async fn calculator_invocation_returns_result() {
    let handle = start_plugin().await;

    let outcome = handle
        .invoke("calculator", ctx(5_000), json!({"expression": "(2+3)*4"}))
        .await;
    assert_eq!(
        outcome,
        InvokeOutcome::Ok {
            text: "20".to_string(),
            images: vec![],
        }
    );

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should succeed");
}

/// 断言 3：calculator 缺参 → `Error{code:"invalid_arguments"}`。
#[tokio::test]
async fn calculator_missing_arguments_is_an_error() {
    let handle = start_plugin().await;

    let outcome = handle.invoke("calculator", ctx(5_000), json!({})).await;
    match outcome {
        InvokeOutcome::Error { code, message } => {
            assert_eq!(code, "invalid_arguments");
            assert!(!message.is_empty(), "error message should not be empty");
        }
        other => panic!("expected invalid_arguments error, got {other:?}"),
    }

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should succeed");
}

/// 断言 4：未知工具名 → Error。
#[tokio::test]
async fn unknown_tool_returns_error() {
    let handle = start_plugin().await;

    let outcome = handle.invoke("no_such_tool", ctx(5_000), json!({})).await;
    match outcome {
        InvokeOutcome::Error { code, .. } => assert_eq!(code, "unknown_contribution"),
        other => panic!("expected unknown_contribution error, got {other:?}"),
    }

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should succeed");
}

/// 断言 5：drain → 插件干净退出（`drain` 返回 Ok、状态 Stopped、未超时）。
#[tokio::test]
async fn drain_stops_plugin_without_timeout() {
    let handle = start_plugin().await;

    let started = Instant::now();
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should not time out");
    assert_eq!(handle.state(), PluginState::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "drain took too long: {:?}",
        started.elapsed()
    );
}
