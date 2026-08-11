//! `/help` — list available commands.

use async_trait::async_trait;
use echo_core::Event;
use echo_server::{Context, HandleResult, Handler};

#[derive(Debug, Clone)]
pub struct HelpHandler {
    prefix: String,
}

impl HelpHandler {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }
}

#[async_trait]
impl Handler for HelpHandler {
    fn name(&self) -> &str {
        "help"
    }

    async fn handle(&self, ctx: &Context, event: &Event) -> HandleResult {
        let Some(msg) = event.as_message() else {
            return HandleResult::Pass;
        };
        if super::parse_command(&msg.plain_text(), &self.prefix, "help").is_none() {
            return HandleResult::Pass;
        }

        let help = format!(
            "EchoAgentPanel 命令列表:\n{}echo <文本> — 复读文本\n{}help — 显示本帮助\n{}status — 运行状态 (仅管理员)\n{}shutdown — 关闭机器人 (仅管理员)",
            self.prefix, self.prefix, self.prefix, self.prefix
        );
        let _ = ctx.reply_to(msg, help).await;
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

    #[tokio::test]
    async fn help_returns_command_list() {
        let handler = HelpHandler::new("/");
        let (ctx, mut api_rx) = test_ctx();
        let result = handler.handle(&ctx, &private_event("/help")).await;
        assert_eq!(result, HandleResult::Handled);
        match api_rx.recv().await.expect("reply queued") {
            Outgoing::Api(req) => {
                let text = req.params["message"][0]["data"]["text"].as_str().unwrap();
                assert!(text.contains("echo"));
                assert!(text.contains("status"));
                assert!(text.contains("shutdown"));
            }
            other => panic!("expected Api outgoing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn help_uses_custom_prefix() {
        let handler = HelpHandler::new("!");
        let (ctx, mut api_rx) = test_ctx();
        let result = handler.handle(&ctx, &private_event("!help")).await;
        assert_eq!(result, HandleResult::Handled);
        match api_rx.recv().await.expect("reply queued") {
            Outgoing::Api(req) => {
                let text = req.params["message"][0]["data"]["text"].as_str().unwrap();
                assert!(text.contains("!echo"));
            }
            other => panic!("expected Api outgoing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_help_message_passes() {
        let handler = HelpHandler::new("/");
        let (ctx, mut api_rx) = test_ctx();
        let result = handler.handle(&ctx, &private_event("hello there")).await;
        assert_eq!(result, HandleResult::Pass);
        assert!(api_rx.try_recv().is_err(), "no reply expected");
    }
}
