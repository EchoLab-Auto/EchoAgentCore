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
            visible_reply: false,
        })],
    }));
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "websearch:1".into(),
        outcome: InvokeOutcome::Ok {
            text: "ok".into(),
            images: vec![],
            result: serde_json::json!({"structured": true}),
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
fn invoke_outcome_result_field_roundtrip_and_backcompat() {
    // W1.1：结构化结果字段（插件实例 op 返回值的协议载体）。
    // ① 带 result 的帧往返；
    roundtrip_plugin(PluginToHost::InvokeResult(InvokeResult {
        call_id: "adapter:1".into(),
        outcome: InvokeOutcome::Ok {
            text: String::new(),
            images: vec![],
            result: json!({"state": "connected", "self_id": "123"}),
        },
    }));

    // ② 旧插件（不带 result 字段的旧帧）→ 缺省 null（向后兼容解码）；
    let legacy = json!({
        "type": "invoke_result",
        "payload": {
            "call_id": "p:1",
            "outcome": {"status": "ok", "text": "legacy"}
        }
    });
    let msg: PluginToHost = serde_json::from_value(legacy).expect("legacy frame tolerated");
    match msg {
        PluginToHost::InvokeResult(r) => match r.outcome {
            InvokeOutcome::Ok { text, result, .. } => {
                assert_eq!(text, "legacy");
                assert_eq!(result, serde_json::Value::Null);
            }
            other => panic!("expected Ok, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    // ③ result 为 null 时线上省略（旧端点解码无差异）。
    let plain = serde_json::to_string(&PluginToHost::InvokeResult(InvokeResult {
        call_id: "p:2".into(),
        outcome: InvokeOutcome::ok("plain"),
    }))
    .unwrap();
    assert!(
        !plain.contains("\"result\""),
        "null result must be omitted on the wire: {plain}"
    );
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
fn instance_contribution_roundtrip_and_capabilities() {
    // P1（实例插件化）：InstanceContribution 线上形态（贡献 + 能力位 +
    // 词汇结构）。
    let instance = Contribution::PluginInstance(InstanceContribution {
        name: "qq".into(),
        platform: "qq".into(),
        display_name: "QQ / OneBot".into(),
        capabilities: InstanceCapabilities {
            send_text: true,
            send_media: true,
            groups: true,
            friends: true,
            gating: true,
            login: true,
        },
        config_schema: None,
        ui: Some(json!({"version": 1, "panels": []})),
        session_labels: Some(json!({"dm": "私聊", "group": "群"})),
        tools: Vec::new(),
    });
    let text = serde_json::to_string(&instance).unwrap();
    let back: Contribution = serde_json::from_str(&text).unwrap();
    assert_eq!(instance, back);

    // 缺省能力位全 false（fail-closed：宿主只调用声明过的方法）。
    let caps: InstanceCapabilities = serde_json::from_value(json!({})).expect("defaults");
    assert!(!caps.send_text && !caps.gating && !caps.login);

    // 词汇结构：通道与消息线格式。
    let msg = IncomingMessage {
        instance: "qq".into(),
        platform: "qq".into(),
        user_id: "123".into(),
        user_name: Some("Alice".into()),
        channel: ChannelType::Group {
            group_id: "456".into(),
        },
        group_name: Some("开发群".into()),
        content: "hello".into(),
        timestamp: 1733814154,
        at_me: true,
        metadata: json!({}),
        images: vec!["/media/ab12".into()],
        files: vec![],
    };
    let text = serde_json::to_string(&msg).unwrap();
    let back: IncomingMessage = serde_json::from_str(&text).unwrap();
    assert_eq!(msg, back);
    // 私聊通道的最小形态（字段省略容忍）。
    let minimal: IncomingMessage = serde_json::from_value(json!({
        "instance": "qq",
        "platform": "qq",
        "user_id": "123",
        "channel": {"type": "dm"}
    }))
    .expect("minimal message tolerated");
    assert_eq!(minimal.channel, ChannelType::Dm);
    assert!(!minimal.at_me && minimal.images.is_empty());
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
        visible_reply: false,
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
