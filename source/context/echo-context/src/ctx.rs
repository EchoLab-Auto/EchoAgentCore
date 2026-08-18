//! Service locator (`Ctx`) and reversible registration (`Disposer`).

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// One registered service, erased.
type ErasedService = Box<dyn Any + Send + Sync>;

/// A service locator keyed by name.
///
/// Services are registered as `Arc<dyn Trait>` (or any `Clone + 'static`
/// value) under a string key; consumers resolve by key and trait type,
/// never importing a concrete provider. This is the Rust analogue of dsh's
/// `ctx.<key>` service lookup.
///
/// Registration returns a [`Disposer`]; disposing removes the service, so
/// plugin unload unwinds its contributions.
#[derive(Default)]
pub struct Ctx {
    services: RwLock<HashMap<String, ErasedService>>,
}

impl Ctx {
    /// Register a service under `key`.
    ///
    /// `T` is normally `Arc<dyn Trait>` (the erased box holds the trait
    /// object); it can also be a concrete `Clone + 'static` value. Returns a
    /// disposer that removes the registration.
    pub fn register<T: Any + Send + Sync>(self: &Arc<Self>, key: &str, service: T) -> Disposer {
        self.services
            .write()
            .expect("ctx services poisoned")
            .insert(key.to_string(), Box::new(service));
        Disposer::from((key, self.clone()))
    }

    /// Resolve a service by key.
    ///
    /// `T` must match the registered type exactly — for a trait-object
    /// registration, resolve with the same `Arc<dyn Trait>` type.
    pub fn resolve<T: Any + Send + Sync + Clone>(&self, key: &str) -> Option<T> {
        let guard = self.services.read().expect("ctx services poisoned");
        guard.get(key)?.downcast_ref::<T>().cloned()
    }

    /// Whether a key is registered (without resolving).
    pub fn contains(&self, key: &str) -> bool {
        self.services
            .read()
            .expect("ctx services poisoned")
            .contains_key(key)
    }

    /// All registered keys.
    pub fn keys(&self) -> Vec<String> {
        self.services
            .read()
            .expect("ctx services poisoned")
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.services.read().expect("ctx services poisoned").len()
    }
}

/// Reversible registration handle.
///
/// Drop or call [`dispose`](Self::dispose) to unwind the registration. The
/// Rust analogue of dsh's `ctx.effect()` disposer.
pub struct Disposer {
    inner: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Disposer {
    /// Build a disposer from a custom unwind closure.
    pub fn from_fn(f: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            inner: Some(Box::new(f)),
        }
    }

    /// Remove the registered service.
    pub fn dispose(mut self) {
        if let Some(f) = self.inner.take() {
            f();
        }
    }
}

impl Drop for Disposer {
    fn drop(&mut self) {
        if let Some(f) = self.inner.take() {
            f();
        }
    }
}

impl From<(&str, Arc<Ctx>)> for Disposer {
    fn from((key, ctx): (&str, Arc<Ctx>)) -> Self {
        let key = key.to_string();
        Self::from_fn(move || {
            ctx.services
                .write()
                .expect("ctx services poisoned")
                .remove(&key);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Greeter: Send + Sync {
        fn greet(&self) -> &'static str;
    }
    struct Hello;
    impl Greeter for Hello {
        fn greet(&self) -> &'static str {
            "hello"
        }
    }

    #[test]
    fn register_and_resolve_trait_object() {
        let ctx = Arc::new(Ctx::default());
        let greeter: Arc<dyn Greeter> = Arc::new(Hello);
        let _keep = ctx.register::<Arc<dyn Greeter>>("greeter", greeter);

        let got: Arc<dyn Greeter> = ctx.resolve::<Arc<dyn Greeter>>("greeter").unwrap();
        assert_eq!(got.greet(), "hello");
    }

    #[test]
    fn register_and_resolve_concrete_value() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.register::<u32>("port", 3132);
        assert_eq!(ctx.resolve::<u32>("port"), Some(3132));
    }

    #[test]
    fn resolve_missing_key_is_none() {
        let ctx = Arc::new(Ctx::default());
        assert!(ctx.resolve::<u32>("nope").is_none());
    }

    #[test]
    fn disposer_removes_registration() {
        let ctx = Arc::new(Ctx::default());
        let disposer = ctx.register::<u32>("port", 3132);
        assert!(ctx.contains("port"));
        disposer.dispose();
        assert!(!ctx.contains("port"));
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn disposer_unwinds_on_drop() {
        let ctx = Arc::new(Ctx::default());
        {
            let _disposer = ctx.register::<u32>("port", 3132);
            assert!(ctx.contains("port"));
        }
        assert!(!ctx.contains("port"), "drop unwinds the registration");
    }

    #[test]
    fn type_mismatch_resolves_none() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.register::<u32>("port", 3132);
        assert!(ctx.resolve::<u64>("port").is_none());
    }

    #[test]
    fn re_register_replaces_and_dispose_removes() {
        let ctx = Arc::new(Ctx::default());
        let _d1 = ctx.register::<u32>("port", 3132);
        let _d2 = ctx.register::<u32>("port", 3133);
        assert_eq!(ctx.resolve::<u32>("port"), Some(3133));
        // Disposing the newest registration removes the key entirely.
        drop(_d2);
        assert!(!ctx.contains("port"));
        // The earlier disposer is now a no-op (key already gone).
        drop(_d1);
        assert!(!ctx.contains("port"));
    }
}
