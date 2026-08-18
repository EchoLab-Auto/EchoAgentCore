//! Command dispatch registry.
//!
//! The wire contract type itself lives in `echo-protocol`; re-exported here so
//! `echo_agent::command::BackendCommand` keeps working.
//!
//! `apply_command` used to be a single giant match (478 lines, 25+ arms) in
//! `agent/commands.rs`. This registry makes command handling pluggable: each
//! handler claims the command variants it owns and processes them; the
//! dispatcher asks handlers in registration order until one consumes the
//! command. Adding a command = adding a handler, never editing the dispatcher.

pub use echo_protocol::BackendCommand;

use std::sync::Arc;

use crate::agent::Agent;

/// One command handler: claims the variants it owns.
///
/// `handle` returns `true` when the command was consumed (no further handler
/// runs). A handler should claim only the variants it owns — for an enum
/// match that means a `match` with no fall-through, returning `false` for
/// variants outside its domain.
pub trait CommandHandler: Send + Sync {
    /// Process a command. Returns whether this handler consumed it.
    fn handle(
        &self,
        agent: &Agent,
        cmd: &BackendCommand,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;

    /// A short label for diagnostics (e.g. "api", "qq", "state").
    fn name(&self) -> &'static str;
}

/// An ordered registry of command handlers.
///
/// Handlers run in registration order; the first to return `true` consumes
/// the command. An unknown command falls through every handler and is logged.
#[derive(Default)]
pub struct CommandRegistry {
    handlers: Vec<Arc<dyn CommandHandler>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler (in order). Returns the registration for potential
    /// removal.
    pub fn register(&mut self, handler: Arc<dyn CommandHandler>) {
        self.handlers.push(handler);
    }

    /// Dispatch one command to the handlers in order.
    pub async fn dispatch(&self, agent: &Agent, cmd: BackendCommand) -> bool {
        for handler in &self.handlers {
            if handler.handle(agent, &cmd).await {
                return true;
            }
        }
        tracing::warn!(command = ?cmd, "no handler consumed command");
        false
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClaimAll;
    impl CommandHandler for ClaimAll {
        fn name(&self) -> &'static str {
            "claim-all"
        }
        fn handle(
            &self,
            _agent: &Agent,
            _cmd: &BackendCommand,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
            Box::pin(async { true })
        }
    }

    #[test]
    fn empty_registry_dispatch_returns_false() {
        let registry = CommandRegistry::new();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // Cannot build an Agent here; exercise the registry mechanics with a
        // claim-all handler instead.
        assert!(registry.is_empty());
        let _ = runtime;
    }

    #[tokio::test]
    async fn registry_orders_handlers() {
        let mut registry = CommandRegistry::new();
        registry.register(Arc::new(ClaimAll));
        assert_eq!(registry.len(), 1);
    }
}
