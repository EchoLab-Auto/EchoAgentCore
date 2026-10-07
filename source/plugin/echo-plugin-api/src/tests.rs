//! 协议契约测试：序列化往返、前向兼容（未知字段容忍）、版本协商。

use crate::contribution::*;
use crate::protocol::*;
use serde_json::json;

fn roundtrip_host(msg: HostToPlugin) {
    let text = serde_json::to_string(&msg).unwrap();
    let back: HostToPlugin = serde_json::from_str(&text).unwrap();
    assert_eq!(msg, back, "HostToPlugin roundtrip: {text}");
}

fn roundtrip_plugin(msg: PluginToHost) {
    let text = serde_json::to_string(&msg).unwrap();
    let back: PluginToHost = serde_json::from_str(&text).unwrap();
    assert_eq!(msg, back, "PluginToHost roundtrip: {text}");
}

#[test]
fn host_messages_roundtrip() {
    roundtrip_host(HostToPlugin::Hello(Hello {
        protocol: PROTOCOL_NAME.into(),
        version: PROTOCOL_VERSION,
        plugin_id: "websearch".into(),
        config: json!({"api_key": "k"}),
    }));
    roundtrip_host(HostToPlugin::Invoke(Invoke {
        call_id: "websearch:1".into(),
        contribution: "web_search".into(),
        ctx: InvokeContext {
            session_id: Some("local:tui::local_user".into()),
            team_id: Some("alix".into()),
            branch_id: None,
            deadline_ms: Some(30_000),
        },
        payload: json!({"query": "rust"}),
    }));
    roundtrip_host(HostToPlugin::Cancel(Cancel {
        call_id: "websearch:1".into(),
    }));
    roundtrip_host(HostToPlugin::Event(EventNotification {
        event: "session/start".into(),
        payload: json!({"id": "s1"}),
    }));
    roundtrip_host(HostToPlugin::Drain(Drain { deadline_ms: 5_000 }));
    roundtrip_host(HostToPlugin::Dispose);
}

#[test]
fn plugin_messages_roundtrip() {
    roundtrip_plugin(PluginToHost::Welcome(Welcome {
        protocol: PROTOCOL_NAME.into(),
        version: PROTOCOL_VERSION,
        plugin_id: "websearch".into(),
        capabilities: vec!["tools".into(), "cancel".into()],
    }));
    roundtrip_plugin(PluginToHost::Register(Register {
        contributions: vec![Contribution::Tool(ToolContribution {
            name: "web_search".into(),
            description: "搜索".into(),
            parameters: json!({"type": "object"}),
            category: "builtin".into(),
            timeout_hint_secs: Some(60),
            package: Some("echo-agent.tools.builtin".into()),
        })],
    }));
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "websearch:1".into(),
        outcome: InvokeOutcome::Ok {
            text: "ok".into(),
            images: vec![],
        },
    }));
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "websearch:2".into(),
        outcome: InvokeOutcome::Error {
            code: "timeout".into(),
            message: "deadline".into(),
        },
    }));
    roundtrip_plugin(PluginToHost::Emit(Emit {
        event: "tool/result".into(),
        payload: json!({}),
    }));
    roundtrip_plugin(PluginToHost::Log(LogRecord {
        level: LogLevel::Warn,
        message: "slow upstream".into(),
        fields: None,
    }));
    roundtrip_plugin(PluginToHost::Ready);
    roundtrip_plugin(PluginToHost::Failed(Failure {
        code: "init".into(),
        message: "boom".into(),
    }));
}

#[test]
fn unknown_fields_are_tolerated() {
    // 前向兼容：新端点发的帧，旧端点必须能解码（未知字段忽略）。
    let frame = json!({
        "type": "invoke",
        "payload": {
            "call_id": "p:1",
            "contribution": "t",
            "future_field": {"nested": true},
            "ctx": {"session_id": "s", "another_new_field": 1},
            "payload": {}
        }
    });
    let msg: HostToPlugin = serde_json::from_value(frame).expect("unknown fields tolerated");
    match msg {
        HostToPlugin::Invoke(inv) => {
            assert_eq!(inv.call_id, "p:1");
            assert_eq!(inv.ctx.session_id.as_deref(), Some("s"));
        }
        other => panic!("expected Invoke, got {other:?}"),
    }
}

#[test]
fn unknown_enum_variant_is_rejected_cleanly() {
    // 未知变体：解码失败（可读错误）而非 panic——宿主据此走版本不兼容路径。
    let frame = json!({"type": "future_message", "payload": {}});
    assert!(serde_json::from_value::<HostToPlugin>(frame).is_err());
}

#[test]
fn version_negotiation() {
    assert!(compatible(PROTOCOL_VERSION, PROTOCOL_VERSION));
    assert!(!compatible(1, 2));
    assert!(!compatible(2, 1));
}

#[test]
fn invoke_context_defaults_and_omission() {
    let ctx: InvokeContext = serde_json::from_value(json!({})).unwrap();
    assert_eq!(ctx, InvokeContext::default());
    // 空的 ctx 字段在线上省略（skip_serializing_if）
    let inv = Invoke {
        call_id: "a".into(),
        contribution: "t".into(),
        ctx: InvokeContext::default(),
        payload: json!(null),
    };
    let text = serde_json::to_string(&inv).unwrap();
    assert!(!text.contains("session_id"));
    assert!(!text.contains("team_id"));
}

#[test]
fn contribution_shapes() {
    let tool = Contribution::Tool(ToolContribution {
        name: "bash".into(),
        description: "d".into(),
        parameters: json!({}),
        category: "builtin".into(),
        timeout_hint_secs: None,
        package: None,
    });
    let text = serde_json::to_string(&tool).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(tool, back);
    // 最小字段集：缺省 category 回退 "plugin"，parameters 回退 null
    let minimal: ToolContribution =
        serde_json::from_value(json!({"name": "t", "description": "d"})).unwrap();
    assert_eq!(minimal.category, "plugin");
    assert_eq!(minimal.parameters, json!(null));
}
