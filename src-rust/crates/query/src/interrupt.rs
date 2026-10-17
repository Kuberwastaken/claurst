//! Epoch-guarded interrupt signal, ported from jcode's agent runtime
//! (`InterruptSignal`, jcode issue #428).
//!
//! claurst's turn cancellation is a `tokio_util CancellationToken` created
//! per turn and only ever cancelled, so it is monotonic by construction.
//! Any future site that needs a resettable fire/reset signal (a timed or
//! deferred reset racing a newer fire) must use [`InterruptSignal`] with
//! [`InterruptSignal::reset_if_epoch`] instead of a bare `AtomicBool`,
//! so a stale deferred reset can never erase a cancel the target has not
//! observed yet.

use std::sync::Arc;

/// Async-aware interrupt signal combining an `AtomicBool` (sync read) with a
/// `tokio::sync::Notify` (async wake) and a monotonic fire epoch.
///
/// [`fire`](Self::fire) bumps the epoch; callers that plan a deferred reset
/// capture the epoch right after firing and later call
/// [`reset_if_epoch`](Self::reset_if_epoch), which skips the reset when a
/// newer fire landed in the meantime instead of erasing it.
#[derive(Clone)]
pub struct InterruptSignal {
    flag: Arc<std::sync::atomic::AtomicBool>,
    epoch: Arc<std::sync::atomic::AtomicU64>,
    notify: Arc<tokio::sync::Notify>,
}

impl InterruptSignal {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn fire(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Unconditional reset. Only safe when no concurrent `fire` can race it;
    /// prefer [`reset_if_epoch`](Self::reset_if_epoch) otherwise.
    pub fn reset(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Current fire epoch. Capture right after a [`fire`](Self::fire) to later
    /// reset only that specific fire via [`reset_if_epoch`](Self::reset_if_epoch).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Reset only if no newer [`fire`](Self::fire) happened since `epoch` was
    /// captured. Returns `true` when the reset was applied. If a fire races
    /// the flag-clear, it is restored so no interrupt is silently erased.
    pub fn reset_if_epoch(&self, epoch: u64) -> bool {
        if self.epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch {
            return false;
        }
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
        if self.epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
            self.notify.notify_waiters();
            return false;
        }
        true
    }

    /// Resolve when the signal is (or becomes) fired. Registers the waiter
    /// with the `Notify` before checking the flag so `notify_waiters()` from
    /// a concurrent [`fire`](Self::fire) cannot be lost between the check
    /// and the park.
    pub async fn notified(&self) {
        let mut notified = std::pin::pin!(self.notify.notified());
        notified.as_mut().enable();
        if self.is_set() {
            return;
        }
        notified.await;
    }

    /// Shared atomic view of the flag, for call sites that read it without
    /// the async machinery. Read-only by contract: callers must only `load`,
    /// never `store`, since a store bypasses the epoch and Notify bookkeeping
    /// and can silently erase a fired interrupt without waking waiters.
    pub fn as_atomic(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// True when `other` is a clone sharing this signal's state.
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.flag, &other.flag)
    }
}

impl Default for InterruptSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn notified_returns_immediately_when_already_fired() {
        let signal = InterruptSignal::new();
        signal.fire();
        tokio::time::timeout(Duration::from_millis(100), signal.notified())
            .await
            .expect("pre-fired signal must resolve notified() immediately");
    }

    #[tokio::test]
    async fn reset_clears_fired_state() {
        let signal = InterruptSignal::new();
        signal.fire();
        assert!(signal.is_set());
        signal.reset();
        assert!(!signal.is_set());
        let waited = tokio::time::timeout(Duration::from_millis(200), signal.notified()).await;
        assert!(waited.is_err(), "reset signal must park notified() again");
    }

    /// A deferred reset holding a stale epoch must not erase a newer fire
    /// (the jcode #428 scenario: rapid repeated interrupts).
    #[test]
    fn reset_if_epoch_skips_when_newer_fire_landed() {
        let signal = InterruptSignal::new();
        signal.fire();
        let first_epoch = signal.epoch();

        signal.fire();
        assert!(
            !signal.reset_if_epoch(first_epoch),
            "stale deferred reset must be skipped"
        );
        assert!(
            signal.is_set(),
            "newer fire must survive the stale deferred reset"
        );

        let second_epoch = signal.epoch();
        assert!(signal.reset_if_epoch(second_epoch));
        assert!(!signal.is_set());

        assert!(!signal.reset_if_epoch(first_epoch));
    }

    /// A fire() racing the flag-clear inside reset_if_epoch() is restored
    /// rather than silently erased.
    #[test]
    fn reset_if_epoch_never_erases_concurrent_fire() {
        for _ in 0..2000 {
            let signal = InterruptSignal::new();
            signal.fire();
            let epoch = signal.epoch();

            let firer = {
                let signal = signal.clone();
                std::thread::spawn(move || signal.fire())
            };
            let _ = signal.reset_if_epoch(epoch);
            firer.join().expect("firer thread");

            assert!(
                signal.is_set(),
                "a concurrent fire() must never be erased by reset_if_epoch()"
            );
        }
    }

    /// fire() must never lose the wakeup no matter where the waiter sits
    /// between creating the notified() future and its first poll.
    #[test]
    fn fire_never_loses_wakeup_while_notified_races() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            for i in 0..2000 {
                let signal = InterruptSignal::new();
                let waiter = {
                    let signal = signal.clone();
                    tokio::spawn(async move { signal.notified().await })
                };
                signal.fire();
                tokio::time::timeout(Duration::from_secs(2), waiter)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("lost wakeup on iteration {i}: notified() missed fire()")
                    })
                    .expect("waiter task must not panic");
            }
        });
    }

    #[test]
    fn same_instance_detects_clones_only() {
        let a = InterruptSignal::new();
        let b = a.clone();
        let c = InterruptSignal::new();
        assert!(a.same_instance(&b));
        assert!(!a.same_instance(&c));
    }
}
