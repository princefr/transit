//! Cooperative cancellation for journey planning.
//!
//! `tokio::time::timeout` around `spawn_blocking` only stops *waiting*; the
//! planner thread keeps running. Callers wrap planning in [`with_deadline`] and
//! the round / sample loops poll [`expired`] to bail out early with whatever
//! journeys they already have.

use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Run `f` with a planning deadline on the current thread (restored afterwards).
pub fn with_deadline<R>(deadline: Instant, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.with(|d| d.set(self.0));
        }
    }
    let _restore = Restore(DEADLINE.with(|d| d.replace(Some(deadline))));
    f()
}

/// True once the current thread's planning deadline has passed.
#[inline]
pub fn expired() -> bool {
    DEADLINE.with(|d| d.get().is_some_and(|t| Instant::now() >= t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn deadline_scoped_to_closure() {
        assert!(!expired());
        with_deadline(Instant::now() - Duration::from_secs(1), || {
            assert!(expired())
        });
        assert!(!expired());
        with_deadline(Instant::now() + Duration::from_secs(60), || {
            assert!(!expired())
        });
    }
}
