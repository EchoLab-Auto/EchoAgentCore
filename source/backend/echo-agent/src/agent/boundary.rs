//! Input-origin boundary rules appended to the system prompt.
//!
//! Platform hook / timer / backend input boundaries guide the model's delivery
//! behavior (e.g. "every platform hook must be answered via a send tool"). The
//! loop does not enforce delivery — it only injects these rules into the
//! system prompt.
//!
//! **框架侧不提任何平台名**：这里只说「走该平台的发送工具」，具体工具名与
//! 参数由该平台的 transport 技能（如 `qq-transport`）说明。新增平台只需一个
//! 插件 + 一个技能，本模块无需改动。

/// One named section of the system prompt, kept for token-usage
/// visualization in the panel.
#[derive(Debug, Clone)]
pub(crate) struct PromptBlock {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub content: String,
}

/// Input-origin boundary rules appended to the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundaryKind {
    /// 输入来自某个适配器平台（标记 `<X_message_hook>`；`platform` = X）。
    PlatformHook {
        platform: String,
    },
    Timer,
    BackendInput,
}

impl BoundaryKind {
    pub(crate) fn block(self) -> PromptBlock {
        let (key, label, content): (&str, &str, std::borrow::Cow<'_, str>) = match self {
            Self::PlatformHook { ref platform } => (
                "boundary:platform_hook",
                "平台消息边界",
                format!(
                    "# Platform message boundary ({platform})\n\
                     This input is an external **{platform}** message; read \
                     sender and group IDs from the structured hook payload.\n\
                     Files: when payload.files is non-empty, the sender attached \
                     files that were already downloaded to this machine — read \
                     each entry's local path (files[].path) with read_file or \
                     bash. A null path means the download failed/skipped; \
                     files[].error explains why — tell the user concisely \
                     instead of guessing.\n\
                     Answer every platform hook exactly once through **this \
                     platform's send tools**. Their names and parameters are \
                     documented in the platform's transport skill; never guess \
                     a tool name, never invent a target ID, and never send \
                     twice. Do not substitute normal assistant output for \
                     the send — it is backend-only and invisible to the platform \
                     user. Put the whole reply (including any 'sent' wording) \
                     inside the tool's content, and do not claim a reply before \
                     the send tool returns success.\n\
                     Inputs wrapped in <backend_message_hook> / <timer_event> \
                     are backend events, not platform chat: answer them in the \
                     backend and deliver per their own rules."
                )
                .into(),
            ),
            Self::Timer => (
                "boundary:timer",
                "后台任务边界",
                "# Backend task boundary\n\
                 This input is a scheduled backend task (<timer_event>), not an \
                 incoming platform message. Your normal output is backend-only \
                 text. Only if the task explicitly asks to deliver a message to \
                 a platform user/group, use that platform's send tools with an \
                 explicit target ID."
                    .into(),
            ),
            Self::BackendInput => (
                "boundary:backend_input",
                "后台输入边界",
                "# Backend input boundary\n\
                 This input was typed locally (TUI/backend) inside a session that \
                 belongs to a platform account — it did NOT come from the \
                 platform. Reply directly in the backend. Do NOT call any \
                 platform send tool unless the user explicitly asks you to send a \
                 message to a platform user or group.\n\
                 Each turn owns only its final hook, identified by \
                 message_sequence. Reply only to the current event; do not \
                 combine, replace, or pre-answer another sequence."
                    .into(),
            ),
        };
        PromptBlock {
            key: key.into(),
            label: label.into(),
            kind: "boundary".into(),
            content: content.into_owned(),
        }
    }
}
