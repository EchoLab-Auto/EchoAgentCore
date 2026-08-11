//! The main QQ message handler — trigger gating, filter pipeline, and
//! delivery to the agent's inbound hook.
//!
//! Extracted from `QqAdapter::start()` so the handler logic can be tested
//! independently.

use std::sync::Arc;

use async_trait::async_trait;
use echo_adapter::types::AdapterEvent;
use echo_core::Event as OneBotEvent;
use echo_server::{Context, HandleResult};

use crate::adapter::{QqAdapter, QqInner};

/// Primary handler: converts OneBot events into inbound messages, applies
/// trigger gating and the filter pipeline, then routes to the agent bridge.
pub struct QqHandler {
    inner: Arc<QqInner>,
}

impl QqHandler {
    pub(crate) fn new(inner: Arc<QqInner>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl echo_server::Handler for QqHandler {
    fn name(&self) -> &str {
        "qq-handler"
    }

    fn priority(&self) -> i32 {
        1000
    }

    async fn handle(&self, ctx: &Context, event: &OneBotEvent) -> HandleResult {
        // Store context for outbound sends.
        *self.inner.active_context.lock().expect("ctx poisoned") = Some(Arc::new(ctx.clone()));

        // Lazily learn group names: OneBot group events carry only group_id,
        // so query the name once and cache it for later messages.
        if let Some(gid) = event.as_message().and_then(|msg| msg.group_id()) {
            if !self.inner.group_names.contains_key(&gid) {
                let group_names = self.inner.group_names.clone();
                let ctx = Arc::new(ctx.clone());
                tokio::spawn(async move {
                    let request = echo_core::action::actions::get_group_info(gid);
                    match ctx.send_api(request).await {
                        Ok(resp) => {
                            if let Some(name) = resp
                                .data
                                .get("group_name")
                                .and_then(|n| n.as_str())
                                .filter(|n| !n.trim().is_empty())
                            {
                                group_names.insert(gid, name.trim().to_string());
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%gid, %error, "group name lookup failed");
                        }
                    }
                });
            }
        }

        let Some(msg) = QqAdapter::convert_message(event, &self.inner.group_names) else {
            return HandleResult::Pass;
        };
        // convert_message returns Some only for message events, so this
        // reference is always valid — no unwrap required.
        let message_event = event.as_message();

        // Trigger gating.
        let trigger = &self.inner.config.trigger;
        if msg.channel.is_group() {
            if !trigger.group_at_reply || !msg.at_me {
                tracing::debug!(user = %msg.user_id, "QQ message dropped by trigger gating (not @me or group reply disabled)");
                return HandleResult::Pass;
            }
        } else if !trigger.dm_auto_reply {
            tracing::debug!(user = %msg.user_id, "QQ DM dropped: dm_auto_reply disabled");
            return HandleResult::Pass;
        }

        // Filter pipeline — clone Arc out of the lock so the guard is
        // dropped before .await.
        let pipeline_opt = self.inner.filter.lock().expect("filter poisoned").clone();
        if let Some(pipeline) = pipeline_opt {
            let (accepted, block_reply) = pipeline.should_accept(&msg).await;
            if !accepted {
                tracing::info!(user = %msg.user_id, channel = %msg.channel, "QQ message blocked by filter pipeline");
                if let (Some(reply), Some(message)) = (block_reply, message_event) {
                    let _ = ctx.reply_to(message, reply).await;
                }
                return HandleResult::Handled;
            }
        }

        tracing::info!(
            user = %msg.user_id,
            channel = %msg.channel,
            "QQ message accepted, routing to agent"
        );

        // Deliver to the one-way hook. Agent output can only reach QQ through
        // an explicit send tool; it is never returned through this boundary.
        let hook = self
            .inner
            .message_hook
            .lock()
            .expect("message hook poisoned")
            .clone();
        if let Some(hook) = hook {
            if let Err(error) = hook.on_incoming_message(msg).await {
                tracing::warn!(%error, "inbound message hook failed");
            }
        } else {
            // No hook — forward to subscribers.
            let adapter_event = AdapterEvent::MessageReceived(msg);
            let subs = self.inner.subscribers.lock().expect("subs poisoned");
            for tx in subs.iter() {
                let _ = tx.send(adapter_event.clone());
            }
        }

        HandleResult::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::QqGateMode;
    use crate::config::{QqAdapterConfig, QqTriggerConfig};
    use echo_core::Event;
    use echo_server::connection::Outgoing;
    use echo_server::Handler;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    /// A scriptable inbound hook for handler tests.
    struct MockHook {
        result: StdMutex<Option<Result<(), String>>>,
        messages: StdMutex<Vec<echo_adapter::IncomingMessage>>,
    }

    impl MockHook {
        fn new(result: Result<(), String>) -> Self {
            Self {
                result: StdMutex::new(Some(result)),
                messages: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl echo_adapter::InboundMessageHook for MockHook {
        async fn on_incoming_message(
            &self,
            msg: echo_adapter::IncomingMessage,
        ) -> Result<(), String> {
            self.messages.lock().unwrap().push(msg);
            self.result.lock().unwrap().take().unwrap_or(Ok(()))
        }
        fn on_connection_state(
            &self,
            _adapter_name: &str,
            _connected: bool,
            _self_id: Option<String>,
        ) {
        }
    }

    fn test_inner(trigger: QqTriggerConfig) -> Arc<QqInner> {
        let cfg = QqAdapterConfig {
            trigger,
            ..Default::default()
        };
        Arc::new(QqInner {
            config: cfg,
            active_context: StdMutex::new(None),
            running: AtomicBool::new(false),
            started_at: StdMutex::new(None),
            connected: AtomicBool::new(false),
            self_id: StdMutex::new(None),
            subscribers: StdMutex::new(Vec::new()),
            filter: StdMutex::new(None),
            shutdown_tx: StdMutex::new(None),
            napcat_config_task: StdMutex::new(None),
            tracker: echo_server::ConnectionTracker::default(),
            message_hook: StdMutex::new(None),
            extra_handlers: StdMutex::new(Vec::new()),
            runtime_allowlist_users: StdMutex::new(Vec::new()),
            runtime_allowlist_groups: StdMutex::new(Vec::new()),
            runtime_denylist_users: StdMutex::new(Vec::new()),
            runtime_denylist_groups: StdMutex::new(Vec::new()),
            gate_mode: StdMutex::new(QqGateMode::None),
            config_store: StdMutex::new(None),
            file_bridge: crate::file_bridge::FileBridge::new(
                "http://localhost:3000",
                "127.0.0.1",
                "napcat",
                "/app/napcat/data",
            ),
            group_names: dashmap::DashMap::new(),
        })
    }

    fn test_ctx() -> (Context, tokio::sync::mpsc::Receiver<Outgoing>) {
        let (api_tx, api_rx) = tokio::sync::mpsc::channel(16);
        let ctx = Context::new(
            10001,
            "conn-test".into(),
            api_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        );
        (ctx, api_rx)
    }

    fn private_event(text: &str) -> Event {
        serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "friend",
            "message_id": 42,
            "user_id": 123456,
            "message": [{"type": "text", "data": {"text": text}}],
            "raw_message": text,
            "font": 0,
            "sender": {"user_id": 123456, "nickname": "tester"}
        }))
        .unwrap()
    }

    fn group_event(text: &str, at_me: bool) -> Event {
        let mut segments = vec![serde_json::json!({"type": "text", "data": {"text": text}})];
        if at_me {
            segments.insert(
                0,
                serde_json::json!({"type": "at", "data": {"qq": "10001"}}),
            );
        }
        serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "normal",
            "message_id": 43,
            "group_id": 999,
            "user_id": 123456,
            "message": segments,
            "raw_message": text,
            "font": 0,
            "sender": {"user_id": 123456, "nickname": "tester", "card": ""}
        }))
        .unwrap()
    }

    fn heartbeat_event() -> Event {
        serde_json::from_value(serde_json::json!({
            "post_type": "meta_event",
            "meta_event_type": "heartbeat",
            "time": 1700000000,
            "self_id": 10001,
            "interval": 5000,
            "status": {"online": true}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn non_message_event_passes() {
        let inner = test_inner(QqTriggerConfig::default());
        let handler = QqHandler::new(inner);
        let (ctx, _rx) = test_ctx();
        assert_eq!(
            handler.handle(&ctx, &heartbeat_event()).await,
            HandleResult::Pass
        );
    }

    #[tokio::test]
    async fn dm_dropped_when_dm_auto_reply_disabled() {
        let trigger = QqTriggerConfig {
            dm_auto_reply: false,
            ..Default::default()
        };
        let handler = QqHandler::new(test_inner(trigger));
        let (ctx, _rx) = test_ctx();
        assert_eq!(
            handler.handle(&ctx, &private_event("hi")).await,
            HandleResult::Pass
        );
    }

    #[tokio::test]
    async fn group_message_without_at_me_passes() {
        let handler = QqHandler::new(test_inner(QqTriggerConfig::default()));
        let (ctx, _rx) = test_ctx();
        assert_eq!(
            handler.handle(&ctx, &group_event("hello", false)).await,
            HandleResult::Pass
        );
    }

    #[tokio::test]
    async fn group_message_with_at_me_handled() {
        let handler = QqHandler::new(test_inner(QqTriggerConfig::default()));
        let (ctx, _rx) = test_ctx();
        assert_eq!(
            handler.handle(&ctx, &group_event("hello", true)).await,
            HandleResult::Handled
        );
    }

    #[tokio::test]
    async fn dm_message_routes_to_hook_without_automatic_reply() {
        let hook = Arc::new(MockHook::new(Ok(())));
        let inner = test_inner(QqTriggerConfig::default());
        *inner.message_hook.lock().unwrap() = Some(hook.clone());
        let handler = QqHandler::new(inner);
        let (ctx, mut api_rx) = test_ctx();

        assert_eq!(
            handler.handle(&ctx, &private_event("ping")).await,
            HandleResult::Handled
        );
        assert_eq!(hook.messages.lock().unwrap().len(), 1);
        assert!(
            api_rx.try_recv().is_err(),
            "hook must not auto-send a reply"
        );
    }

    #[tokio::test]
    async fn hook_error_does_not_send_platform_message() {
        let hook: Arc<dyn echo_adapter::InboundMessageHook> =
            Arc::new(MockHook::new(Err("llm down".into())));
        let inner = test_inner(QqTriggerConfig::default());
        *inner.message_hook.lock().unwrap() = Some(hook);
        let handler = QqHandler::new(inner);
        let (ctx, mut api_rx) = test_ctx();

        assert_eq!(
            handler.handle(&ctx, &private_event("ping")).await,
            HandleResult::Handled
        );
        assert!(api_rx.try_recv().is_err(), "hook errors must stay internal");
    }

    #[tokio::test]
    async fn no_bridge_forwards_to_subscribers() {
        let inner = test_inner(QqTriggerConfig::default());
        let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();
        inner.subscribers.lock().unwrap().push(sub_tx);
        let handler = QqHandler::new(inner);
        let (ctx, _api_rx) = test_ctx();

        assert_eq!(
            handler.handle(&ctx, &private_event("hi")).await,
            HandleResult::Handled
        );
        let event = sub_rx.recv().await.expect("subscriber notified");
        assert!(matches!(
            event,
            echo_adapter::AdapterEvent::MessageReceived(_)
        ));
    }
}
