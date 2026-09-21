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
//! only other way out is `CallbackSlots::drain`, for when the platform drops
//! every outstanding request at once (CoreBluetooth leaving `PoweredOn`). That
//! assumes, unverified, that no request from before the drain is answered once
//! a newer one holds its key: with no identity to check, the answer would be
//! taken as the newer one's.
//!
//! `submit`, `take` and `drain` on one set of slots must all run on one
//! serial queue; the lock makes the slots shareable, not ordered.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::error::{BlewError, BlewResult};

/// The way back to a request's caller.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct Answer<T>(oneshot::Sender<BlewResult<T>>);

/// A request's answer and the receiver its caller awaits.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) fn answer_channel<T>() -> (Answer<T>, oneshot::Receiver<BlewResult<T>>) {
    let (tx, rx) = oneshot::channel();
    (Answer(tx), rx)
}

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<T> Answer<T> {
    /// Deliver `result`. `false` when the caller had stopped waiting, which is
    /// the late answer to a request it gave up on.
    pub(crate) fn send(self, result: BlewResult<T>) -> bool {
        self.0.send(result).is_ok()
    }

    /// Whether the caller has stopped waiting.
    pub(crate) fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// A registration the platform still owes a callback.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct Pending<P, T> {
    payload: P,
    answer: Answer<T>,
}

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<P, T> Pending<P, T> {
    /// Separate what the answer applies to from the means of delivering it,
    /// so the payload can take effect before the caller is woken.
    pub(crate) fn split(self) -> (P, Answer<T>) {
        (self.payload, self.answer)
    }

    /// Deliver `result`, discarding the payload. See [`Answer::send`].
    pub(crate) fn answer(self, result: BlewResult<T>) -> bool {
        self.answer.send(result)
    }
}

/// One slot per key, each held until the platform answers it.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) struct CallbackSlots<K, P, T> {
    slots: HashMap<K, Pending<P, T>>,
}

impl<K, P, T> Default for CallbackSlots<K, P, T> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }
}

#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
impl<K: Eq + Hash, P, T> CallbackSlots<K, P, T> {
    /// Hold `key` for a request, with `payload` for whoever takes the answer.
    /// Hands both back while an earlier request under the same key is still
    /// owed its callback, whether or not that request's caller is still
    /// waiting.
    fn register(&mut self, key: K, payload: P, answer: Answer<T>) -> Result<(), (P, Answer<T>)> {
        use std::collections::hash_map::Entry;
        match self.slots.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(Pending { payload, answer });
                Ok(())
            }
            Entry::Occupied(_) => Err((payload, answer)),
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

/// How a request's [`submit`] turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) enum Turn {
    /// Registered and handed to the platform.
    Issued,
    /// Refused with `NotPowered`, never issued.
    NotPowered,
    /// Refused because its key was held, never issued.
    Busy,
    /// Its caller gave up before the turn came; never issued.
    Abandoned,
}

/// One request's turn: register it and `issue` it, or refuse it through
/// `answer`. Run on the slots' queue, nothing lands between the power check,
/// the registration and `issue`. The lock is released before `issue`, which
/// calls into the platform.
#[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
pub(crate) fn submit<K: Eq + Hash, P, T>(
    slots: &Mutex<CallbackSlots<K, P, T>>,
    powered: bool,
    key: K,
    payload: P,
    answer: Answer<T>,
    busy: impl FnOnce() -> BlewError,
    issue: impl FnOnce(),
) -> Turn {
    // Its caller was told it didn't happen.
    if answer.is_closed() {
        return Turn::Abandoned;
    }
    if !powered {
        answer.send(Err(BlewError::NotPowered));
        return Turn::NotPowered;
    }
    let refused = slots.lock().register(key, payload, answer);
    if let Err((_, answer)) = refused {
        answer.send(Err(busy()));
        return Turn::Busy;
    }
    issue();
    Turn::Issued
}

/// Wait at most `limit` for a request's answer. Giving up leaves its slot held.
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
    use std::cell::RefCell;

    use super::*;

    type Slots = Mutex<CallbackSlots<u8, &'static str, u32>>;

    fn busy() -> BlewError {
        BlewError::Peripheral {
            source: "busy".into(),
        }
    }

    fn gave_up() -> BlewError {
        BlewError::Peripheral {
            source: "gave up".into(),
        }
    }

    /// Takes a turn, recording whether it reached the platform.
    fn turn(
        slots: &Slots,
        powered: bool,
        payload: &'static str,
    ) -> (Turn, bool, oneshot::Receiver<BlewResult<u32>>) {
        let (answer, rx) = answer_channel();
        let issued = std::cell::Cell::new(false);
        let turn = submit(slots, powered, 1, payload, answer, busy, || {
            issued.set(true)
        });
        (turn, issued.get(), rx)
    }

    fn held_by(slots: &Slots) -> Option<&'static str> {
        let pending = slots.lock().take(&1)?;
        let (payload, answer) = pending.split();
        slots.lock().register(1, payload, answer).ok()?;
        Some(payload)
    }

    fn power_down(slots: &Slots) {
        let drained = slots.lock().drain();
        for pending in drained {
            pending.answer(Err(BlewError::NotPowered));
        }
    }

    #[test]
    fn a_turn_registers_and_issues() {
        let slots = Slots::default();
        let (turn, issued, _rx) = turn(&slots, true, "a");
        assert_eq!(turn, Turn::Issued);
        assert!(issued);
        assert_eq!(held_by(&slots), Some("a"));
    }

    #[test]
    fn a_turn_while_powered_off_is_refused_and_not_issued() {
        let slots = Slots::default();
        let (turn, issued, mut rx) = turn(&slots, false, "a");
        assert_eq!(turn, Turn::NotPowered);
        assert!(!issued);
        assert!(matches!(rx.try_recv().unwrap(), Err(BlewError::NotPowered)));
        assert_eq!(held_by(&slots), None);
    }

    #[test]
    fn a_held_key_refuses_the_next_turn_without_issuing() {
        let slots = Slots::default();
        let (_, _, _first) = turn(&slots, true, "first");

        let (second, issued, mut rx) = turn(&slots, true, "second");
        assert_eq!(second, Turn::Busy);
        assert!(!issued);
        assert!(matches!(
            rx.try_recv().unwrap(),
            Err(BlewError::Peripheral { .. })
        ));
        assert_eq!(held_by(&slots), Some("first"));
    }

    #[test]
    fn keys_are_independent() {
        let slots = Slots::default();
        let (answer, _a) = answer_channel();
        submit(&slots, true, 1, "a", answer, busy, || {});
        let (answer, _b) = answer_channel();
        assert_eq!(
            submit(&slots, true, 2, "b", answer, busy, || {}),
            Turn::Issued
        );
    }

    #[test]
    fn a_turn_whose_caller_gave_up_in_the_queue_is_not_issued() {
        let slots = Slots::default();
        let (answer, rx) = answer_channel::<u32>();
        drop(rx);
        let issued = std::cell::Cell::new(false);
        let turn = submit(&slots, true, 1, "a", answer, busy, || issued.set(true));
        assert_eq!(turn, Turn::Abandoned);
        assert!(!issued.get());
        assert_eq!(held_by(&slots), None);
    }

    #[test]
    fn the_callback_reaches_the_waiter_and_frees_the_slot() {
        let slots = Slots::default();
        let (_, _, mut rx) = turn(&slots, true, "chars");

        let (payload, answer) = slots.lock().take(&1).unwrap().split();
        assert_eq!(payload, "chars");
        assert!(answer.send(Ok(7)));
        assert_eq!(rx.try_recv().unwrap().unwrap(), 7);
        assert_eq!(turn(&slots, true, "again").0, Turn::Issued);
    }

    #[test]
    fn a_late_answer_is_consumed_by_the_request_it_answers() {
        let slots = Slots::default();
        let (_, _, abandoned) = turn(&slots, true, "first");
        drop(abandoned);

        assert_eq!(turn(&slots, true, "retry").0, Turn::Busy);

        let (payload, answer) = slots.lock().take(&1).unwrap().split();
        assert_eq!(payload, "first");
        assert!(!answer.send(Ok(1)), "reaches nobody");

        let (retry, _, mut rx) = turn(&slots, true, "retry");
        assert_eq!(retry, Turn::Issued);
        assert!(slots.lock().take(&1).unwrap().answer(Ok(2)));
        assert_eq!(rx.try_recv().unwrap().unwrap(), 2);
    }

    #[test]
    fn drain_fails_every_waiter_and_frees_every_slot() {
        let slots = Slots::default();
        let (_, _, mut a) = turn(&slots, true, "a");
        power_down(&slots);
        assert!(matches!(a.try_recv().unwrap(), Err(BlewError::NotPowered)));
        assert_eq!(held_by(&slots), None);
        assert_eq!(turn(&slots, true, "b").0, Turn::Issued);
    }

    /// A turn lands before a power-down or after it, never across it.
    #[test]
    fn a_turn_queued_across_a_power_cycle_cannot_take_a_newer_request() {
        fn queued<'a>(
            slots: &'a Slots,
            issued: &'a RefCell<Vec<&'static str>>,
            name: &'static str,
            answer: Answer<u32>,
        ) -> Box<dyn FnOnce() + 'a> {
            Box::new(move || {
                submit(slots, true, 1, name, answer, busy, || {
                    issued.borrow_mut().push(name);
                });
            })
        }

        let slots = Slots::default();
        let issued = RefCell::new(Vec::new());
        let (answer_a, mut rx_a) = answer_channel();
        let (answer_b, _rx_b) = answer_channel();

        // A was queued before the power-down but runs after power returns
        // and B has taken the key.
        let queue = vec![
            Box::new(|| power_down(&slots)) as Box<dyn FnOnce()>,
            queued(&slots, &issued, "b", answer_b),
            queued(&slots, &issued, "a", answer_a),
        ];
        for step in queue {
            step();
        }

        assert_eq!(*issued.borrow(), vec!["b"], "A must not reach the platform");
        assert!(matches!(
            rx_a.try_recv().unwrap(),
            Err(BlewError::Peripheral { .. })
        ));
        assert_eq!(held_by(&slots), Some("b"), "B keeps its slot");
    }

    #[test]
    fn a_turn_before_the_power_down_is_issued_then_failed_by_it() {
        let slots = Slots::default();
        let (turn_a, issued, mut rx_a) = turn(&slots, true, "a");
        assert_eq!((turn_a, issued), (Turn::Issued, true));

        power_down(&slots);

        assert!(matches!(
            rx_a.try_recv().unwrap(),
            Err(BlewError::NotPowered)
        ));
        assert_eq!(turn(&slots, true, "b").0, Turn::Issued);
    }

    #[tokio::test(start_paused = true)]
    async fn giving_up_leaves_the_slot_held() {
        let slots = Slots::default();
        let (_, _, rx) = turn(&slots, true, "first");

        let result = await_answer(rx, Duration::from_secs(5), gave_up).await;
        assert!(matches!(result, Err(BlewError::Peripheral { .. })));

        assert_eq!(turn(&slots, true, "retry").0, Turn::Busy);
        assert!(!slots.lock().take(&1).unwrap().answer(Ok(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_inside_the_limit_is_returned() {
        let slots = Slots::default();
        let (_, _, rx) = turn(&slots, true, "first");
        assert!(slots.lock().take(&1).unwrap().answer(Ok(9)));

        let result = await_answer(rx, Duration::from_secs(5), gave_up).await;
        assert_eq!(result.unwrap(), 9);
    }
}
