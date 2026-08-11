//! `/echo <text>` — reply with the same text.

use async_trait::async_trait;
use echo_core::Event;
use echo_server::{Context, HandleResult, Handler};

#[derive(Debug, Clone)]
pub struct EchoHandler {
    prefix: String,
}

impl EchoHandler {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }
}

#[async_trait]
impl Handler for EchoHandler {
    fn name(&self) -> &str {
        "echo"
    }

    fn priority(&self) -> i32 {
        200
    }

    async fn handle(&self, ctx: &Context, event: &Event) -> HandleResult {
        let Some(msg) = event.as_message() else {
            return HandleResult::Pass;
        };
        let Some(rest) = super::parse_command(&msg.plain_text(), &self.prefix, "echo") else {
            return HandleResult::Pass;
        };

        if rest.is_empty() {
            let _ = ctx
                .reply_to(msg, format!("用法: {}echo <文本>", self.prefix))
                .await;
        } else {
            let _ = ctx.reply_to(msg, rest).await;
        }
        HandleResult::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_server::connection::Outgoing;

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

    /// A test Context whose outbound queue we can inspect.
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

    async fn dispatch_and_read_reply(
        handler: &EchoHandler,
        event: &Event,
    ) -> echo_core::ApiRequest {
        let (ctx, mut api_rx) = test_ctx();
        let result = handler.handle(&ctx, event).await;
        assert_eq!(result, HandleResult::Handled);
        match api_rx.recv().await.expect("reply queued") {
            Outgoing::Api(req) => req,
            other => panic!("expected Api outgoing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn echoes_text_back_to_sender() {
        let handler = EchoHandler::new("/");
        let req = dispatch_and_read_reply(&handler, &private_event("/echo hello 世界")).await;
        assert_eq!(req.action, "send_private_msg");
        assert_eq!(req.params["user_id"], 123456);
        assert_eq!(req.params["message"][0]["data"]["text"], "hello 世界");
    }

    #[tokio::test]
    async fn empty_args_reply_usage_hint() {
        let handler = EchoHandler::new("/");
        let req = dispatch_and_read_reply(&handler, &private_event("/echo")).await;
        let hint = req.params["message"][0]["data"]["text"].as_str().unwrap();
        assert!(hint.contains("用法"));
    }

    #[tokio::test]
    async fn non_echo_message_passes_through() {
        let handler = EchoHandler::new("/");
        let (ctx, mut api_rx) = test_ctx();
        let event = private_event("just some chat");
        let result = handler.handle(&ctx, &event).await;
        assert_eq!(result, HandleResult::Pass);
        assert!(api_rx.try_recv().is_err(), "no reply should be queued");
    }

    #[tokio::test]
    async fn similar_command_not_triggered() {
        let handler = EchoHandler::new("/");
        let (ctx, mut api_rx) = test_ctx();
        let event = private_event("/echofoo");
        let result = handler.handle(&ctx, &event).await;
        assert_eq!(result, HandleResult::Pass);
        assert!(api_rx.try_recv().is_err());
    }
}
