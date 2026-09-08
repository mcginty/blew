//! Generation-tagged connect attempts.
//!
//! Lives outside `platform::android` so it can be tested on every host rather
//! than only on a device. Android reports a GATT callback by device address
//! alone, so a retired attempt's late callback is indistinguishable from the
//! live one's unless every attempt carries an identity of its own. The
//! generation is that identity: it travels to Kotlin with the connect request
//! and comes back on every completion, and a completion naming an attempt that
//! no longer owns the address is dropped rather than applied to its successor.

use std::collections::HashMap;

use tokio::sync::oneshot;

use crate::error::BlewResult;

type Waiter = oneshot::Sender<BlewResult<()>>;

/// Generation meaning "whichever attempt is live for this address", for
/// callers with no particular attempt in mind. Matches
/// `BleCentralManager.ANY_GENERATION`; never handed out by
/// [`ConnectAttempts::begin`], so it matches nothing in [`ConnectAttempts`]
/// itself.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) const ANY_GENERATION: i32 = 0;

struct Attempt {
    generation: i32,
    waiter: Waiter,
}

/// The connect attempt in flight per device address, at most one each.
#[derive(Default)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct ConnectAttempts {
    next_generation: i32,
    pending: HashMap<String, Attempt>,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl ConnectAttempts {
    /// Claim the connect slot for `addr`, returning the attempt's generation
    /// and its waiter.
    ///
    /// `None` when a connect is already in flight there. Deliberately not
    /// "latest wins": evicting the first caller's waiter silently orphans it.
    pub(crate) fn begin(&mut self, addr: &str) -> Option<(i32, oneshot::Receiver<BlewResult<()>>)> {
        if self.pending.contains_key(addr) {
            return None;
        }
        let generation = self.next_generation();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(
            addr.to_owned(),
            Attempt {
                generation,
                waiter: tx,
            },
        );
        Some((generation, rx))
    }

    /// Take the waiter for `addr`, but only if it belongs to `generation`.
    ///
    /// A completion naming a retired attempt leaves the live one untouched.
    /// The failure this exists for is a superseded attempt's late disconnect
    /// resolving its replacement's `connect()` as if the link had dropped.
    pub(crate) fn complete(&mut self, addr: &str, generation: i32) -> Option<Waiter> {
        if self.pending.get(addr)?.generation != generation {
            return None;
        }
        self.pending.remove(addr).map(|attempt| attempt.waiter)
    }

    fn next_generation(&mut self) -> i32 {
        // Skipped rather than allowed to land on ANY_GENERATION, which would
        // make one attempt in every wrap of the counter unaddressable.
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == ANY_GENERATION {
            self.next_generation = 1;
        }
        self.next_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BlewError;
    use crate::types::DeviceId;

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "11:22:33:44:55:66";

    fn disconnected() -> BlewResult<()> {
        Err(BlewError::NotConnected(DeviceId::from(A)))
    }

    #[test]
    fn an_overlapping_connect_is_refused() {
        let mut state = ConnectAttempts::default();
        let (_gen, _rx) = state.begin(A).expect("first connect claims the slot");
        assert!(
            state.begin(A).is_none(),
            "latest-wins would orphan the first"
        );
    }

    #[test]
    fn different_addresses_are_independent() {
        let mut state = ConnectAttempts::default();
        let (first, _rx) = state.begin(A).unwrap();
        let (second, _rx) = state.begin(B).unwrap();
        assert_ne!(first, second, "generations are global, not per address");
    }

    #[test]
    fn generations_are_unique_and_never_the_wildcard() {
        let mut state = ConnectAttempts::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let (generation, _rx) = state.begin(A).unwrap();
            assert_ne!(generation, ANY_GENERATION);
            assert!(!seen.contains(&generation));
            seen.push(generation);
            state.complete(A, generation).expect("waiter");
        }
    }

    #[test]
    fn a_completion_reaches_its_own_attempt() {
        let mut state = ConnectAttempts::default();
        let (generation, rx) = state.begin(A).unwrap();
        state
            .complete(A, generation)
            .expect("waiter")
            .send(Ok(()))
            .expect("receiver still live");
        assert!(rx.blocking_recv().expect("delivered").is_ok());
    }

    #[test]
    fn a_retired_attempts_disconnect_does_not_resolve_its_replacement() {
        // Retire attempt A, start B to the same address, then deliver A's
        // delayed disconnect. Address-keyed completion resolved B's caller as
        // disconnected while B's connection was still coming up.
        let mut state = ConnectAttempts::default();
        let (retired, _rx) = state.begin(A).unwrap();
        state
            .complete(A, retired)
            .expect("timeout takes the waiter");

        let (live, mut live_rx) = state.begin(A).unwrap();
        assert_ne!(retired, live);

        assert!(
            state.complete(A, retired).is_none(),
            "the retired attempt's callback must not take the live waiter"
        );
        assert!(
            live_rx.try_recv().is_err(),
            "the live connect is still waiting on its own callback"
        );

        // ...and the live attempt still completes normally afterwards.
        state
            .complete(A, live)
            .expect("waiter")
            .send(disconnected())
            .expect("receiver still live");
        assert!(live_rx.blocking_recv().expect("delivered").is_err());
    }

    #[test]
    fn the_wildcard_generation_resolves_nothing() {
        // Kotlin reports ANY_GENERATION when a force-close found no attempt to
        // tear down. That must not be read as "matches whatever is pending".
        let mut state = ConnectAttempts::default();
        let (_generation, _rx) = state.begin(A).unwrap();
        assert!(state.complete(A, ANY_GENERATION).is_none());
    }

    #[test]
    fn a_completion_for_an_unknown_address_is_dropped() {
        let mut state = ConnectAttempts::default();
        let (generation, _rx) = state.begin(A).unwrap();
        assert!(state.complete(B, generation).is_none());
    }
}
