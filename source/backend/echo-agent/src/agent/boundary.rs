//! Input-origin boundary rules appended to the system prompt.
//!
//! QQ hook / timer / backend input boundaries guide the model's delivery
//! behavior (e.g. "every QQ hook must be answered via a send tool"). The
//! loop does not enforce delivery — it only injects these rules into the
//! system prompt.

/// Input-origin boundary rules appended to the system prompt.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundaryKind {
    QqHook,
    Timer,
    BackendInput,
}

impl BoundaryKind {
    pub(crate) fn block(self) -> PromptBlock {
        let (key, label, content) = match self {
            Self::QqHook => (
                "boundary:qq_hook",
                "QQ 消息边界",
                "# QQ transport boundary
                 This input is an external QQ message (<qq_message_hook>); read                  sender and group IDs from the structured hook payload.
                 Answer every <qq_message_hook> exactly once via a send tool:                  send_private_msg for private chats (user_id from                  payload.sender.user_id), send_group_msg for groups (group_id                  from payload.channel.group_id). Never invent a target ID, never                  send twice, and never substitute normal assistant output for                  the send — it is backend-only and invisible to the QQ user. Put                  the whole reply (including any 'sent' wording) inside the                  tool's content, and do not claim a reply before the send tool                  returns success.
                 Inputs wrapped in <backend_message_hook>/<timer_event> are                  backend events, not QQ chat: answer them in the backend and                  deliver per their own rules.",
            ),
            Self::Timer => (
                "boundary:timer",
                "后台任务边界",
                "# Backend task boundary
                 This input is a scheduled backend task (<timer_event>), not an                  incoming QQ message. Your normal output is backend-only text.                  Only if the task explicitly asks to deliver a message to QQ,                  call send_private_msg or send_group_msg with an explicit target ID.",
            ),
            Self::BackendInput => (
                "boundary:backend_input",
                "后台输入边界",
                "# Backend input boundary
                 This input was typed in the backend/TUI (no <qq_message_hook>).                  Answer it in the backend directly — do NOT call send_private_msg                  or send_group_msg (they would push to a QQ user who did not                  send this).",
            ),
        };
        PromptBlock {
            key: key.into(),
            label: label.into(),
            kind: "boundary".into(),
            content: content.into(),
        }
    }
}

/// One named section of the system prompt, kept for token-usage
/// visualization in the panel.
#[derive(Debug, Clone)]
pub(crate) struct PromptBlock {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub content: String,
}
