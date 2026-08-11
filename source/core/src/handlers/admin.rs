//! Owner-only admin commands: `/status` and `/shutdown`.

use async_trait::async_trait;
use echo_core::Event;
use echo_server::{ConnectionTracker, Context, HandleResult, Handler};
use tokio::sync::watch;

#[derive(Debug)]
pub struct AdminHandler {
    prefix: String,
    owner_qq: i64,
    shutdown_tx: watch::Sender<bool>,
    tracker: ConnectionTracker,
    started_at: std::time::Instant,
}

impl AdminHandler {
    pub fn new(
        prefix: &str,
        owner_qq: i64,
        shutdown_tx: watch::Sender<bool>,
        tracker: ConnectionTracker,
    ) -> Self {
        Self {
            prefix: prefix.to_string(),
            owner_qq,
            shutdown_tx,
            tracker,
            started_at: std::time::Instant::now(),
        }
    }

    fn is_owner(&self, user_id: i64) -> bool {
        self.owner_qq > 0 && user_id == self.owner_qq
    }
}

#[async_trait]
impl Handler for AdminHandler {
    fn name(&self) -> &str {
        "admin"
    }

    async fn handle(&self, ctx: &Context, event: &Event) -> HandleResult {
        let Some(msg) = event.as_message() else {
            return HandleResult::Pass;
        };
        let text = msg.plain_text();
        let is_status = super::parse_command(&text, &self.prefix, "status").is_some();
        let is_shutdown = super::parse_command(&text, &self.prefix, "shutdown").is_some();
        if !is_status && !is_shutdown {
            return HandleResult::Pass;
        }

        if !self.is_owner(msg.user_id()) {
            let _ = ctx.reply_to(msg, "该命令仅限管理员使用").await;
            return HandleResult::Handled;
        }

        if is_shutdown {
            let _ = ctx.reply_to(msg, "正在关闭…").await;
            let _ = self.shutdown_tx.send(true);
        } else {
            let connections = self.tracker.connections();
            let status = format!(
                "运行状态\n- 机器人 QQ: {}\n- 连接数: {}\n- 累计事件: {}\n- 运行时长: {}s",
                ctx.self_id,
                connections.len(),
                self.tracker.total_events(),
                self.started_at.elapsed().as_secs()
            );
            let _ = ctx.reply_to(msg, status).await;
        }
        HandleResult::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_server::connection::Outgoing;

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

    fn private_event(text: &str, user_id: i64) -> Event {
        serde_json::from_value(serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "time": 1700000000,
            "self_id": 10001,
            "sub_type": "friend",
            "message_id": 42,
            "user_id": user_id,
            "message": [{"type": "text", "data": {"text": text}}],
            "raw_message": text,
            "font": 0,
            "sender": {"user_id": user_id, "nickname": "tester"}
        }))
        .unwrap()
    }

    fn test_handler(owner_qq: i64) -> (AdminHandler, tokio::sync::watch::Receiver<bool>) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        (
            AdminHandler::new("/", owner_qq, shutdown_tx, ConnectionTracker::default()),
            shutdown_rx,
        )
    }

    #[tokio::test]
    async fn non_owner_is_rejected_for_status() {
        let (handler, _rx) = test_handler(99999); // owner is 99999
        let (ctx, mut api_rx) = test_ctx();
        let result = handler
            .handle(&ctx, &private_event("/status", 123456))
            .await;
        assert_eq!(result, HandleResult::Handled);
        match api_rx.recv().await.expect("reply queued") {
            Outgoing::Api(req) => {
                let text = req.params["message"][0]["data"]["text"].as_str().unwrap();
                assert!(text.contains("仅限管理员"));
            }
            other => panic!("expected Api outgoing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn owner_status_reports_runtime_info() {
        let (handler, _rx) = test_handler(123456);
        let (ctx, mut api_rx) = test_ctx();
        let result = handler
            .handle(&ctx, &private_event("/status", 123456))
            .await;
        assert_eq!(result, HandleResult::Handled);
        match api_rx.recv().await.expect("reply queued") {
            Outgoing::Api(req) => {
                let text = req.params["message"][0]["data"]["text"].as_str().unwrap();
                assert!(text.contains("运行状态"));
                assert!(text.contains("10001")); // ctx.self_id
            }
            other => panic!("expected Api outgoing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn owner_shutdown_triggers_signal() {
        let (handler, mut shutdown_rx) = test_handler(123456);
        let (ctx, _api_rx) = test_ctx();
        let result = handler
            .handle(&ctx, &private_event("/shutdown", 123456))
            .await;
        assert_eq!(result, HandleResult::Handled);
        assert!(
            *shutdown_rx.borrow_and_update(),
            "shutdown signal must be sent"
        );
    }

    #[tokio::test]
    async fn non_owner_shutdown_does_not_signal() {
        let (handler, mut shutdown_rx) = test_handler(99999);
        let (ctx, _api_rx) = test_ctx();
        let result = handler
            .handle(&ctx, &private_event("/shutdown", 123456))
            .await;
        assert_eq!(result, HandleResult::Handled);
        assert!(!*shutdown_rx.borrow_and_update());
    }

    #[tokio::test]
    async fn plain_message_passes() {
        let (handler, _rx) = test_handler(123456);
        let (ctx, mut api_rx) = test_ctx();
        let result = handler.handle(&ctx, &private_event("hello", 123456)).await;
        assert_eq!(result, HandleResult::Pass);
        assert!(api_rx.try_recv().is_err());
    }
}
