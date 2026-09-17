//! One notification send per device, held until the stack reports it.
//!
//! Lives outside `platform::android` so it can be tested on every host. The
//! Android stack takes one value per device until `onNotificationSent`, and
//! that callback names only the device, so a callback belongs to whichever
//! send is registered for the device when it arrives. That is only true while
//! no second send can register before the first one's callback or a
//! disconnect clears it. A send whose caller stopped waiting -- timed out, or
//! dropped -- therefore stays registered as *abandoned*, still holding the
//! device's gate, rather than being withdrawn.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::time::Instant;

use crate::error::{BlewError, BlewResult};
use crate::types::DeviceId;

type Waiter = oneshot::Sender<BlewResult<()>>;

/// What the platform did with a value it was handed.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) enum Handoff<T> {
    /// The stack took the value. Its callback, or a disconnect, resolves the
    /// send, which then returns `T` on success.
    Accepted(T),
    /// The stack didn't take the value, so no callback will follow.
    Declined(BlewResult<T>),
}

#[derive(Default)]
pub(crate) struct NotifyGates {
    devices: HashMap<String, Device>,
    next_token: u64,
}

struct Device {
    gate: Arc<Semaphore>,
    in_flight: Option<InFlight>,
}

struct InFlight {
    token: u64,
    /// `None` once the caller stopped waiting.
    waiter: Option<Waiter>,
    _permit: OwnedSemaphorePermit,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl NotifyGates {
    fn device(&mut self, addr: &str) -> &mut Device {
        self.devices
            .entry(addr.to_owned())
            .or_insert_with(|| Device {
                gate: Arc::new(Semaphore::new(1)),
                in_flight: None,
            })
    }

    fn gate(&mut self, addr: &str) -> Arc<Semaphore> {
        Arc::clone(&self.device(addr).gate)
    }

    /// Register a send for `addr`. Called before the value is handed to the
    /// platform, whose callback can arrive before the hand-off returns.
    fn begin(
        &mut self,
        addr: &str,
        permit: OwnedSemaphorePermit,
    ) -> (u64, oneshot::Receiver<BlewResult<()>>) {
        let token = self.next_token;
        self.next_token += 1;
        let (tx, rx) = oneshot::channel();
        // The permit guarantees the slot is empty, and that the device wasn't
        // forgotten: the permit holds a clone of its gate.
        self.device(addr).in_flight = Some(InFlight {
            token,
            waiter: Some(tx),
            _permit: permit,
        });
        (token, rx)
    }

    /// Clear a send the platform never took, releasing the gate.
    fn withdraw(&mut self, addr: &str, token: u64) {
        if let Some(device) = self.devices.get_mut(addr)
            && device.in_flight.as_ref().is_some_and(|f| f.token == token)
        {
            device.in_flight = None;
        }
        self.forget_if_idle(addr);
    }

    /// The caller stopped waiting. The send stays registered, holding the gate,
    /// until its callback or a disconnect.
    fn abandon(&mut self, addr: &str, token: u64) {
        if let Some(flight) = self
            .devices
            .get_mut(addr)
            .and_then(|d| d.in_flight.as_mut())
            .filter(|f| f.token == token)
        {
            flight.waiter = None;
        }
    }

    /// The platform reported the send registered for `addr`. An abandoned send
    /// is cleared without a result.
    pub(crate) fn complete(&mut self, addr: &str, result: BlewResult<()>) {
        if let Some(flight) = self.devices.get_mut(addr).and_then(|d| d.in_flight.take())
            && let Some(waiter) = flight.waiter
        {
            let _ = waiter.send(result);
        }
        self.forget_if_idle(addr);
    }

    /// The device disconnected, so no callback will arrive for its send.
    pub(crate) fn disconnect(&mut self, addr: &str) {
        self.complete(
            addr,
            Err(BlewError::DisconnectedDuringOperation(DeviceId::from(addr))),
        );
    }

    /// Forget a device nothing is sending to or waiting on. Clones of the gate
    /// are only taken under the lock this is called under.
    fn forget_if_idle(&mut self, addr: &str) {
        if self
            .devices
            .get(addr)
            .is_some_and(|d| d.in_flight.is_none() && Arc::strong_count(&d.gate) == 1)
        {
            self.devices.remove(addr);
        }
    }
}

/// Abandons the send if the caller stops waiting before it resolves, however it
/// stops: returning on a timeout, or the future being dropped.
struct Registration<'a> {
    gates: &'a Mutex<NotifyGates>,
    addr: &'a str,
    token: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        // A no-op once the send was completed or withdrawn: the token no
        // longer matches anything registered.
        self.gates.lock().abandon(self.addr, self.token);
    }
}

/// Send one value to `addr` through `hand_off`, one send per device at a time.
///
/// `timeout` covers waiting for the gate as well as for the platform's report,
/// so a device whose callback never arrives fails later sends with a timeout
/// rather than hanging them. A send that times out waiting for the gate never
/// reaches the platform and leaves nothing registered.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn send<T>(
    gates: &Mutex<NotifyGates>,
    addr: &str,
    timeout: Duration,
    hand_off: impl FnOnce() -> Handoff<T>,
) -> BlewResult<T> {
    let deadline = Instant::now() + timeout;
    let gate = gates.lock().gate(addr);
    let permit = match tokio::time::timeout_at(deadline, gate.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(BlewError::Internal("notification gate closed".into())),
        Err(_) => {
            gates.lock().forget_if_idle(addr);
            return Err(timed_out(timeout));
        }
    };

    let (token, rx) = gates.lock().begin(addr, permit);
    let registration = Registration { gates, addr, token };

    let value = match hand_off() {
        Handoff::Accepted(value) => value,
        Handoff::Declined(result) => {
            gates.lock().withdraw(addr, token);
            return result;
        }
    };

    let result = match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(result)) => result.map(|()| value),
        Ok(Err(_)) => Err(BlewError::Internal(
            "peripheral shut down before the notification completed".into(),
        )),
        Err(_) => Err(timed_out(timeout)),
    };
    drop(registration);
    result
}

fn timed_out(timeout: Duration) -> BlewError {
    BlewError::Peripheral {
        source: format!("notification did not complete within {timeout:?}").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SHORT: Duration = Duration::from_millis(20);
    const LONG: Duration = Duration::from_secs(5);

    fn gates() -> Arc<Mutex<NotifyGates>> {
        Arc::new(Mutex::new(NotifyGates::default()))
    }

    fn busy(gates: &Mutex<NotifyGates>, addr: &str) -> bool {
        gates
            .lock()
            .devices
            .get(addr)
            .is_some_and(|d| d.in_flight.is_some())
    }

    fn abandoned(gates: &Mutex<NotifyGates>, addr: &str) -> bool {
        gates
            .lock()
            .devices
            .get(addr)
            .and_then(|d| d.in_flight.as_ref())
            .is_some_and(|f| f.waiter.is_none())
    }

    /// Spawn a send that records when it reaches the platform.
    fn spawn_send(
        gates: &Arc<Mutex<NotifyGates>>,
        timeout: Duration,
    ) -> (
        Arc<AtomicBool>,
        tokio::task::JoinHandle<BlewResult<&'static str>>,
    ) {
        let handed_off = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&handed_off);
        let gates = Arc::clone(gates);
        let task = tokio::spawn(async move {
            send(&gates, "A", timeout, move || {
                flag.store(true, Ordering::SeqCst);
                Handoff::Accepted("sent")
            })
            .await
        });
        (handed_off, task)
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn a_timed_out_send_stays_registered_and_keeps_the_gate() {
        let gates = gates();
        let result = send(&gates, "A", SHORT, || Handoff::Accepted(())).await;
        assert!(matches!(result, Err(BlewError::Peripheral { .. })));
        assert!(abandoned(&gates, "A"));
    }

    #[tokio::test]
    async fn after_a_timeout_the_late_callback_frees_the_gate_and_the_next_send_waits_for_its_own()
    {
        let gates = gates();
        send(&gates, "A", SHORT, || Handoff::Accepted(()))
            .await
            .unwrap_err();

        let (handed_off, task) = spawn_send(&gates, LONG);
        settle().await;
        assert!(
            !handed_off.load(Ordering::SeqCst),
            "a second send must not start while the abandoned one is registered"
        );

        // The late callback for the abandoned send.
        gates.lock().complete("A", Ok(()));
        settle().await;
        assert!(handed_off.load(Ordering::SeqCst));
        assert!(
            !task.is_finished(),
            "the old callback must not resolve the new send"
        );

        gates.lock().complete(
            "A",
            Err(BlewError::Internal("the new send's own callback".into())),
        );
        let result = task.await.unwrap();
        assert!(
            matches!(result, Err(BlewError::Internal(m)) if m == "the new send's own callback")
        );
        assert!(!busy(&gates, "A"));
    }

    #[tokio::test]
    async fn after_a_timeout_a_disconnect_frees_the_gate() {
        let gates = gates();
        send(&gates, "A", SHORT, || Handoff::Accepted(()))
            .await
            .unwrap_err();

        gates.lock().disconnect("A");
        assert!(!busy(&gates, "A"));

        let (handed_off, task) = spawn_send(&gates, LONG);
        settle().await;
        assert!(handed_off.load(Ordering::SeqCst));
        gates.lock().complete("A", Ok(()));
        assert_eq!(task.await.unwrap().unwrap(), "sent");
    }

    #[tokio::test]
    async fn a_send_that_times_out_waiting_for_the_gate_sends_and_registers_nothing() {
        let gates = gates();
        send(&gates, "A", SHORT, || Handoff::Accepted(()))
            .await
            .unwrap_err();
        let abandoned_token = gates.lock().devices["A"].in_flight.as_ref().unwrap().token;

        let reached_platform = AtomicBool::new(false);
        let result = send(&gates, "A", SHORT, || {
            reached_platform.store(true, Ordering::SeqCst);
            Handoff::Accepted(())
        })
        .await;
        assert!(matches!(result, Err(BlewError::Peripheral { .. })));
        assert!(!reached_platform.load(Ordering::SeqCst));
        assert_eq!(
            gates.lock().devices["A"].in_flight.as_ref().unwrap().token,
            abandoned_token,
            "the waiting send must not replace the abandoned entry"
        );
    }

    #[tokio::test]
    async fn a_dropped_send_is_abandoned_not_withdrawn() {
        let gates = gates();
        let (_, task) = spawn_send(&gates, LONG);
        settle().await;
        assert!(busy(&gates, "A"));

        task.abort();
        let _ = task.await;
        assert!(abandoned(&gates, "A"));

        let (handed_off, _next) = spawn_send(&gates, LONG);
        settle().await;
        assert!(!handed_off.load(Ordering::SeqCst));
        gates.lock().complete("A", Ok(()));
        settle().await;
        assert!(handed_off.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_declined_send_frees_the_gate_at_once() {
        let gates = gates();
        let result: BlewResult<()> = send(&gates, "A", LONG, || {
            Handoff::Declined(Err(BlewError::NotSupported))
        })
        .await;
        assert!(matches!(result, Err(BlewError::NotSupported)));
        assert!(gates.lock().devices.is_empty());
    }

    #[tokio::test]
    async fn a_disconnect_fails_a_waiting_send() {
        let gates = gates();
        let (_, task) = spawn_send(&gates, LONG);
        settle().await;
        gates.lock().disconnect("A");
        assert!(matches!(
            task.await.unwrap(),
            Err(BlewError::DisconnectedDuringOperation(id)) if id.as_str() == "A"
        ));
    }

    #[tokio::test]
    async fn a_completed_send_leaves_nothing_behind() {
        let gates = gates();
        let (_, task) = spawn_send(&gates, LONG);
        settle().await;
        gates.lock().complete("A", Ok(()));
        assert_eq!(task.await.unwrap().unwrap(), "sent");
        assert!(gates.lock().devices.is_empty());
    }

    #[test]
    fn a_stale_token_cannot_touch_a_newer_send() {
        let mut g = NotifyGates::default();
        let permit = g.gate("A").try_acquire_owned().unwrap();
        let (old, _rx_old) = g.begin("A", permit);
        g.complete("A", Ok(()));

        let permit = g.gate("A").try_acquire_owned().unwrap();
        let (_new, mut rx_new) = g.begin("A", permit);
        g.withdraw("A", old);
        g.abandon("A", old);
        assert!(g.devices["A"].in_flight.as_ref().unwrap().waiter.is_some());
        assert!(g.gate("A").try_acquire_owned().is_err());
        assert!(rx_new.try_recv().is_err());
    }
}
