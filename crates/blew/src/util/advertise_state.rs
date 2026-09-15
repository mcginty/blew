//! Single-flight advertising state.
//!
//! Lives outside `platform::android` so it can be tested on every host rather
//! than only on a device. The transitions matter more than they look: the
//! platform can only stop an advertisement by handing back the exact
//! `AdvertiseCallback` it was started with, so a second concurrent start would
//! strand the first with nothing able to reach it, and a start that completes
//! after a stop would leave the slot claimed while the radio is idle.

use tokio::sync::oneshot;

use crate::error::BlewResult;

type Waiter = oneshot::Sender<BlewResult<()>>;

/// What the advertiser is doing.
///
/// One value rather than a `pending` slot beside an `active` flag, because the
/// two always had to change together and the window between them was a bug:
/// a stop landing there was undone by the resuming start.
#[derive(Default)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) enum Advertising {
    #[default]
    Idle,
    /// A start is in flight, waiting on the stack's callback.
    Starting(i32, Waiter),
    /// The stack confirmed advertising started.
    Active(i32),
}

impl Advertising {
    /// The request that owns the slot, if any.
    pub(crate) fn request_id(&self) -> Option<i32> {
        match self {
            Self::Idle => None,
            Self::Starting(id, _) | Self::Active(id) => Some(*id),
        }
    }
}

#[derive(Default)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct AdvertiseState {
    next_id: i32,
    current: Advertising,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl AdvertiseState {
    /// Claim the slot. `None` when it is already taken.
    pub(crate) fn register(&mut self) -> Option<(i32, oneshot::Receiver<BlewResult<()>>)> {
        if !matches!(self.current, Advertising::Idle) {
            return None;
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let (tx, rx) = oneshot::channel();
        self.current = Advertising::Starting(id, tx);
        Some((id, rx))
    }

    /// Apply the stack's verdict for `request_id`, returning its waiter.
    ///
    /// The `Starting` -> `Active` transition happens here rather than in the
    /// woken task, so a stop cannot slip between the wake-up and the
    /// transition and then be undone by the task that resumes.
    ///
    /// A verdict for a request that no longer owns the slot is dropped.
    pub(crate) fn complete(&mut self, request_id: i32, started: bool) -> Option<Waiter> {
        match std::mem::take(&mut self.current) {
            Advertising::Starting(id, tx) if id == request_id => {
                self.current = if started {
                    Advertising::Active(id)
                } else {
                    Advertising::Idle
                };
                Some(tx)
            }
            other => {
                self.current = other;
                None
            }
        }
    }

    /// Return the slot to `Idle` if `request_id` still owns it.
    ///
    /// Applies in either state: a caller dropped after its start was confirmed
    /// still needs the advertisement torn down, because nobody received the
    /// `Ok`.
    pub(crate) fn release(&mut self, request_id: i32) -> bool {
        if self.current.request_id() == Some(request_id) {
            self.current = Advertising::Idle;
            true
        } else {
            false
        }
    }

    /// Take the slot whoever owns it, for an explicit stop. The displaced
    /// value lets the caller wake a start that was still in flight rather than
    /// leaving it on its deadline.
    pub(crate) fn take(&mut self) -> Advertising {
        std::mem::take(&mut self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_start_is_refused_while_starting() {
        let mut state = AdvertiseState::default();
        let (_id, _rx) = state.register().expect("first start claims the slot");
        assert!(state.register().is_none());
    }

    #[test]
    fn a_second_start_is_refused_while_active() {
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();
        state.complete(id, true).expect("waiter");
        assert!(state.register().is_none(), "active must refuse a new start");
    }

    #[test]
    fn ids_are_unique_across_requests() {
        let mut state = AdvertiseState::default();
        let (first, _rx) = state.register().unwrap();
        state.complete(first, false);
        let (second, _rx) = state.register().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn a_failed_start_frees_the_slot() {
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();
        state.complete(id, false).expect("waiter");
        assert!(state.register().is_some());
    }

    #[test]
    fn a_stale_verdict_is_ignored() {
        let mut state = AdvertiseState::default();
        let (first, _rx) = state.register().unwrap();
        state.release(first);
        let (second, _rx) = state.register().unwrap();

        // The abandoned request's callback arrives late.
        assert!(state.complete(first, true).is_none());
        // ...and must not have disturbed the request that owns the slot now.
        assert_eq!(state.take().request_id(), Some(second));
    }

    #[test]
    fn release_only_applies_to_the_owner() {
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();
        assert!(!state.release(id.wrapping_add(7)));
        assert!(state.release(id));
        assert!(state.register().is_some());
    }

    #[test]
    fn release_applies_to_a_confirmed_request_too() {
        // A caller dropped after confirmation still has to free the slot.
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();
        state.complete(id, true);
        assert!(state.release(id));
        assert!(state.register().is_some());
    }

    #[test]
    fn stop_during_startup_yields_the_waiter() {
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();
        match state.take() {
            Advertising::Starting(taken, _tx) => assert_eq!(taken, id),
            _ => panic!("stop must surface the in-flight start so it can be woken"),
        }
        assert!(
            state.register().is_some(),
            "a stop during startup must not block later starts"
        );
    }

    #[test]
    fn a_stop_during_startup_leaves_the_start_unable_to_prove_ownership() {
        // The shape of the Android advertising race: a stop can take the slot
        // while the start it displaced has not yet reached the platform, and
        // that start can go on to begin advertising afterwards. Its cleanup
        // therefore cannot ask "do I still own the slot?" before tearing the
        // request down -- the answer is always no, and the radio would be left
        // advertising with the state machine saying `Idle`. Cancellation has
        // to be keyed on the request id, which survives here regardless.
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();

        let displaced = state.take();
        assert_eq!(
            displaced.request_id(),
            Some(id),
            "stop must name what it displaced"
        );

        assert!(
            !state.release(id),
            "the stop already freed the slot, so the start cannot claim ownership"
        );
    }

    #[test]
    fn a_stop_names_the_request_it_displaces_in_every_state() {
        // Stop reaches the platform qualified by this id, so that an older
        // stop cannot tear down a newer start that claimed the advertiser
        // after the slot was freed.
        let mut state = AdvertiseState::default();
        assert_eq!(
            state.take().request_id(),
            None,
            "nothing registered, nothing to stop"
        );

        let (starting, _rx) = state.register().unwrap();
        assert_eq!(state.take().request_id(), Some(starting));

        let (active, _rx) = state.register().unwrap();
        state.complete(active, true).expect("waiter");
        assert_eq!(state.take().request_id(), Some(active));
    }

    #[test]
    fn a_start_confirmed_after_a_stop_does_not_reclaim_the_slot() {
        // The regression this state machine exists for: with a separate
        // `active` flag, a stop landing between the wake-up and the flag being
        // set was undone by the resuming task, leaving the slot permanently
        // claimed while the radio was idle.
        let mut state = AdvertiseState::default();
        let (id, _rx) = state.register().unwrap();

        // Verdict arrives and moves the slot to Active...
        state.complete(id, true).expect("waiter");
        // ...then a stop takes it.
        assert_eq!(state.take().request_id(), Some(id));

        // Anything the resuming task does with its own id must not resurrect it.
        assert!(!state.release(id));
        assert!(state.complete(id, true).is_none());
        assert!(
            state.register().is_some(),
            "the slot must still be free after a stop"
        );
    }
}
