//! Thread-scoped `tracing` capture for tests.
//!
//! `tracing` caches whether any subscriber wants each callsite the first time
//! any thread reaches it. In tracing-core 0.1.36 (`callsite.rs`), when at most
//! one dispatcher was registered as of the last `Dispatch::new`, that
//! registration asks only the reaching thread's default. A test that installs
//! its own subscriber with `tracing::subscriber::with_default` is then at the
//! mercy of every other test thread: one that reaches a callsite first, while
//! this test's subscriber is the only one registered, caches it as wanted by
//! nobody, and nothing recomputes it, so this test never sees that callsite's
//! events. Its assertions then see no events at all, including assertions that
//! something sensitive was never logged.
//!
//! [`with_thread_subscriber`] keeps two other dispatchers registered for the
//! life of the test process, so every callsite registration consults every
//! live subscriber.

use std::sync::LazyLock;
use tracing::{Dispatch, Subscriber, subscriber::NoSubscriber};

/// Runs `body` with `subscriber` as this thread's default subscriber, as
/// `tracing::subscriber::with_default` does, but receiving every event this
/// thread emits, whichever thread first reached its callsite.
pub(crate) fn with_thread_subscriber<T>(
    subscriber: impl Subscriber + Send + Sync + 'static,
    body: impl FnOnce() -> T,
) -> T {
    // Never dropped. Two, so no registration ever sees itself as the only
    // dispatcher, not even while the first capture subscriber registers.
    static BYSTANDERS: LazyLock<[Dispatch; 2]> =
        LazyLock::new(|| [(); 2].map(|()| Dispatch::new(NoSubscriber::default())));
    LazyLock::force(&BYSTANDERS);
    tracing::subscriber::with_default(subscriber, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use tracing::{Event, Metadata, span};

    struct Count(Arc<AtomicUsize>);
    impl Subscriber for Count {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &Event<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    #[test]
    fn a_callsite_another_thread_reaches_first_still_reports_here() {
        fn emit() {
            tracing::info!(target: "cagents::test_tracing", "observed");
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let (installed, wait_for_install) = mpsc::channel();
        let (reached, wait_for_reach) = mpsc::channel();
        // The other thread reaches the callsite first, with no subscriber of
        // its own, while this thread's subscriber is installed.
        let other = std::thread::spawn(move || {
            wait_for_install.recv().unwrap();
            emit();
            reached.send(()).unwrap();
        });
        with_thread_subscriber(Count(seen.clone()), || {
            installed.send(()).unwrap();
            wait_for_reach.recv().unwrap();
            emit();
        });
        other.join().unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 1);
    }
}
