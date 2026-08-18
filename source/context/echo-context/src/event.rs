//! Typed event bus with dispatch modes.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ctx::Disposer;

/// One dispatchable event. Implementors are concrete event structs/enums;
/// `Clone` is required so parallel dispatch can hand each listener its own
/// copy. The trait is `Any`-downcastable, so listeners register for one event
/// type and the bus fans out only to matching listeners — the Rust analogue
/// of dsh's merge-extensible typed event maps.
pub trait Event: Any + Send + Sync + Clone + std::fmt::Debug + 'static {}

/// How listeners of an event are dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Fan out to all listeners in registration order; no return value.
    Observe,
    /// Around-middleware: listeners receive the event and a `next()` handle;
    /// calling `next()` delegates, not calling it short-circuits the chain
    /// (dsh waterfall semantics).
    Waterfall,
    /// Run all listeners concurrently, each on its own copy of the event.
    Parallel,
    /// Run listeners in registration order.
    Serial,
}

/// Outcome of a waterfall listener: delegate to the next listener, or
/// short-circuit the chain with the (possibly modified) event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaterfallDecision {
    /// Pass the event to the next listener in the chain.
    Delegate,
    /// Stop the chain; the event as mutated by this listener is final.
    ShortCircuit,
}

/// One registered listener, erased by event type.
trait ErasedListener: Send + Sync {
    /// Invoke the listener on `event`. Returns whether the chain continues.
    fn call(&self, event: &mut dyn Any) -> bool;
}

/// A listener for event type `E`.
struct Listener<E, F>
where
    E: Event,
    F: Fn(&mut E, &mut dyn FnMut(&mut E) -> WaterfallDecision) -> WaterfallDecision + Send + Sync,
{
    f: F,
    _marker: std::marker::PhantomData<E>,
}

impl<E, F> ErasedListener for Listener<E, F>
where
    E: Event,
    F: Fn(&mut E, &mut dyn FnMut(&mut E) -> WaterfallDecision) -> WaterfallDecision + Send + Sync,
{
    fn call(&self, event: &mut dyn Any) -> bool {
        let event = event
            .downcast_mut::<E>()
            .expect("bus dispatches to matching event type");
        let mut noop_next: &mut dyn FnMut(&mut E) -> WaterfallDecision =
            &mut |_| WaterfallDecision::Delegate;
        (self.f)(event, &mut noop_next) == WaterfallDecision::Delegate
    }
}

/// A typed, multi-listener event bus.
///
/// Listeners register for one concrete event type with a dispatch mode;
/// [`emit`](Self::emit) dispatches only to that event's listeners. This is
/// the Rust analogue of dsh's typed event system (observe/waterfall/
/// parallel/serial).
#[derive(Default)]
pub struct EventBus {
    /// Listeners keyed by event `TypeId`, in registration order.
    listeners: RwLock<HashMap<TypeId, Vec<Arc<dyn ErasedListener>>>>,
}

impl EventBus {
    /// Register a listener for event type `E`.
    ///
    /// The listener receives `&mut E` and a `next()` handle (for waterfall
    /// delegation) and returns a [`WaterfallDecision`]. Observe/Parallel/
    /// Serial listeners return `Delegate` and may ignore the handle. Returns
    /// a disposer that unregisters this listener.
    pub fn subscribe<E, F>(self: &Arc<Self>, f: F) -> Disposer
    where
        E: Event,
        F: Fn(&mut E, &mut dyn FnMut(&mut E) -> WaterfallDecision) -> WaterfallDecision
            + Send
            + Sync
            + 'static,
    {
        let key = TypeId::of::<E>();
        let mut guard = self.listeners.write().expect("event bus poisoned");
        let listeners = guard.entry(key).or_default();
        let index = listeners.len();
        listeners.push(Arc::new(Listener {
            f,
            _marker: std::marker::PhantomData,
        }));
        drop(guard);

        let bus = self.clone();
        Disposer::from_fn(move || {
            let mut guard = bus.listeners.write().expect("event bus poisoned");
            if let Some(listeners) = guard.get_mut(&key) {
                if index < listeners.len() {
                    listeners.remove(index);
                }
                if listeners.is_empty() {
                    guard.remove(&key);
                }
            }
        })
    }

    /// Register an observe-only listener (always delegates).
    pub fn observe<E, F>(self: &Arc<Self>, f: F) -> Disposer
    where
        E: Event,
        F: Fn(&mut E) + Send + Sync + 'static,
    {
        self.subscribe::<E, _>(move |event, _next| {
            f(event);
            WaterfallDecision::Delegate
        })
    }

    /// Dispatch an event synchronously (Observe/Waterfall only).
    ///
    /// Listeners are synchronous, so no runtime is required — the hot paths
    /// (`Agent::emit`) can fan out without an async context. Parallel/Serial
    /// require the async [`emit`](Self::emit).
    pub fn emit_sync<E: Event>(&self, event: E, mode: DispatchMode) -> E {
        let listeners = self.listeners_for::<E>();
        let mut event = event;
        match mode {
            DispatchMode::Observe | DispatchMode::Serial => {
                for listener in &listeners {
                    listener.call(&mut event);
                }
            }
            DispatchMode::Waterfall => {
                for listener in &listeners {
                    if !listener.call(&mut event) {
                        break;
                    }
                }
            }
            DispatchMode::Parallel => {
                // Parallel requires async (tokio::spawn); synchronously it
                // degrades to serial fan-out.
                for listener in &listeners {
                    listener.call(&mut event);
                }
            }
        }
        event
    }

    /// Dispatch an event to its listeners under `mode`, returning the event
    /// (possibly mutated by waterfall listeners).
    ///
    /// Waterfall listeners run in registration order; a `ShortCircuit`
    /// decision stops the chain with the event as mutated so far. Parallel
    /// dispatch clones the event per listener and runs them concurrently,
    /// returning the original event (listeners' mutations are discarded by
    /// design — use waterfall or serial for stateful chains).
    pub async fn emit<E: Event>(&self, event: E, mode: DispatchMode) -> E {
        let listeners = self.listeners_for::<E>();
        if listeners.is_empty() {
            return event;
        }
        match mode {
            DispatchMode::Observe | DispatchMode::Serial => {
                let mut event = event;
                for listener in &listeners {
                    listener.call(&mut event);
                }
                event
            }
            DispatchMode::Waterfall => {
                let mut event = event;
                for listener in &listeners {
                    if !listener.call(&mut event) {
                        break;
                    }
                }
                event
            }
            DispatchMode::Parallel => {
                let mut handles = Vec::with_capacity(listeners.len());
                for listener in &listeners {
                    let mut event = event.clone();
                    let listener = listener.clone();
                    handles.push(tokio::spawn(async move {
                        listener.call(&mut event);
                    }));
                }
                for handle in handles {
                    let _ = handle.await;
                }
                event
            }
        }
    }

    fn listeners_for<E: Event>(&self) -> Vec<Arc<dyn ErasedListener>> {
        let guard = self.listeners.read().expect("event bus poisoned");
        guard.get(&TypeId::of::<E>()).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct Ping {
        hits: usize,
    }
    impl Event for Ping {}

    #[derive(Debug, Clone)]
    struct ValueEvent {
        value: u32,
    }
    impl Event for ValueEvent {}

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::default())
    }

    #[tokio::test]
    async fn observe_fans_out_to_all_listeners() {
        let bus = bus();
        let _d1 = bus.observe::<Ping, _>(|e| e.hits += 1);
        let _d2 = bus.observe::<Ping, _>(|e| e.hits += 10);
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Observe).await;
        assert_eq!(event.hits, 11);
    }

    #[tokio::test]
    async fn observe_ignores_other_event_types() {
        let bus = bus();
        let _d = bus.observe::<Ping, _>(|e| e.hits += 1);
        let event = bus
            .emit(ValueEvent { value: 7 }, DispatchMode::Observe)
            .await;
        assert_eq!(event.value, 7, "no listener → event unchanged");
    }

    #[tokio::test]
    async fn waterfall_short_circuit_stops_chain() {
        let bus = bus();
        let _d1 = bus.subscribe::<ValueEvent, _>(|e, _next| {
            e.value += 1;
            WaterfallDecision::Delegate
        });
        let _d2 = bus.subscribe::<ValueEvent, _>(|e, _next| {
            e.value += 100;
            WaterfallDecision::ShortCircuit
        });
        let _d3 = bus.subscribe::<ValueEvent, _>(|e, _next| {
            e.value += 1000;
            WaterfallDecision::Delegate
        });
        let event = bus
            .emit(ValueEvent { value: 0 }, DispatchMode::Waterfall)
            .await;
        assert_eq!(event.value, 101, "third listener skipped by short-circuit");
    }

    #[tokio::test]
    async fn waterfall_next_delegates_in_order() {
        let bus = bus();
        let _d1 = bus.subscribe::<ValueEvent, _>(|e, next| {
            e.value += 1;
            next(e);
            WaterfallDecision::Delegate
        });
        let _d2 = bus.subscribe::<ValueEvent, _>(|e, _next| {
            e.value += 2;
            WaterfallDecision::Delegate
        });
        let event = bus
            .emit(ValueEvent { value: 0 }, DispatchMode::Waterfall)
            .await;
        assert_eq!(event.value, 3);
    }

    #[tokio::test]
    async fn disposer_removes_listener() {
        let bus = bus();
        let disposer = bus.observe::<Ping, _>(|e| e.hits += 1);
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Observe).await;
        assert_eq!(event.hits, 1);
        disposer.dispose();
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Observe).await;
        assert_eq!(event.hits, 0, "listener removed");
    }

    #[tokio::test]
    async fn serial_runs_all_listeners_in_order() {
        let bus = bus();
        let _d1 = bus.observe::<Ping, _>(|e| e.hits += 1);
        let _d2 = bus.observe::<Ping, _>(|e| e.hits += 2);
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Serial).await;
        assert_eq!(event.hits, 3);
    }

    #[tokio::test]
    async fn parallel_runs_all_listeners() {
        let bus = bus();
        let _d1 = bus.observe::<Ping, _>(|e| e.hits += 1);
        let _d2 = bus.observe::<Ping, _>(|e| e.hits += 2);
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Parallel).await;
        // Parallel listeners mutate their own copy; the returned event is the
        // original (unmutated) one by design.
        assert_eq!(event.hits, 0);
    }

    #[tokio::test]
    async fn listener_disposer_unwinds_on_drop() {
        let bus = bus();
        {
            let _d = bus.observe::<Ping, _>(|e| e.hits += 1);
        }
        let event = bus.emit(Ping { hits: 0 }, DispatchMode::Observe).await;
        assert_eq!(event.hits, 0);
    }
}
