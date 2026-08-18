//! Tests for the definition-layer vocabulary and pure helpers.

use echo_defs::message::{ChatMessage, ChatRole, ToolCall};
use echo_defs::token::{
    estimate_history_tokens, estimate_message_tokens, estimate_tokens, truncate_message_to_tokens,
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
