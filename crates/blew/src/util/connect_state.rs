//! Android connection identity, retained through disconnect and cancellation.

use crate::central::DisconnectCause;
use crate::error::{BlewError, BlewResult};
use crate::types::DeviceId;
use std::collections::HashMap;
use tokio::sync::oneshot;

type Waiter = oneshot::Sender<BlewResult<()>>;

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct ConnectionGuard<F: FnOnce(DisconnectCause)> {
    cleanup: Option<F>,
    cause: DisconnectCause,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl<F: FnOnce(DisconnectCause)> ConnectionGuard<F> {
    pub(crate) fn new(cleanup: F) -> Self {
        Self {
            cleanup: Some(cleanup),
            cause: DisconnectCause::LocalClose,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.cleanup = None;
    }

    pub(crate) fn timed_out(&mut self) {
        self.cause = DisconnectCause::Timeout;
    }
}

impl<F: FnOnce(DisconnectCause)> Drop for ConnectionGuard<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup(self.cause.clone());
        }
    }
}

struct Attempt {
    generation: i32,
    waiter: Option<Waiter>,
    disconnecting: bool,
    disconnects: Vec<oneshot::Sender<()>>,
}

#[derive(Default)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct ConnectAttempts {
    next_generation: i32,
    pending: HashMap<String, Attempt>,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl ConnectAttempts {
    pub(crate) fn begin(&mut self, addr: &str) -> Option<(i32, oneshot::Receiver<BlewResult<()>>)> {
        if self
            .pending
            .get(addr)
            .is_some_and(|a| a.waiter.is_some() || a.disconnecting)
        {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        let generation = self.next_generation;
        let (tx, rx) = oneshot::channel();
        self.pending.insert(
            addr.to_owned(),
            Attempt {
                generation,
                waiter: Some(tx),
                disconnecting: false,
                disconnects: Vec::new(),
            },
        );
        Some((generation, rx))
    }

    pub(crate) fn generation(&self, addr: &str) -> Option<i32> {
        self.pending.get(addr).map(|a| a.generation)
    }

    pub(crate) fn is_live(&self, addr: &str, generation: i32) -> bool {
        self.generation(addr) == Some(generation)
    }

    pub(crate) fn connected(&mut self, addr: &str, generation: i32) -> bool {
        let Some(attempt) = self.pending.get_mut(addr) else {
            return false;
        };
        if attempt.generation != generation || attempt.disconnecting {
            return false;
        }
        let Some(tx) = attempt.waiter.take() else {
            return false;
        };
        let _ = tx.send(Ok(()));
        true
    }

    pub(crate) fn disconnect(&mut self, addr: &str) -> Option<(i32, oneshot::Receiver<()>)> {
        let attempt = self.pending.get_mut(addr)?;
        attempt.disconnecting = true;
        let (tx, rx) = oneshot::channel();
        attempt.disconnects.push(tx);
        Some((attempt.generation, rx))
    }

    pub(crate) fn retire(&mut self, addr: &str, generation: i32) -> bool {
        if !self.is_live(addr, generation) {
            return false;
        }
        let attempt = self.pending.remove(addr).expect("matched attempt");
        if let Some(tx) = attempt.waiter {
            let _ = tx.send(Err(BlewError::NotConnected(DeviceId::from(addr))));
        }
        for tx in attempt.disconnects {
            let _ = tx.send(());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dropping_an_inflight_future_retires_and_closes_its_attempt() {
        use std::sync::{Arc, Mutex};
        let state = Arc::new(Mutex::new(ConnectAttempts::default()));
        let closes = Arc::new(Mutex::new(Vec::new()));
        let (generation, rx) = state.lock().unwrap().begin(A).unwrap();
        let worker_state = state.clone();
        let worker_closes = closes.clone();
        let (started, ready) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = ConnectionGuard::new(move |cause| {
                worker_state.lock().unwrap().retire(A, generation);
                worker_closes.lock().unwrap().push((generation, cause));
            });
            started.send(()).unwrap();
            let _ = rx.await;
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            *closes.lock().unwrap(),
            [(generation, DisconnectCause::LocalClose)]
        );
        assert!(state.lock().unwrap().begin(A).is_some());
    }

    #[test]
    fn timeout_cleanup_is_single_fire_and_success_disarms_it() {
        let mut causes = Vec::new();
        {
            let mut guard = ConnectionGuard::new(|cause| causes.push(cause));
            guard.timed_out();
        }
        {
            let mut guard = ConnectionGuard::new(|cause| causes.push(cause));
            guard.disarm();
        }
        assert_eq!(causes, [DisconnectCause::Timeout]);
    }

    const A: &str = "AA:BB:CC:DD:EE:FF";

    #[test]
    fn overlapping_connect_is_refused() {
        let mut state = ConnectAttempts::default();
        let _first = state.begin(A).unwrap();
        assert!(state.begin(A).is_none());
        assert!(state.begin("other").is_some());
    }

    #[test]
    fn identity_survives_connected_until_disconnect() {
        let mut state = ConnectAttempts::default();
        let (generation, rx) = state.begin(A).unwrap();
        assert!(state.connected(A, generation));
        assert!(rx.blocking_recv().unwrap().is_ok());
        assert_eq!(state.generation(A), Some(generation));
        assert!(
            !state.connected(A, generation),
            "duplicate connection event"
        );
        let (closing, rx) = state.disconnect(A).unwrap();
        assert_eq!(closing, generation);
        assert!(state.begin(A).is_none());
        assert!(state.retire(A, generation));
        assert!(rx.blocking_recv().is_ok());
        assert_eq!(state.generation(A), None);
    }

    #[test]
    fn stale_results_do_not_reach_replacement_or_its_disconnect_waiters() {
        let mut state = ConnectAttempts::default();
        let (old, _) = state.begin(A).unwrap();
        assert!(state.retire(A, old));
        let (new, mut connect_rx) = state.begin(A).unwrap();
        assert!(!state.connected(A, old));
        assert!(!state.retire(A, old));
        assert!(matches!(
            connect_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.connected(A, new));
        let (_, mut disconnect_rx) = state.disconnect(A).unwrap();
        assert!(!state.retire(A, old));
        assert!(matches!(
            disconnect_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.retire(A, new));
        assert!(disconnect_rx.blocking_recv().is_ok());
    }

    #[test]
    fn disconnect_during_mtu_suppresses_connected_and_releases_all_waiters() {
        let mut state = ConnectAttempts::default();
        let (generation, connect_rx) = state.begin(A).unwrap();
        let (_, first) = state.disconnect(A).unwrap();
        let (_, second) = state.disconnect(A).unwrap();
        assert!(!state.connected(A, generation));
        assert!(state.retire(A, generation));
        assert!(connect_rx.blocking_recv().unwrap().is_err());
        assert!(first.blocking_recv().is_ok());
        assert!(second.blocking_recv().is_ok());
    }

    #[test]
    fn generations_wrap_without_reusing_zero() {
        let mut state = ConnectAttempts {
            next_generation: -1,
            ..Default::default()
        };
        assert_eq!(state.begin(A).unwrap().0, 1);
    }
}
