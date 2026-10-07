//! Strongly-typed [`ServiceKey`]s: compile-time pairing of a service name
//! with the exact type stored under it in [`Ctx`](crate::Ctx).
//!
//! Keys are `const`-constructible, so a crate declares the services it
//! provides / consumes once as `static`s and shares them by path — no
//! strings at the call site, and a name can never be resolved as the wrong
//! type.

use std::fmt;
use std::marker::PhantomData;

/// A strongly-typed service key.
///
/// Pairs a `&'static str` name with the exact type `T` stored under it, so
/// [`Ctx::provide`](crate::Ctx::provide), [`Ctx::service`](crate::Ctx::service),
/// [`Ctx::require`](crate::Ctx::require) and
/// [`Ctx::service_or_wait`](crate::Ctx::service_or_wait) can never mix up the
/// types behind a name.
///
/// Declare keys as `static`s next to the interface they identify:
///
/// ```
/// use std::sync::Arc;
/// use echo_context::{Ctx, ServiceKey};
///
/// trait LlmProvider: Send + Sync {
///     fn model(&self) -> &'static str;
/// }
///
/// // Declared once where the interface lives; providers and consumers both
/// // refer to this key without importing each other's concrete types.
/// static LLM: ServiceKey<Arc<dyn LlmProvider>> = ServiceKey::new("llm");
///
/// struct MockProvider;
/// impl LlmProvider for MockProvider {
///     fn model(&self) -> &'static str {
///         "mock"
///     }
/// }
///
/// let ctx = Arc::new(Ctx::default());
/// let _keep = ctx.provide(&LLM, Arc::new(MockProvider)).unwrap();
///
/// let provider: Arc<dyn LlmProvider> = ctx.require(&LLM).unwrap();
/// assert_eq!(provider.model(), "mock");
/// ```
///
/// The marker is `PhantomData<fn() -> T>`: a key never owns a `T`, so it is
/// `Send + Sync + Copy` for *every* `T` — even non-`Send` payloads — and it
/// is covariant in `T`.
pub struct ServiceKey<T> {
    name: &'static str,
    _marker: PhantomData<fn() -> T>,
}

impl<T> ServiceKey<T> {
    /// Create a key addressing the service registered under `name`.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }

    /// The name this key addresses in [`Ctx`](crate::Ctx).
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<T> Clone for ServiceKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ServiceKey<T> {}

impl<T> fmt::Debug for ServiceKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceKey")
            .field("name", &self.name)
            .field("type", &std::any::type_name::<T>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    trait Provider: Send + Sync {}
    impl Provider for () {}

    static LLM: ServiceKey<Arc<dyn Provider>> = ServiceKey::new("llm");

    #[test]
    fn name_is_exposed() {
        let key = ServiceKey::<u32>::new("port");
        assert_eq!(key.name(), "port");
        assert_eq!(LLM.name(), "llm");
    }

    #[test]
    fn keys_are_usable_as_statics() {
        static A: ServiceKey<u32> = ServiceKey::new("a");
        static B: ServiceKey<u32> = ServiceKey::new("b");
        assert_eq!(A.name(), "a");
        assert_eq!(B.name(), "b");
        // `Copy` lets one key be handed to many consumers.
        let copied = A;
        assert_eq!(copied.name(), "a");
    }

    #[test]
    fn key_is_send_sync_copy_for_any_t() {
        fn assert_traits<T: Send + Sync + Copy + Clone + fmt::Debug>() {}
        assert_traits::<ServiceKey<u32>>();
        // Even payloads that are not themselves `Send`/`Sync`: the key never
        // owns a `T`, it only points at one.
        assert_traits::<ServiceKey<std::rc::Rc<u32>>>();
        assert_traits::<ServiceKey<()>>();
    }
}
