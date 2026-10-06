//! Panic containment for background workers.
//!
//! relm4's `spawn_command`/`spawn_oneshot_command` and `std::thread::spawn`
//! swallow a worker's panic (the hook in `main.rs` only logs it). A worker that
//! panics then never sends its completion message, leaving a "busy" flag set
//! for good. [`catch_or`] turns such a panic into a fallback value, so the
//! completion message is still produced and the usual cleanup runs.

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Runs `body`; if it panics, logs the incident (naming `what`) and returns
/// `fallback()` instead. The panic message itself is already logged by the
/// global panic hook.
///
/// Workers only hand owned data or channel senders across this boundary, and
/// a poisoned lock is recovered by its users (`lock_or_recover`), so asserting
/// unwind safety is sound here.
pub fn catch_or<T>(what: &str, body: impl FnOnce() -> T, fallback: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(v) => v,
        Err(_) => {
            tracing::error!("Background worker '{what}' panicked; running its cleanup");
            fallback()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_body_value_without_panic() {
        let mut fallback_ran = false;
        let v = catch_or(
            "test",
            || 42,
            || {
                fallback_ran = true;
                0
            },
        );
        assert_eq!(v, 42);
        assert!(!fallback_ran);
    }

    #[test]
    fn returns_fallback_on_panic() {
        let v = catch_or("test", || -> i32 { panic!("boom") }, || 7);
        assert_eq!(v, 7);
    }

    #[test]
    fn fallback_completion_message_is_sent_on_panic() {
        // The worker pattern: a panicking body must still yield the "done"
        // message the UI waits for.
        let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let worker = std::thread::spawn(move || {
            let result = catch_or(
                "test worker",
                || -> Result<u32, String> { panic!("worker failed") },
                || Err("internal error".into()),
            );
            let _ = tx.send(result);
        });
        worker.join().expect("panic must not escape catch_or");
        assert_eq!(rx.recv().unwrap(), Err("internal error".to_string()));
    }
}
