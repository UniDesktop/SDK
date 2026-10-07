//! Bridge between the synchronous UDA traits and the async D-Bus work underneath.
//!
//! The platform traits are synchronous by design (C-ABI callers have no async
//! runtime), but zbus is async. The previous bridge built an inline runtime and
//! called `block_on` unconditionally, which panics with "Cannot start a runtime
//! from within a runtime" whenever a host already running tokio calls in (the
//! Linux tray worker, for instance, runs inside one).
//!
//! [`run_async`] picks the right bridge for both contexts:
//!
//! - **No ambient runtime** (the normal synchronous host): an inline
//!   current-thread runtime, built and dropped within the call, so no runtime
//!   is parked for the process lifetime.
//! - **Ambient runtime detected**: the future is polled on a dedicated thread
//!   with its own runtime and the result travels back over a channel. The
//!   caller's thread blocks on the channel instead of `block_on`, which tokio
//!   permits from inside a runtime.
//!
//! # A note on duplication
//!
//! `crates/uda-ffi/src/notify.rs` maintains a structurally identical
//! `run_sync`. The two cannot share code: `uda-ffi` sits above this crate and
//! `uda-core` must stay tokio-free (AGENTS.md Principle 3), so there is no
//! lower home for the bridge. When changing the nesting or thread-reaping
//! behaviour here, evolve the other copy in step.

use std::future::Future;

use crate::error::UdaError;

/// Drive `future` to completion from synchronous code, whatever context it is
/// called from.
///
/// Runtime construction is fallible and reported as [`UdaError::Internal`]; no
/// path in this function may `unwrap` or `expect` (AGENTS.md Principle 1).
pub(crate) fn run_async<T, F>(future: F) -> Result<T, UdaError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // Built on the calling thread so a construction failure is reported as an
    // error instead of stranding the channel below.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

    // The normal case for a synchronous host: no runtime is running, so the
    // inline current-thread runtime drives the future directly.
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(runtime.block_on(future));
    }

    // A host inside tokio: `block_on` on the ambient handle would panic, so the
    // future is moved to a worker thread with its own runtime and the result is
    // carried back over a channel. The thread is named and its construction is
    // fallible: `std::thread::spawn` would panic if the OS refused.
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("uda-async-runner".to_string())
        .spawn(move || {
            // A send failure only means the caller stopped waiting; the value is
            // dropped and the caller below reports the closed channel, so
            // neither side may panic.
            let _ = sender.send(runtime.block_on(future));
        })
        .map_err(|e| UdaError::Internal(format!("could not start the async runner thread: {e}")))?;

    let result = receiver.recv().map_err(|_| {
        UdaError::Internal("the async runner thread ended without a result".to_string())
    })?;

    // The worker has already answered, so joining only reaps the thread that is
    // on its way out - the same reaping discipline as `crate::notify::run_sync`
    // on the FFI side. A panic after delivery cannot invalidate the result the
    // caller is about to receive, and a panic before delivery already surfaced
    // above as the closed channel, so neither case unwinds here.
    if worker.join().is_err() {
        log::debug!("the async runner thread panicked after delivering its result");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_async_answers_outside_any_runtime() {
        let value: i32 = run_async(async { 7 }).expect("run_async must work without a runtime");
        assert_eq!(value, 7);
    }

    #[test]
    fn run_async_answers_inside_a_runtime() {
        // The regression this module exists for: calling the bridge from within
        // a tokio context must not panic ("Cannot start a runtime from within a
        // runtime") and must still deliver the result.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime is always buildable");
        let value = runtime.block_on(async {
            run_async(async { 11i32 }).expect("run_async must work inside a runtime")
        });
        assert_eq!(value, 11);
    }

    #[test]
    fn run_async_propagates_errors_from_the_future() {
        let result: Result<Result<(), UdaError>, UdaError> =
            run_async(async { Err::<(), _>(UdaError::NotSupported("no backend".to_string())) });
        let inner = result.expect("run_async must work without a runtime");
        assert!(matches!(inner, Err(UdaError::NotSupported(_))));
    }
}
