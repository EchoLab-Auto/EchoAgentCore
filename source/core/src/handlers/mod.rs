//! Built-in command handlers.

mod admin;
mod echo;
mod help;

pub use admin::AdminHandler;
pub use echo::EchoHandler;
pub use help::HelpHandler;

/// Parse a `prefix + name` command from the start of `text`.
///
/// Returns the argument string (already trimmed), or `None` if `text` does
/// not start with the command. A command must be followed by whitespace or
/// the end of the input, so `/echo` does not match `/echofoo`.
pub(crate) fn parse_command(text: &str, prefix: &str, name: &str) -> Option<String> {
    let expected = format!("{prefix}{name}");
    let rest = text.strip_prefix(&expected)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_bare_command() {
        assert_eq!(parse_command("/help", "/", "help"), Some(String::new()));
    }

    #[test]
    fn matches_command_with_args() {
        assert_eq!(
            parse_command("/echo  hello world", "/", "echo"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn rejects_similar_prefix() {
        assert!(parse_command("/echofoo", "/", "echo").is_none());
        assert!(parse_command("/helpme", "/", "help").is_none());
    }

    #[test]
    fn rejects_non_command() {
        assert!(parse_command("just text", "/", "help").is_none());
    }

    #[test]
    fn trims_leading_whitespace() {
        // Leading whitespace is trimmed; trailing whitespace is preserved.
        assert_eq!(
            parse_command("/echo    spaced  out   ", "/", "echo"),
            Some("spaced  out   ".to_string())
        );
        assert_eq!(parse_command("/help ", "/", "help"), Some(String::new()));
    }

    #[test]
    fn empty_prefix_matches_anywhere() {
        // Empty prefix (auto-fallback) — the command itself must still be
        // token-bounded.
        assert_eq!(parse_command("status", "", "status"), Some(String::new()));
        assert!(parse_command("statusful", "", "status").is_none());
    }

    #[test]
    fn custom_prefix_is_respected() {
        assert_eq!(
            parse_command("!ping hello", "!", "ping"),
            Some("hello".to_string())
        );
        assert!(parse_command("/ping hello", "!", "ping").is_none());
    }
}
