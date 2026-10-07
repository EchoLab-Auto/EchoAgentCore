//! Service locator (`Ctx`) and reversible registration (`Disposer`).

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::ServiceKey;

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
///
/// [`Ctx::register`] / [`Ctx::resolve`] use plain string keys; the
/// strongly-typed [`ServiceKey`] API on the same table — [`Ctx::provide`],
/// [`Ctx::service`], [`Ctx::require`], [`Ctx::service_or_wait`] — pairs each
/// name with the exact stored type and lets consumers wait for a service to
/// become ready instead of hand-rolling startup ordering.
#[derive(Default)]
pub struct Ctx {
    services: RwLock<HashMap<String, ErasedService>>,
    /// Woken whenever a service is provided or a registration is disposed,
    /// so [`Ctx::service_or_wait`] can re-check the table.
    ready: Notify,
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

    /// Provide `service` under a strongly-typed `key`.
    ///
    /// Unlike [`register`](Self::register), a name can only be provided once:
    /// if `key.name()` is already occupied — by *any* type, including a
    /// string-keyed [`register`](Self::register) — the call fails with
    /// [`ServiceError::Conflict`] and the table is left unchanged.
    ///
    /// On success the service is visible to [`service`](Self::service) /
    /// [`require`](Self::require) immediately, every
    /// [`service_or_wait`](Self::service_or_wait) waiter is woken, and the
    /// returned [`Disposer`] removes the service (waking waiters again) when
    /// dropped or disposed.
    pub fn provide<T: Any + Send + Sync>(
        self: &Arc<Self>,
        key: &ServiceKey<T>,
        service: T,
    ) -> Result<Disposer, ServiceError> {
        {
            let mut services = self.services.write().expect("ctx services poisoned");
            if services.contains_key(key.name()) {
                return Err(ServiceError::Conflict {
                    key: key.name().to_string(),
                });
            }
            services.insert(key.name().to_string(), Box::new(service));
        }
        self.ready.notify_waiters();
        let ctx = self.clone();
        let name = key.name();
        Ok(Disposer::from_fn(move || ctx.remove_and_wake(name)))
    }

    /// Resolve the service stored under a typed `key`.
    ///
    /// Returns `None` both when the key is not registered and when it holds a
    /// value of a different type.
    pub fn service<T: Any + Send + Sync + Clone>(&self, key: &ServiceKey<T>) -> Option<T> {
        let services = self.services.read().expect("ctx services poisoned");
        services.get(key.name())?.downcast_ref::<T>().cloned()
    }

    /// Resolve the service stored under a typed `key`, or report what *is*
    /// available when it is missing.
    pub fn require<T: Any + Send + Sync + Clone>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<T, ServiceError> {
        match self.service(key) {
            Some(service) => Ok(service),
            None => Err(ServiceError::Missing {
                key: key.name().to_string(),
                available: self.keys(),
            }),
        }
    }

    /// Resolve the service stored under a typed `key`, waiting up to
    /// `timeout` for it to be provided.
    ///
    /// Returns immediately on a hit; otherwise waits for a
    /// provide/dispose notification and re-checks the table. Returns `None`
    /// on timeout. The loop registers with the notifier *before* its final
    /// table check, so a `provide` racing the wait cannot be lost.
    pub async fn service_or_wait<T: Any + Send + Sync + Clone>(
        &self,
        key: &ServiceKey<T>,
        timeout: Duration,
    ) -> Option<T> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Fast path: already provided.
            if let Some(service) = self.service(key) {
                return Some(service);
            }
            // A `Notified` future only joins the wait list when polled (or
            // explicitly `enable`d), so register before the re-check below: a
            // `provide` racing this loop is then either seen by the re-check
            // or guaranteed to wake the registered future.
            let notified = self.ready.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();
            if let Some(service) = self.service(key) {
                return Some(service);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return self.service(key);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    // Final look: a `provide` may have raced the timer.
                    return self.service(key);
                }
            }
        }
    }

    /// Remove `name` (if present) and wake every
    /// [`service_or_wait`](Self::service_or_wait) waiter so it can re-check
    /// the table.
    fn remove_and_wake(self: &Arc<Self>, name: &str) {
        {
            let mut services = self.services.write().expect("ctx services poisoned");
            services.remove(name);
        }
        self.ready.notify_waiters();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.services.read().expect("ctx services poisoned").len()
    }
}

/// Errors produced by the strongly-typed [`ServiceKey`] API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// The key is already registered (by any producer, under any type).
    #[error("service key `{key}` is already provided")]
    Conflict { key: String },
    /// The key is missing, or holds a type other than the one requested.
    #[error("service key `{key}` is not available; registered keys: {available:?}")]
    Missing { key: String, available: Vec<String> },
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

    fn expect_conflict(res: Result<Disposer, ServiceError>) -> ServiceError {
        match res {
            Ok(_) => panic!("expected ServiceError::Conflict, got Ok"),
            Err(err) => err,
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

    // ---- strongly-typed ServiceKey API --------------------------------------

    #[test]
    fn provide_and_service_roundtrip() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("typed-port");
        assert_eq!(ctx.service(&key), None);

        let keep = ctx.provide(&key, 3132).expect("provide");
        assert_eq!(ctx.service(&key), Some(3132));
        assert!(ctx.contains("typed-port"));
        // The legacy string API shares the same table entry.
        assert_eq!(ctx.resolve::<u32>("typed-port"), Some(3132));

        keep.dispose();
        assert_eq!(ctx.service(&key), None);
        assert!(!ctx.contains("typed-port"));
    }

    #[test]
    fn provide_typed_trait_object_roundtrip() {
        static GREETER: ServiceKey<Arc<dyn Greeter>> = ServiceKey::new("typed-greeter");
        let ctx = Arc::new(Ctx::default());
        let greeter: Arc<dyn Greeter> = Arc::new(Hello);
        let keep = ctx.provide(&GREETER, greeter).expect("provide");

        let got: Arc<dyn Greeter> = ctx.service(&GREETER).expect("typed service");
        assert_eq!(got.greet(), "hello");
        assert_eq!(ctx.require(&GREETER).unwrap().greet(), "hello");

        drop(keep);
        assert!(ctx.service::<Arc<dyn Greeter>>(&GREETER).is_none());
    }

    #[test]
    fn duplicate_provide_conflicts_and_keeps_original() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("dup");
        let _keep = ctx.provide(&key, 1).expect("first provide");

        let err = expect_conflict(ctx.provide(&key, 2));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "dup".to_string()
            }
        );
        assert_eq!(ctx.service(&key), Some(1), "original value stays in place");
    }

    #[test]
    fn provide_conflict_is_type_agnostic() {
        let ctx = Arc::new(Ctx::default());
        let number = ServiceKey::<u32>::new("occupied");
        let text = ServiceKey::<String>::new("occupied");
        let _keep = ctx.provide(&number, 7).expect("first provide");

        // Same name, different type → still a conflict.
        let err = expect_conflict(ctx.provide(&text, "seven".to_string()));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "occupied".to_string()
            }
        );

        // A string-keyed registration occupies the name just the same.
        let ctx2 = Arc::new(Ctx::default());
        let _legacy = ctx2.register::<u32>("occupied", 1);
        let err = expect_conflict(ctx2.provide(&number, 7));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "occupied".to_string()
            }
        );
    }

    #[test]
    fn service_type_mismatch_is_none() {
        let ctx = Arc::new(Ctx::default());
        let as_u32 = ServiceKey::<u32>::new("mismatch");
        let as_u64 = ServiceKey::<u64>::new("mismatch");
        let _keep = ctx.provide(&as_u32, 7).unwrap();

        assert_eq!(ctx.service(&as_u64), None, "different type under same name");
        assert_eq!(ctx.service(&as_u32), Some(7));
    }

    #[test]
    fn require_missing_reports_available_keys() {
        let ctx = Arc::new(Ctx::default());
        let _legacy = ctx.register::<u32>("legacy", 1);
        let typed = ServiceKey::<u32>::new("typed");
        let _keep = ctx.provide(&typed, 2).unwrap();

        // Typed lookup also finds a string-keyed registration under the name.
        assert_eq!(ctx.service(&ServiceKey::<u32>::new("legacy")), Some(1));

        match ctx.require(&ServiceKey::<u32>::new("nope")) {
            Err(ServiceError::Missing { key, available }) => {
                assert_eq!(key, "nope");
                assert!(
                    available.contains(&"legacy".to_string()),
                    "available: {available:?}"
                );
                assert!(
                    available.contains(&"typed".to_string()),
                    "available: {available:?}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn require_returns_provided_value() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("present");
        let _keep = ctx.provide(&key, 42).unwrap();
        assert_eq!(ctx.require(&key).expect("require"), 42);
    }

    #[tokio::test]
    async fn service_or_wait_returns_immediately_when_registered() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("ready");
        let _keep = ctx.provide(&key, 3132).unwrap();
        assert_eq!(
            ctx.service_or_wait(&key, Duration::from_secs(5)).await,
            Some(3132)
        );
    }

    #[tokio::test]
    async fn service_or_wait_waits_for_late_provider() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("late");
        let provider_ctx = Arc::clone(&ctx);
        let provider = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            provider_ctx.provide(&key, 9).unwrap()
        });

        let got = ctx.service_or_wait(&key, Duration::from_secs(5)).await;
        assert_eq!(got, Some(9));
        // Keep the registration alive past the assertion.
        drop(provider.await.expect("provider task"));
    }

    #[tokio::test]
    async fn service_or_wait_times_out_when_provider_is_too_late() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("too-late");
        let provider_ctx = Arc::clone(&ctx);
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            provider_ctx.provide(&key, 1)
        });

        let started = std::time::Instant::now();
        let got = ctx.service_or_wait(&key, Duration::from_millis(50)).await;
        assert_eq!(got, None, "a 50ms wait must not see a 200ms provider");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(45),
            "should have actually waited, got {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(1),
            "must not hang past its timeout, got {waited:?}"
        );

        late.abort();
    }

    #[tokio::test]
    async fn dispose_during_wait_does_not_hang_the_waiter() {
        let ctx = Arc::new(Ctx::default());
        let provided = ServiceKey::<u32>::new("provided-then-disposed");
        let wanted = ServiceKey::<u32>::new("wanted");

        // A typed registration whose disposal fires the wake-up below.
        let bystander = ctx.provide(&provided, 1).unwrap();

        let waiter_ctx = Arc::clone(&ctx);
        let waiter = tokio::spawn(async move {
            waiter_ctx
                .service_or_wait(&wanted, Duration::from_secs(2))
                .await
        });

        // Dispose an unrelated service while the waiter is parked: the
        // notification wakes it, it re-checks (still missing) and keeps
        // waiting — no spurious return, no hang.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bystander);

        // The awaited service then arrives; the waiter must observe it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _keep = ctx.provide(&wanted, 3132).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must not hang")
            .expect("waiter task must not panic");
        assert_eq!(got, Some(3132));
    }

    #[tokio::test]
    async fn dispose_wake_does_not_falsely_satisfy_waiter() {
        let ctx = Arc::new(Ctx::default());
        let provided = ServiceKey::<u32>::new("bystander-typed");
        let wanted = ServiceKey::<u32>::new("stays-missing");
        let bystander = ctx.provide(&provided, 1).unwrap();

        let started = std::time::Instant::now();
        let waiter_ctx = Arc::clone(&ctx);
        let waiter = tokio::spawn(async move {
            waiter_ctx
                .service_or_wait(&wanted, Duration::from_millis(200))
                .await
        });

        // Dispose during the wait → waiter re-checks and keeps waiting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bystander);

        let got = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter must not hang")
            .expect("waiter task must not panic");
        assert_eq!(
            got, None,
            "an unrelated disposal must not satisfy the waiter"
        );
        // It kept waiting after the ~50ms wake (its timeout is 200ms): it did
        // not bail out early on the notification.
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "must keep waiting after the dispose wake, returned after {:?}",
            started.elapsed()
        );
    }
}
