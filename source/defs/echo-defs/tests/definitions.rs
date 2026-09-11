//! Tests for the definition-layer vocabulary and pure helpers.

use echo_defs::message::{ChatMessage, ChatRole, ToolCall};
use echo_defs::token::{
    estimate_history_tokens, estimate_image_tokens, estimate_message_tokens, estimate_tokens,
    truncate_message_to_tokens, truncate_text_to_tokens,
};
use proptest::prelude::Just;
use proptest::prop_assert_eq;

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn cjk_chars_count_one_token_each() {
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("中文标点：，。"), 7);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn ascii_chars_count_one_token_per_three() {
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("a"), 1, "at least one token");
        assert_eq!(estimate_tokens("abcd"), 2);
    }

    #[test]
    fn mixed_text_is_summed() {
        // 你好 = 2 CJK；" hello world" 11 个非 CJK 字符 → ceil(11/3) = 4。
        assert_eq!(estimate_tokens("你好 hello world"), 6);
    }

    #[test]
    fn message_tokens_include_structure_overhead() {
        let msg = ChatMessage::user("你好");
        assert_eq!(estimate_message_tokens(&msg), 2 + 4);
    }

    #[test]
    fn history_tokens_sum_messages() {
        let history = vec![ChatMessage::user("你好"), ChatMessage::assistant("ok")];
        assert_eq!(estimate_history_tokens(&history), (2 + 4) + (1 + 4));
    }

    #[test]
    fn long_base64_runs_count_one_token_per_char() {
        // 实测 DeepSeek /anthropic：40k base64 字符 ≈ 28k 输入 token（0.7/字符）。
        // 旧的 1 token / 3 字符把内嵌图片少算 >2×，是 1M 窗口被单张图打爆的根因。
        let payload = "QUJD".repeat(100);
        assert_eq!(payload.len(), 400);
        assert_eq!(estimate_tokens(&payload), 400);
        // 短串（普通英文单词大小）仍按文本计。
        assert_eq!(estimate_tokens("QUJDQUJDQUJDQUJD"), 6);
    }

    #[test]
    fn embedded_image_payload_dominates_the_estimate() {
        let text = format!("look data:image/png;base64,{}", "A".repeat(4096));
        assert!(estimate_tokens(&text) >= 4096);
    }

    #[test]
    fn image_tokens_follow_encoded_size_and_stay_bounded() {
        let tiny = format!("data:image/png;base64,{}", "A".repeat(1000));
        assert_eq!(
            estimate_image_tokens(&tiny),
            85,
            "floor keeps small images honest"
        );
        let medium = format!("data:image/png;base64,{}", "A".repeat(2_000_000));
        assert_eq!(estimate_image_tokens(&medium), 6000);
        let absurd = format!("data:image/png;base64,{}", "A".repeat(20_000_000));
        assert_eq!(estimate_image_tokens(&absurd), 8192, "capped");
        assert_eq!(estimate_image_tokens("https://cdn.example.com/a.png"), 85);
    }

    #[test]
    fn message_tokens_include_tool_calls_and_images() {
        let mut user = ChatMessage::user("hi");
        user.images = vec![format!("data:image/png;base64,{}", "A".repeat(2000))];
        assert!(estimate_message_tokens(&user) >= 1 + 4 + 85);

        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: format!("{{\"script\":\"{}\"}}", "x".repeat(300)),
        }]);
        assert!(
            estimate_message_tokens(&assistant) > 300,
            "tool arguments count toward the budget"
        );
    }

    #[test]
    fn truncation_honours_the_base64_cost_model() {
        let text = format!("head data:image/png;base64,{}", "A".repeat(4096));
        let truncated = truncate_text_to_tokens(&text, 500);
        assert!(estimate_tokens(&truncated) <= 500);
        assert!(truncated.contains("内容过长已截断"));
    }

    #[test]
    fn truncate_message_fits_budget_and_keeps_marker() {
        let mut msg = ChatMessage::user("x".repeat(600));
        truncate_message_to_tokens(&mut msg, 100);
        assert!(estimate_message_tokens(&msg) <= 100);
        assert!(msg.content.contains("内容过长已截断"));
    }
}

#[cfg(test)]
mod message_tests {
    use super::*;

    proptest::proptest! {
        /// ChatMessage JSON round-trip for any plausible message.
        #[test]
        fn chat_message_json_roundtrip(
            role in proptest::prop_oneof![
                Just(ChatRole::System),
                Just(ChatRole::User),
                Just(ChatRole::Assistant),
                Just(ChatRole::Tool),
            ],
            content in ".*",
            has_tool_call in proptest::bool::ANY,
            tool_call_id in "[a-z0-9_-]{0,20}",
        ) {
            let tool_calls = if has_tool_call {
                Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "calculator".into(),
                    arguments: "{\"expr\":\"1+1\"}".into(),
                }])
            } else {
                None
            };
            let msg = ChatMessage {
                role,
                content,
                reasoning_content: None,
                tool_calls,
                tool_call_id: (role == ChatRole::Tool).then_some(tool_call_id),
            images: vec![],
            };
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back.role, msg.role);
            prop_assert_eq!(back.content, msg.content);
            prop_assert_eq!(back.tool_calls, msg.tool_calls);
            prop_assert_eq!(back.tool_call_id, msg.tool_call_id);
        }

        /// ToolCall JSON round-trip.
        #[test]
        fn tool_call_json_roundtrip(
            id in "[a-z0-9_-]{1,20}",
            name in "[a-z0-9_]{1,20}",
            arguments in ".*",
        ) {
            let call = ToolCall { id, name, arguments };
            let json = serde_json::to_string(&call).expect("serialize");
            let back: ToolCall = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, call);
        }
    }
}
