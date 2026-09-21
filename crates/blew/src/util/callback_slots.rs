//! Waiters for platform callbacks that don't say which request they answer.
//!
//! Lives outside `platform::apple` so it can be tested on every host.
//! CoreBluetooth answers `addService:`, `startAdvertising:` and
//! `publishL2CAPChannelWithEncryption:` through delegate callbacks that
//! identify the request by its service UUID at most, and the other two not at
//! all. Once a caller can stop waiting -- it timed out, or its future was
//! dropped -- the answer can still arrive later, and a newer request under the
//! same key would take it for its own: a stale success confirming a request
//! the platform never answered.
//!
//! So a slot is held from registration until the callback that answers it
//! arrives, whether or not anyone is still waiting, and a request that finds
//! its key held is refused rather than queued or allowed to replace it. The
//! only other way out is the platform dropping every outstanding request at
//! once, which CoreBluetooth does when the adapter leaves `PoweredOn`; see
//! [`CallbackSlots::drain`].

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::{BlewError, BlewResult};

/// Identifies one registration, so withdrawing it can't remove a later one
/// that took the same key.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket(u64);

/// A registration the platform still owes a callback.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct Pending<P, T> {
    ticket: Ticket,
    payload: P,
    tx: oneshot::Sender<BlewResult<T>>,
}

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<P, T> Pending<P, T> {
    /// Separate what the answer applies to from the means of delivering it,
    /// so the payload can take effect before the caller is woken.
    pub(crate) fn split(self) -> (P, Answer<T>) {
        (self.payload, Answer(self.tx))
    }

    /// Deliver `result`, discarding the payload. See [`Answer::send`].
    pub(crate) fn answer(self, result: BlewResult<T>) -> bool {
        self.split().1.send(result)
    }
}

/// The way back to a registration's caller.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct Answer<T>(oneshot::Sender<BlewResult<T>>);

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<T> Answer<T> {
    /// Deliver `result`. `false` when the caller had stopped waiting, which is
    /// the late answer to a request it gave up on.
    pub(crate) fn send(self, result: BlewResult<T>) -> bool {
        self.0.send(result).is_ok()
    }
}

/// One slot per key, each held until the platform answers it.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct CallbackSlots<K, P, T> {
    next_ticket: u64,
    slots: HashMap<K, Pending<P, T>>,
}

impl<K, P, T> Default for CallbackSlots<K, P, T> {
    fn default() -> Self {
        Self {
            next_ticket: 0,
            slots: HashMap::new(),
        }
    }
}

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<K: Eq + Hash, P, T> CallbackSlots<K, P, T> {
    /// Claim `key` for a request about to be issued, carrying `payload` for
    /// whoever takes the answer. `None` while an earlier request under the
    /// same key is still owed its callback, whether or not that request's
    /// caller is still waiting.
    pub(crate) fn register(
        &mut self,
        key: K,
        payload: P,
    ) -> Option<(Ticket, oneshot::Receiver<BlewResult<T>>)> {
        use std::collections::hash_map::Entry;
        let Entry::Vacant(slot) = self.slots.entry(key) else {
            return None;
        };
        self.next_ticket = self.next_ticket.wrapping_add(1);
        let ticket = Ticket(self.next_ticket);
        let (tx, rx) = oneshot::channel();
        slot.insert(Pending {
            ticket,
            payload,
            tx,
        });
        Some((ticket, rx))
    }

    /// Give back a registration whose request was never issued, so no callback
    /// is owed for it. Removes only the registration `ticket` names: a
    /// [`drain`](Self::drain) can already have freed it, and a later
    /// registration taken the key.
    pub(crate) fn withdraw(&mut self, key: &K, ticket: Ticket) -> bool {
        if self.slots.get(key).is_some_and(|p| p.ticket == ticket) {
            self.slots.remove(key);
            true
        } else {
            false
        }
    }

    /// The registration a callback for `key` answers, freeing its slot.
    pub(crate) fn take(&mut self, key: &K) -> Option<Pending<P, T>> {
        self.slots.remove(key)
    }

    /// Every registration, freeing every slot. For when the platform has
    /// dropped all outstanding requests and will answer none of them.
    pub(crate) fn drain(&mut self) -> Vec<Pending<P, T>> {
        self.slots.drain().map(|(_, pending)| pending).collect()
    }
}

/// Wait at most `limit` for a registration's answer.
///
/// Giving up leaves the slot held: the platform still owes the answer, and it
/// has to be consumed by this registration rather than by the next one under
/// the same key.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) async fn await_answer<T>(
    rx: oneshot::Receiver<BlewResult<T>>,
    limit: Duration,
    timed_out: impl FnOnce() -> BlewError,
) -> BlewResult<T> {
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(BlewError::Internal(
            "callback slot dropped before it was answered".into(),
        )),
        Err(_) => Err(timed_out()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slots = CallbackSlots<u8, &'static str, u32>;

    fn gave_up() -> BlewError {
        BlewError::Peripheral {
            source: "gave up".into(),
        }
    }

    #[test]
    fn a_held_key_refuses_a_second_registration() {
        let mut slots = Slots::default();
        let _first = slots.register(1, "first").unwrap();
        assert!(slots.register(1, "second").is_none());
    }

    #[test]
    fn keys_are_independent() {
        let mut slots = Slots::default();
        let _a = slots.register(1, "a").unwrap();
        assert!(slots.register(2, "b").is_some());
    }

    #[test]
    fn the_callback_reaches_the_waiter_and_frees_the_slot() {
        let mut slots = Slots::default();
        let (_, mut rx) = slots.register(1, "chars").unwrap();

        let (payload, answer) = slots.take(&1).unwrap().split();
        assert_eq!(payload, "chars");
        assert!(answer.send(Ok(7)));
        assert_eq!(rx.try_recv().unwrap().unwrap(), 7);

        assert!(slots.register(1, "again").is_some());
    }

    /// The case the whole type exists for: a request whose caller stopped
    /// waiting still owns its key, so its late answer can't confirm the next.
    #[test]
    fn a_late_answer_is_consumed_by_the_request_it_answers() {
        let mut slots = Slots::default();
        let (_, abandoned) = slots.register(1, "first").unwrap();
        drop(abandoned);

        assert!(
            slots.register(1, "retry").is_none(),
            "the retry must not take a key the platform still owes an answer on"
        );

        // The first request's answer lands, is consumed, and reaches nobody.
        let (payload, answer) = slots.take(&1).unwrap().split();
        assert_eq!(payload, "first");
        assert!(!answer.send(Ok(1)));

        // Only now does the retry get the key, and only its own answer.
        let (_, mut rx) = slots.register(1, "retry").unwrap();
        assert!(slots.take(&1).unwrap().answer(Ok(2)));
        assert_eq!(rx.try_recv().unwrap().unwrap(), 2);
    }

    #[test]
    fn withdraw_removes_only_its_own_registration() {
        let mut slots = Slots::default();
        let (stale, _) = slots.register(1, "stale").unwrap();
        for pending in slots.drain() {
            pending.answer(Err(BlewError::NotPowered));
        }
        let (current, _rx) = slots.register(1, "current").unwrap();

        assert!(!slots.withdraw(&1, stale));
        assert!(
            slots.register(1, "third").is_none(),
            "current still holds it"
        );
        assert!(slots.withdraw(&1, current));
        assert!(slots.register(1, "third").is_some());
    }

    #[test]
    fn drain_fails_every_waiter_and_frees_every_slot() {
        let mut slots = Slots::default();
        let (_, mut a) = slots.register(1, "a").unwrap();
        let (_, abandoned) = slots.register(2, "b").unwrap();
        drop(abandoned);

        let drained = slots.drain();
        assert_eq!(drained.len(), 2);
        for pending in drained {
            pending.answer(Err(BlewError::NotPowered));
        }

        assert!(matches!(a.try_recv().unwrap(), Err(BlewError::NotPowered)));
        assert!(slots.take(&1).is_none());
        assert!(slots.register(1, "a").is_some());
        assert!(slots.register(2, "b").is_some());
    }

    #[test]
    fn a_callback_with_nothing_registered_is_unclaimed() {
        let mut slots = Slots::default();
        assert!(slots.take(&1).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn giving_up_leaves_the_slot_held() {
        let mut slots = Slots::default();
        let (_, rx) = slots.register(1, "first").unwrap();

        let result = await_answer(rx, Duration::from_secs(5), gave_up).await;
        assert!(matches!(result, Err(BlewError::Peripheral { .. })));

        assert!(slots.register(1, "retry").is_none());
        assert!(!slots.take(&1).unwrap().answer(Ok(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_inside_the_limit_is_returned() {
        let mut slots = Slots::default();
        let (_, rx) = slots.register(1, "first").unwrap();
        assert!(slots.take(&1).unwrap().answer(Ok(9)));

        let result = await_answer(rx, Duration::from_secs(5), gave_up).await;
        assert_eq!(result.unwrap(), 9);
    }
}
