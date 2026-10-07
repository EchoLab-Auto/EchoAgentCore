//! EchoAgentCore service context — the Rust analogue of dsh's `ctx`.
//!
//! Three mechanisms:
//!
//! - [`Ctx`] — a service locator keyed by name. Services are registered as
//!   `Arc<dyn Trait>` (or any `Clone + 'static` value) and resolved by key;
//!   registration returns a [`Disposer`] that unwinds it, so every
//!   contribution is a reversible effect (dsh `ctx.effect()`). On top of the
//!   string-keyed API, the strongly-typed [`ServiceKey`] API
//!   ([`Ctx::provide`], [`Ctx::service`], [`Ctx::require`],
//!   [`Ctx::service_or_wait`]) pairs each name with the exact stored type and
//!   lets consumers wait for a service to become ready.
//! - [`EventBus`] — typed events with dispatch modes: [`DispatchMode::Observe`]
//!   (fan-out), [`DispatchMode::Waterfall`] (around-middleware with `next()`
//!   delegation), [`DispatchMode::Parallel`] (all listeners concurrently),
//!   [`DispatchMode::Serial`] (in order; currently identical to `Observe` — no short-circuit, no production emitter yet). Events are
//!   merge-extensible via the [`Event`] trait (the Rust analogue of dsh's
//!   declaration merging): any new event type is a new `Event` implementor.
//! - [`ScopedRegistry`] — name-keyed registrations with per-scope shadowing:
//!   a scoped entry replaces its same-named global twin for that scope alone.

mod ctx;
mod event;
mod scope;
mod service_key;

pub use ctx::{Ctx, Disposer, ServiceError};
pub use event::{DispatchMode, Event, EventBus, WaterfallDecision};
pub use scope::ScopedRegistry;
pub use service_key::ServiceKey;
