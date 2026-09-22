//! Waiters for Android GATT results, one slot per operation key.
//!
//! Lives outside `platform::android` so it can be tested on every host.
//! Kotlin reports a read or write by device, generation and characteristic
//! only, so two operations in flight under one key can't be told apart: the
//! first one's result would complete the second. A slot is therefore held
//! from the moment an operation is dispatched until Kotlin reports it, whether
//! or not its caller is still waiting, and a second operation under the same
//! key waits for the slot rather than replacing it. Kotlin reports every
//! operation it accepted exactly once, and a disconnect frees the attempt's
//! slots through [`OpSlots::release_where`], so a wait always ends.
//!
//! That last part holds only if a slot can't be registered for an attempt
//! that has already been retired: its retirement would have missed the slot,
//! and its result would be dropped as stale. So [`OpSlots::claim`] leaves
//! both the liveness check and [`OpSlots::try_claim`] to the caller, which
//! does them under the lock that retirement takes.

use std::collections::HashMap;

use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct OpSlots<T> {
    slots: Mutex<HashMap<String, oneshot::Sender<T>>>,
    released: Notify,
}

impl<T> Default for OpSlots<T> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            released: Notify::new(),
        }
    }
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl<T> OpSlots<T> {
    /// Wait until `attempt` claims a slot. `attempt` checks that its operation
    /// is still live and calls [`Self::try_claim`], holding the lock that
    /// retirement takes throughout. It runs again after every release, and its
    /// error ends the wait.
    pub(crate) async fn claim<R, E>(
        &self,
        mut attempt: impl FnMut(&Self) -> Result<Option<R>, E>,
    ) -> Result<R, E> {
        loop {
            let released = self.released.notified();
            if let Some(claimed) = attempt(self)? {
                return Ok(claimed);
            }
            released.await;
        }
    }

    /// Hold `key` if it is free.
    pub(crate) fn try_claim(&self, key: String) -> Option<(String, oneshot::Receiver<T>)> {
        let mut slots = self.slots.lock();
        if slots.contains_key(&key) {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        slots.insert(key.clone(), tx);
        Some((key, rx))
    }

    /// Deliver the result for `key` and free its slot. `false` when no slot
    /// was held, or its caller had stopped waiting.
    pub(crate) fn complete(&self, key: &str, value: T) -> bool {
        let tx = self.slots.lock().remove(key);
        self.released.notify_waiters();
        tx.is_some_and(|tx| tx.send(value).is_ok())
    }

    /// Free `key` without a result, for an operation that was never dispatched.
    pub(crate) fn release(&self, key: &str) {
        self.slots.lock().remove(key);
        self.released.notify_waiters();
    }

    /// Free every slot whose key matches; their callers see the channel close.
    pub(crate) fn release_where(&self, mut matches: impl FnMut(&str) -> bool) {
        self.slots.lock().retain(|key, _| !matches(key));
        self.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Claimed<T> = (String, oneshot::Receiver<T>);

    fn ok<T>(key: &str) -> impl FnMut(&OpSlots<T>) -> Result<Option<Claimed<T>>, ()> + '_ {
        move |slots| Ok(slots.try_claim(key.to_owned()))
    }

    #[tokio::test]
    async fn a_result_reaches_the_operation_holding_the_slot() {
        let slots = OpSlots::default();
        let (key, rx) = slots.claim(ok("a")).await.unwrap();
        assert!(slots.complete(&key, 1));
        assert_eq!(rx.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_second_operation_waits_for_the_first_ones_result() {
        let slots = Arc::new(OpSlots::default());
        let (_, first) = slots.claim(ok("a")).await.unwrap();
        let second = tokio::spawn({
            let slots = slots.clone();
            async move { slots.claim(ok("a")).await.unwrap().1.await }
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        assert!(slots.complete("a", 1));
        assert_eq!(first.await.unwrap(), 1);
        tokio::task::yield_now().await;
        assert!(slots.complete("a", 2));
        assert_eq!(second.await.unwrap().unwrap(), 2);
    }

    #[tokio::test]
    async fn a_caller_that_gave_up_keeps_its_slot_until_the_result() {
        let slots = Arc::new(OpSlots::default());
        drop(slots.claim(ok("a")).await.unwrap());
        let second = tokio::spawn({
            let slots = slots.clone();
            async move { slots.claim(ok("a")).await.map(|_| ()) }
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished(), "the abandoned result is still owed");

        assert!(!slots.complete("a", 1), "nobody is waiting for it");
        second.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn other_keys_do_not_wait() {
        let slots = OpSlots::<u8>::default();
        let _a = slots.claim(ok("a")).await.unwrap();
        let _b = slots.claim(ok("b")).await.unwrap();
    }

    #[tokio::test]
    async fn releasing_an_attempt_wakes_its_waiters_to_recheck() {
        let slots = Arc::new(OpSlots::<u8>::default());
        let (_, first) = slots.claim(ok("dev:1:x")).await.unwrap();
        let connected = Arc::new(Mutex::new(true));
        let second = tokio::spawn({
            let (slots, connected) = (slots.clone(), connected.clone());
            async move {
                slots
                    .claim(|slots| {
                        if *connected.lock() {
                            Ok(slots.try_claim("dev:1:x".to_owned()))
                        } else {
                            Err("not connected")
                        }
                    })
                    .await
                    .map(|_| ())
            }
        });
        tokio::task::yield_now().await;

        *connected.lock() = false;
        slots.release_where(|key| key.starts_with("dev:1:"));

        assert!(first.await.is_err(), "the channel closes");
        assert_eq!(second.await.unwrap(), Err("not connected"));
    }

    /// The caller's lock (here `live`) covers the check and the registration,
    /// and retirement takes it too, so there is no moment when an attempt has
    /// retired but a slot can still be registered for it.
    #[tokio::test]
    async fn a_claim_for_a_retired_attempt_leaves_no_slot() {
        let slots = OpSlots::<u8>::default();
        let live = Mutex::new(Some(1));
        let claim = |slots: &OpSlots<u8>| {
            let live = live.lock();
            let generation = live.ok_or("not connected")?;
            Ok(slots.try_claim(format!("dev:{generation}:x")))
        };

        {
            let mut live = live.lock();
            *live = None;
            slots.release_where(|key| key.starts_with("dev:1:"));
        }

        assert_eq!(slots.claim(claim).await.map(|_| ()), Err("not connected"));
        assert!(slots.slots.lock().is_empty());
    }
}
