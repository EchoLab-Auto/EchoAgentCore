//! EchoAgentCore service context — the Rust analogue of dsh's `ctx`.
//!
//! Three mechanisms:
//!
//! - [`Ctx`] — a service locator keyed by name. Services are registered as
//!   `Arc<dyn Trait>` (or any `Clone + 'static` value) and resolved by key;
//!   registration returns a [`Disposer`] that unwinds it, so every
//!   contribution is a reversible effect (dsh `ctx.effect()`).
//! - [`EventBus`] — typed events with dispatch modes: [`DispatchMode::Observe`]
//!   (fan-out), [`DispatchMode::Waterfall`] (around-middleware with `next()`
//!   delegation), [`DispatchMode::Parallel`] (all listeners concurrently),
//!   [`DispatchMode::Serial`] (in order, first refusal stops). Events are
//!   merge-extensible via the [`Event`] trait (the Rust analogue of dsh's
//!   declaration merging): any new event type is a new `Event` implementor.
//! - [`ScopedRegistry`] — name-keyed registrations with per-scope shadowing:
//!   a scoped entry replaces its same-named global twin for that scope alone.

mod ctx;
mod event;
mod scope;

pub use ctx::{Ctx, Disposer};
pub use event::{DispatchMode, Event, EventBus, WaterfallDecision};
pub use scope::ScopedRegistry;
