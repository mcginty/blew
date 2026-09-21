//! Apple (macOS / iOS) implementation of [`PeripheralBackend`].
//!
//! Architecture:
//! - A dedicated GCD serial queue receives all `CBPeripheralManager` delegate callbacks.
//! - GATT service/characteristic mutable objects are retained so we can push
//!   notifications and respond to read/write requests.
//! - RAII [`ReadResponder`] / [`WriteResponder`] carry ATT responses back to the
//!   CB queue via Tokio oneshot channels + background tasks.

#![allow(
    non_snake_case,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    unsafe_op_in_unsafe_fn
)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use futures_core::Stream;
use objc2::define_class;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass};
#[cfg(target_os = "ios")]
use objc2_core_bluetooth::CBPeripheralManagerOptionRestoreIdentifierKey;
use objc2_core_bluetooth::CBPeripheralManagerRestoredStateServicesKey;
use objc2_core_bluetooth::{
    CBATTError, CBATTRequest, CBAdvertisementDataLocalNameKey, CBAdvertisementDataServiceUUIDsKey,
    CBAttributePermissions, CBCentral, CBCharacteristic, CBCharacteristicProperties,
    CBL2CAPChannel, CBL2CAPPSM, CBManagerState, CBMutableCharacteristic, CBMutableService,
    CBPeripheralManager, CBPeripheralManagerConnectionLatency, CBPeripheralManagerDelegate,
    CBService, CBUUID,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSError, NSObjectProtocol, NSString};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use tracing::{debug, trace, warn};

use crate::error::{BlewError, BlewResult};
use crate::gatt::props::{AttributePermissions, CharacteristicProperties};
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, L2capEncryption, types::Psm};
use crate::peripheral::backend::{self, PeripheralBackend};
use crate::peripheral::types::{
    AdvertisingConfig, Delivery, PeripheralConfig, PeripheralRequest, PeripheralStateEvent,
    ReadResponder, WriteResponder,
};
use crate::platform::apple::helpers::{
    ObjcSend, cbuuid_to_uuid, central_device_id, retain_send, uuid_to_cbuuid,
};
use crate::platform::apple::l2cap::bridge_l2cap_channel;
use crate::types::DeviceId;
use crate::util::BroadcastEventStream;
use crate::util::callback_slots::{
    Answer, CallbackSlots, Turn, answer_channel, await_answer, submit,
};

/// How long a [`request_in_turn`] waits for CoreBluetooth's answer, which
/// normally takes milliseconds.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Characteristics of one service, keyed by UUID, as `chars` holds them.
type CharMap = HashMap<Uuid, ObjcSend<CBMutableCharacteristic>>;

/// CoreBluetooth ignores a command issued in any other state and never answers it.
fn powered_on(manager: &CBPeripheralManager) -> bool {
    let state = unsafe { manager.state() };
    state == CBManagerState::PoweredOn
}

fn our_props_to_cb(props: CharacteristicProperties) -> CBCharacteristicProperties {
    let mut out = CBCharacteristicProperties(0);
    if props.contains(CharacteristicProperties::BROADCAST) {
        out |= CBCharacteristicProperties::Broadcast;
    }
    if props.contains(CharacteristicProperties::READ) {
        out |= CBCharacteristicProperties::Read;
    }
    if props.contains(CharacteristicProperties::WRITE_WITHOUT_RESPONSE) {
        out |= CBCharacteristicProperties::WriteWithoutResponse;
    }
    if props.contains(CharacteristicProperties::WRITE) {
        out |= CBCharacteristicProperties::Write;
    }
    if props.contains(CharacteristicProperties::NOTIFY) {
        out |= CBCharacteristicProperties::Notify;
    }
    if props.contains(CharacteristicProperties::INDICATE) {
        out |= CBCharacteristicProperties::Indicate;
    }
    out
}

fn our_perms_to_cb(perms: AttributePermissions) -> CBAttributePermissions {
    let mut out = CBAttributePermissions(0);
    if perms.contains(AttributePermissions::READ) {
        out |= CBAttributePermissions::Readable;
    }
    if perms.contains(AttributePermissions::WRITE) {
        out |= CBAttributePermissions::Writeable;
    }
    if perms.contains(AttributePermissions::READ_ENCRYPTED) {
        out |= CBAttributePermissions::ReadEncryptionRequired;
    }
    if perms.contains(AttributePermissions::WRITE_ENCRYPTED) {
        out |= CBAttributePermissions::WriteEncryptionRequired;
    }
    out
}

/// The `encryptionRequired:` argument for `publishL2CAPChannelWithEncryption:`.
///
/// CoreBluetooth offers one bit, so `RequireAuthentication` has no expressible
/// form: publishing with encryption demands an encrypted link but says nothing
/// about whether the pairing that produced the key was MITM-protected.
fn publish_encryption_flag(encryption: L2capEncryption) -> BlewResult<bool> {
    match encryption {
        L2capEncryption::Insecure => Ok(false),
        L2capEncryption::RequireEncryption => Ok(true),
        other => Err(BlewError::L2capEncryptionUnsupported {
            requested: other,
            reason: "publishL2CAPChannelWithEncryption: is a single boolean and \
                     cannot demand an authenticated pairing",
        }),
    }
}

/// A notification CoreBluetooth refused because its transmit queue was full.
struct PendingNotify {
    device_id: DeviceId,
    char_uuid: Uuid,
    value: Vec<u8>,
    done: oneshot::Sender<BlewResult<Delivery>>,
}

/// Result of one `updateValue:forCharacteristic:onSubscribedCentrals:` attempt.
enum NotifyOutcome {
    /// CoreBluetooth accepted the value into its transmit queue.
    Sent,
    /// The target central is no longer subscribed — treated as a no-op, matching
    /// the pre-existing behaviour for a subscriber that disappeared mid-call.
    SubscriberGone,
    /// No local `CBMutableCharacteristic` is registered under that UUID.
    CharNotFound,
    /// The transmit queue is full. CoreBluetooth will call
    /// `peripheralManagerIsReadyToUpdateSubscribers:` when space frees up.
    QueueFull,
}

impl NotifyOutcome {
    /// The caller-facing result of an attempt that did not hit a full queue.
    ///
    /// Acceptance into the transmit queue is all CoreBluetooth reports, even for
    /// an indication, so a sent value is never [`Delivery::Confirmed`].
    fn delivery(&self, char_uuid: Uuid) -> BlewResult<Delivery> {
        match self {
            NotifyOutcome::Sent | NotifyOutcome::QueueFull => Ok(Delivery::Sent),
            NotifyOutcome::SubscriberGone => Ok(Delivery::NoSubscriber),
            NotifyOutcome::CharNotFound => {
                Err(BlewError::LocalCharacteristicNotFound { char_uuid })
            }
        }
    }
}

struct PeripheralInner {
    /// `CBMutableCharacteristic` objects keyed by UUID, for notification sending.
    chars: Mutex<HashMap<Uuid, ObjcSend<CBMutableCharacteristic>>>,
    /// Retained `CBCentral` handles, keyed by (characteristic UUID, device id),
    /// populated by `didSubscribeToCharacteristic` and cleared by
    /// `didUnsubscribeFromCharacteristic`. Used by `notify_characteristic` to
    /// target a single central rather than broadcasting.
    subscribers: Mutex<HashMap<Uuid, HashMap<DeviceId, ObjcSend<CBCentral>>>>,
    /// The `start_advertising()` still owed `didStartAdvertising:error:`.
    adv: Mutex<CallbackSlots<(), (), ()>>,
    /// Each `add_service()` still owed `didAddService:error:`, keyed by service
    /// UUID, carrying the characteristics that join `chars` once it is added.
    add_svc: Mutex<CallbackSlots<Uuid, CharMap, ()>>,
    /// Inbound GATT requests. The receiver is handed out at most once via
    /// [`PeripheralBackend::take_requests`].
    request_tx: mpsc::UnboundedSender<PeripheralRequest>,
    request_rx: Mutex<Option<mpsc::UnboundedReceiver<PeripheralRequest>>>,
    /// Broadcast sender for clone-able state events (adapter power, subscription changes).
    state_tx: broadcast::Sender<PeripheralStateEvent>,
    /// Populated once by `willRestoreState:`; drained exactly once via
    /// [`PeripheralBackend::take_restored`]. Buffered so callers can observe the
    /// restored service list after construction returns — broadcast delivery
    /// would race the callback.
    restored: Mutex<Option<Vec<Uuid>>>,
    /// Powered state watch.
    powered_tx: watch::Sender<bool>,
    /// Result of `publishL2CAPChannelWithEncryption` -- carries the assigned PSM.
    l2cap_config: Mutex<crate::l2cap::L2capConfig>,
    /// The `l2cap_listener()` still owed `didPublishL2CAPChannel:error:`.
    l2cap_publish: Mutex<CallbackSlots<(), (), Psm>>,
    /// Sender for incoming L2CAP channels (set by `l2cap_listener`). Unbounded so
    /// the GCD delegate queue is never blocked by a slow accept-stream consumer.
    #[allow(clippy::type_complexity)]
    l2cap_channel_tx: Mutex<Option<mpsc::UnboundedSender<BlewResult<(DeviceId, L2capChannel)>>>>,
    /// Notifications CoreBluetooth refused for lack of transmit-queue space,
    /// retried in FIFO order from `peripheralManagerIsReadyToUpdateSubscribers:`.
    ///
    /// This lock is held across the `updateValue:` call on both the enqueue and
    /// the drain path. That is deliberate: CoreBluetooth only signals readiness
    /// after an update has failed, so an attempt that raced the readiness
    /// callback and enqueued *after* the drain had already run would never be
    /// retried. Serialising attempt-and-enqueue against drain closes that
    /// window. Lock order is always `pending_notifies` -> `chars` ->
    /// `subscribers`.
    pending_notifies: Mutex<VecDeque<PendingNotify>>,
    /// Tokio runtime handle, captured at construction time so GCD callbacks
    /// (which run off the Tokio thread) can spawn tasks onto the runtime.
    runtime: Handle,
}

impl PeripheralInner {
    fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (powered_tx, powered_rx) = watch::channel(false);
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (state_tx, _) = broadcast::channel(256);
        let inner = Arc::new(Self {
            chars: Default::default(),
            subscribers: Default::default(),
            adv: Mutex::default(),
            add_svc: Mutex::default(),
            request_tx,
            request_rx: Mutex::new(Some(request_rx)),
            state_tx,
            restored: Mutex::new(None),
            powered_tx,
            l2cap_config: Mutex::new(crate::l2cap::L2capConfig::default()),
            l2cap_publish: Mutex::default(),
            l2cap_channel_tx: Mutex::new(None),
            pending_notifies: Mutex::new(VecDeque::new()),
            runtime: Handle::current(),
        });
        (inner, powered_rx)
    }

    fn emit_state(&self, event: PeripheralStateEvent) {
        let _ = self.state_tx.send(event);
    }

    /// Drop what a state below `PoweredOn` invalidates, before the adapter
    /// event goes out.
    ///
    /// `CBPeripheralManager.h` documents two depths: below `PoweredOn`
    /// advertising pauses and every central disconnects; only below
    /// `PoweredOff` is the local database cleared. `chars` mirrors that
    /// database, so it goes only then -- clearing it on a plain power-off
    /// would fail notifications on services CoreBluetooth kept.
    fn power_down(&self, state: CBManagerState) {
        let database_cleared = state.0 < CBManagerState::PoweredOff.0;

        // CoreBluetooth answers none of these once it leaves `PoweredOn`.
        let adds = self.add_svc.lock().drain();
        for pending in adds {
            pending.answer(Err(BlewError::NotPowered));
        }
        let starts = self.adv.lock().drain();
        for pending in starts {
            pending.answer(Err(BlewError::NotPowered));
        }
        let publishes = self.l2cap_publish.lock().drain();
        for pending in publishes {
            pending.answer(Err(BlewError::NotPowered));
        }
        // Whether a PSM survives a power-off is undocumented; ending the stream
        // tells its consumer to publish again rather than wait forever.
        if let Some(tx) = self.l2cap_channel_tx.lock().take() {
            let _ = tx.send(Err(BlewError::NotPowered));
        }

        // Held throughout, so no notification queues behind a dropped subscriber.
        let lost: Vec<(DeviceId, Uuid)> = {
            let mut queue = self.pending_notifies.lock();
            for pending in queue.drain(..) {
                let _ = pending.done.send(Err(BlewError::NotPowered));
            }
            if database_cleared {
                self.chars.lock().clear();
            }
            self.subscribers
                .lock()
                .drain()
                .flat_map(|(char_uuid, centrals)| {
                    centrals
                        .into_keys()
                        .map(move |client_id| (client_id, char_uuid))
                })
                .collect()
        };
        for (client_id, char_uuid) in lost {
            self.emit_state(PeripheralStateEvent::SubscriptionChanged {
                client_id,
                char_uuid,
                subscribed: false,
            });
        }
    }

    /// Settle the `add_service` that `didAddService:error:` answers.
    fn service_added(&self, svc_uuid: Uuid, error: Option<String>) {
        let pending = self.add_svc.lock().take(&svc_uuid);
        let Some(pending) = pending else { return };
        let (chars, answer) = pending.split();
        let result = if let Some(e) = error {
            warn!(service_uuid = %svc_uuid, error = %e, "failed to add GATT service");
            Err(BlewError::Internal(e))
        } else {
            debug!(service_uuid = %svc_uuid, "GATT service added");
            // Before the caller wakes, so it can notify at once. Even if it gave
            // up, the service is in the database.
            self.chars.lock().extend(chars);
            Ok(())
        };
        if !answer.send(result) {
            debug!(service_uuid = %svc_uuid, "service result arrived after its add gave up waiting");
        }
    }

    /// One `updateValue:forCharacteristic:onSubscribedCentrals:` attempt.
    ///
    /// Callers must already hold the `pending_notifies` lock — see the field
    /// docs for why.
    fn try_update_value(
        &self,
        manager: &CBPeripheralManager,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: &[u8],
    ) -> NotifyOutcome {
        let cb_char = {
            let lock = self.chars.lock();
            lock.get(&char_uuid).map(|c| unsafe { retain_send(&**c) })
        };
        let Some(cb_char) = cb_char else {
            return NotifyOutcome::CharNotFound;
        };

        let cb_central = {
            let lock = self.subscribers.lock();
            lock.get(&char_uuid)
                .and_then(|m| m.get(device_id))
                .map(|c| unsafe { retain_send(&**c) })
        };
        let Some(cb_central) = cb_central else {
            return NotifyOutcome::SubscriberGone;
        };

        let data = NSData::from_vec(value.to_vec());
        let centrals = NSArray::from_slice(&[cb_central.0.as_ref()]);
        let accepted = unsafe {
            manager.updateValue_forCharacteristic_onSubscribedCentrals(
                &data,
                &cb_char.0,
                Some(&centrals),
            )
        };
        if accepted {
            NotifyOutcome::Sent
        } else {
            NotifyOutcome::QueueFull
        }
    }

    /// Retry queued notifications until one is refused again or the queue empties.
    fn drain_pending_notifies(&self, manager: &CBPeripheralManager) {
        let mut queue = self.pending_notifies.lock();
        while let Some(pending) = queue.front() {
            let outcome = self.try_update_value(
                manager,
                &pending.device_id,
                pending.char_uuid,
                &pending.value,
            );
            if matches!(outcome, NotifyOutcome::QueueFull) {
                break;
            }
            let pending = queue.pop_front().expect("front was just observed");
            let _ = pending.done.send(outcome.delivery(pending.char_uuid));
        }
    }

    fn emit_request(&self, request: PeripheralRequest) {
        let _ = self.request_tx.send(request);
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements.
    #[unsafe(super(NSObject))]
    #[name = "BlewPeripheralDelegate"]
    #[ivars = Arc<PeripheralInner>]
    struct PeripheralDelegate;

    unsafe impl NSObjectProtocol for PeripheralDelegate {}

    unsafe impl CBPeripheralManagerDelegate for PeripheralDelegate {
        #[unsafe(method(peripheralManagerDidUpdateState:))]
        unsafe fn peripheralManagerDidUpdateState(&self, peripheral: &CBPeripheralManager) {
            let state = unsafe { peripheral.state() };
            let powered = state == CBManagerState::PoweredOn;
            debug!(powered, state = state.0, "peripheral adapter state changed");
            let inner = self.ivars();
            if !powered {
                inner.power_down(state);
            }
            let _ = inner.powered_tx.send(powered);
            inner.emit_state(PeripheralStateEvent::AdapterStateChanged { powered });
        }

        #[unsafe(method(peripheralManager:willRestoreState:))]
        unsafe fn peripheralManager_willRestoreState(
            &self,
            _peripheral: &CBPeripheralManager,
            dict: &NSDictionary<NSString, AnyObject>,
        ) {
            let inner = self.ivars();
            let key = unsafe { CBPeripheralManagerRestoredStateServicesKey };
            let Some(obj) = dict.objectForKey(key) else {
                *inner.restored.lock() = Some(Vec::new());
                return;
            };
            // SAFETY: CoreBluetooth guarantees this key's value is NSArray<CBMutableService>.
            let services: Retained<NSArray<CBMutableService>> = Retained::cast_unchecked(obj);
            let mut restored_uuids = Vec::new();
            {
                let mut chars_lock = inner.chars.lock();
                for service in services.to_vec() {
                    let svc_uuid_ret = service.UUID();
                    let Some(svc_uuid) = cbuuid_to_uuid(&svc_uuid_ret) else {
                        continue;
                    };
                    if let Some(chars) = unsafe { service.characteristics() } {
                        for ch in chars.to_vec() {
                            let ch_uuid_ret = ch.UUID();
                            let Some(ch_uuid) = cbuuid_to_uuid(&ch_uuid_ret) else {
                                continue;
                            };
                            // SAFETY: services we publish are always built from CBMutableCharacteristic,
                            // so the runtime class matches.
                            let mutable: &CBMutableCharacteristic =
                                unsafe { &*(&raw const *ch).cast() };
                            chars_lock.insert(ch_uuid, unsafe { retain_send(mutable) });
                        }
                    }
                    restored_uuids.push(svc_uuid);
                }
            }
            debug!(
                count = restored_uuids.len(),
                "OS-level state restoration recovered peripheral services"
            );
            *inner.restored.lock() = Some(restored_uuids);
        }

        #[unsafe(method(peripheralManagerDidStartAdvertising:error:))]
        unsafe fn peripheralManagerDidStartAdvertising_error(
            &self,
            _peripheral: &CBPeripheralManager,
            error: Option<&NSError>,
        ) {
            let inner = self.ivars();
            let pending = inner.adv.lock().take(&());
            let Some(pending) = pending else { return };
            let result = error.map_or_else(
                || {
                    debug!("advertising started");
                    Ok(())
                },
                |e| {
                    warn!(error = %e.localizedDescription(), "advertising failed to start");
                    Err(BlewError::Internal(e.localizedDescription().to_string()))
                },
            );
            if !pending.answer(result) {
                debug!("advertising result arrived after its start gave up waiting");
            }
        }

        #[unsafe(method(peripheralManager:didAddService:error:))]
        unsafe fn peripheralManager_didAddService_error(
            &self,
            _peripheral: &CBPeripheralManager,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            let inner = self.ivars();
            let svc_uuid_ret = service.UUID();
            let Some(svc_uuid) = cbuuid_to_uuid(&svc_uuid_ret) else {
                return;
            };
            inner.service_added(svc_uuid, error.map(|e| e.localizedDescription().to_string()));
        }

        #[unsafe(method(peripheralManager:central:didSubscribeToCharacteristic:))]
        unsafe fn peripheralManager_central_didSubscribeToCharacteristic(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            characteristic: &CBCharacteristic,
        ) {
            let inner = self.ivars();
            let char_uuid_ret = characteristic.UUID();
            let Some(char_uuid) = cbuuid_to_uuid(&char_uuid_ret) else {
                return;
            };
            let client_id = central_device_id(central);
            trace!(client_id = %client_id, %char_uuid, "client subscribed to characteristic");
            {
                let mut subs = inner.subscribers.lock();
                let entry = subs.entry(char_uuid).or_default();
                entry.insert(client_id.clone(), unsafe { retain_send(central) });
            }
            inner.emit_state(PeripheralStateEvent::SubscriptionChanged {
                client_id,
                char_uuid,
                subscribed: true,
            });
        }

        #[unsafe(method(peripheralManager:central:didUnsubscribeFromCharacteristic:))]
        unsafe fn peripheralManager_central_didUnsubscribeFromCharacteristic(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            characteristic: &CBCharacteristic,
        ) {
            let inner = self.ivars();
            let char_uuid_ret = characteristic.UUID();
            let Some(char_uuid) = cbuuid_to_uuid(&char_uuid_ret) else {
                return;
            };
            let client_id = central_device_id(central);
            trace!(client_id = %client_id, %char_uuid, "client unsubscribed from characteristic");
            let removed = {
                let mut subs = inner.subscribers.lock();
                let removed = subs
                    .get_mut(&char_uuid)
                    .and_then(|entry| entry.remove(&client_id))
                    .is_some();
                if subs.get(&char_uuid).is_some_and(HashMap::is_empty) {
                    subs.remove(&char_uuid);
                }
                removed
            };
            // `power_down` has already reported the rest.
            if removed {
                inner.emit_state(PeripheralStateEvent::SubscriptionChanged {
                    client_id,
                    char_uuid,
                    subscribed: false,
                });
            }
        }

        #[unsafe(method(peripheralManager:didReceiveReadRequest:))]
        unsafe fn peripheralManager_didReceiveReadRequest(
            &self,
            peripheral: &CBPeripheralManager,
            request: &CBATTRequest,
        ) {
            let inner = self.ivars();
            let req_char = request.characteristic();
            let req_char_uuid = req_char.UUID();
            let Some(char_uuid) = cbuuid_to_uuid(&req_char_uuid) else {
                peripheral.respondToRequest_withResult(request, CBATTError::AttributeNotFound);
                return;
            };

            let service_uuid = req_char
                .service()
                .and_then(|s| { let u = s.UUID(); cbuuid_to_uuid(&u) })
                .unwrap_or(Uuid::nil());

            let req_central = request.central();
            let client_id = central_device_id(&req_central);
            let offset = request.offset() as u16;

            trace!(client_id = %client_id, %char_uuid, offset, "ATT read request");

            let (tx, rx) = oneshot::channel::<Result<Vec<u8>, ()>>();
            let responder = ReadResponder::new(tx);

            inner.emit_request(PeripheralRequest::Read {
                client_id,
                service_uuid,
                char_uuid,
                offset,
                responder,
            });

            // Spawn a task to relay the ATT response back to CoreBluetooth.
            // Must use the captured runtime handle because this callback fires
            // on the GCD queue, outside the Tokio runtime context.
            let request_retained = unsafe { retain_send(request) };
            let manager_retained = unsafe { retain_send(peripheral) };
            inner.runtime.spawn(async move {
                match rx.await {
                    Ok(Ok(data)) => unsafe {
                        let nsdata = NSData::from_vec(data);
                        request_retained.setValue(Some(&nsdata));
                        manager_retained.respondToRequest_withResult(
                            &request_retained,
                            CBATTError::Success,
                        );
                    },
                    _ => unsafe {
                        manager_retained.respondToRequest_withResult(
                            &request_retained,
                            CBATTError::AttributeNotFound,
                        );
                    },
                }
            });
        }

        #[unsafe(method(peripheralManager:didReceiveWriteRequests:))]
        unsafe fn peripheralManager_didReceiveWriteRequests(
            &self,
            peripheral: &CBPeripheralManager,
            requests: &NSArray<CBATTRequest>,
        ) {
            let inner = self.ivars();

            let count = requests.count();
            if count == 0 {
                return;
            }
            // CoreBluetooth batches queued and long (prepared) writes into this
            // array. Exactly one ATT response is owed, and it must name the
            // *first* request -- but every request in the batch carries its own
            // slice of the payload and all of them must be delivered, or a long
            // write silently loses everything past the first fragment.
            let first = requests.objectAtIndex(0);

            let mut waiters = Vec::with_capacity(count);
            for i in 0..count {
                let request = requests.objectAtIndex(i);
                let req_char = request.characteristic();
                let req_char_uuid = req_char.UUID();
                let Some(char_uuid) = cbuuid_to_uuid(&req_char_uuid) else {
                    // One unmappable characteristic invalidates the batch: the
                    // fragments are not independently applicable.
                    peripheral.respondToRequest_withResult(&first, CBATTError::AttributeNotFound);
                    return;
                };

                let service_uuid = req_char
                    .service()
                    .and_then(|s| { let u = s.UUID(); cbuuid_to_uuid(&u) })
                    .unwrap_or(Uuid::nil());

                let req_central = request.central();
                let client_id = central_device_id(&req_central);
                let offset = request.offset() as u16;
                let value = request.value().map(|d| d.to_vec()).unwrap_or_default();

                trace!(
                    client_id = %client_id,
                    %char_uuid,
                    offset,
                    len = value.len(),
                    fragment = i,
                    "ATT write request"
                );

                let (tx, rx) = oneshot::channel::<bool>();
                waiters.push(rx);

                inner.emit_request(PeripheralRequest::Write {
                    client_id,
                    service_uuid,
                    char_uuid,
                    offset,
                    value,
                    responder: Some(WriteResponder::new(tx)),
                });
            }

            let first_retained = unsafe { retain_send(&*first) };
            let manager_retained = unsafe { retain_send(peripheral) };
            inner.runtime.spawn(async move {
                // Await every fragment before responding -- short-circuiting
                // would drop the remaining receivers and strand the app's
                // responders. The batch succeeds only if all fragments do.
                let mut success = true;
                for rx in waiters {
                    success &= rx.await.unwrap_or(false);
                }
                let result = if success {
                    CBATTError::Success
                } else {
                    CBATTError::WriteNotPermitted
                };
                unsafe {
                    manager_retained.respondToRequest_withResult(&first_retained, result);
                };
            });
        }

        /// Fires when transmit-queue space frees up after an
        /// `updateValue:forCharacteristic:onSubscribedCentrals:` returned NO.
        /// Without this, a refused notification would simply be lost.
        #[unsafe(method(peripheralManagerIsReadyToUpdateSubscribers:))]
        unsafe fn peripheralManagerIsReadyToUpdateSubscribers(
            &self,
            peripheral: &CBPeripheralManager,
        ) {
            trace!("peripheral ready to update subscribers; draining queued notifications");
            self.ivars().drain_pending_notifies(peripheral);
        }

        /// Fires when `publishL2CAPChannelWithEncryption` completes.
        /// Delivers the OS-assigned PSM (or an error) to the waiting `l2cap_listener` call.
        #[unsafe(method(peripheralManager:didPublishL2CAPChannel:error:))]
        unsafe fn peripheralManager_didPublishL2CAPChannel_error(
            &self,
            _peripheral: &CBPeripheralManager,
            PSM: CBL2CAPPSM,
            error: Option<&NSError>,
        ) {
            let inner = self.ivars();
            let pending = inner.l2cap_publish.lock().take(&());
            let Some(pending) = pending else { return };
            let result = if let Some(e) = error {
                warn!(error = %e.localizedDescription(), "L2CAP channel publish failed");
                Err(BlewError::Internal(e.localizedDescription().to_string()))
            } else {
                debug!(psm = PSM, "L2CAP channel published");
                Ok(Psm(PSM))
            };
            if !pending.answer(result) {
                debug!(psm = PSM, "L2CAP publish result arrived after its listener gave up waiting");
            }
        }

        /// Fires when a central opens an L2CAP channel to us.
        #[unsafe(method(peripheralManager:didOpenL2CAPChannel:error:))]
        unsafe fn peripheralManager_didOpenL2CAPChannel_error(
            &self,
            manager: &CBPeripheralManager,
            channel: Option<&CBL2CAPChannel>,
            error: Option<&NSError>,
        ) {
            let inner = self.ivars();
            let tx = inner.l2cap_channel_tx.lock().clone();
            let Some(tx) = tx else { return };

            if let Some(e) = error {
                warn!(error = %e.localizedDescription(), "incoming L2CAP channel failed");
                let _ = tx.send(Err(BlewError::Internal(
                    e.localizedDescription().to_string(),
                )));
                return;
            }
            let Some(ch) = channel else { return };
            debug!("incoming L2CAP channel accepted");

            // Request low-latency connection parameters for higher throughput.
            // On the peripheral side the channel peer is always CBCentral.
            let device_id = if let Some(peer) = ch.peer() {
                let central: Retained<CBCentral> = Retained::cast_unchecked(peer);
                manager.setDesiredConnectionLatency_forCentral(
                    CBPeripheralManagerConnectionLatency::Low,
                    &central,
                );
                central_device_id(&central)
            } else {
                DeviceId::from("unknown")
            };

            let config = inner.l2cap_config.lock().clone();
            match bridge_l2cap_channel(ch, &inner.runtime, &config) {
                Ok(l2cap) => {
                    let _ = tx.send(Ok((device_id, l2cap)));
                }
                Err(reason) => {
                    warn!(?reason, "apple L2CAP channel unusable");
                    let _ = tx.send(Err(BlewError::L2cap {
                        source: format!("{reason:?}").into(),
                    }));
                }
            }
        }
    }
);

impl PeripheralDelegate {
    fn new(inner: Arc<PeripheralInner>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(inner);
        unsafe { objc2::msg_send![super(this), init] }
    }
}

struct PeripheralHandle {
    manager: ObjcSend<CBPeripheralManager>,
    /// Held here so the CB manager's weak-ref delegate stays alive.
    _delegate: ObjcSend<PeripheralDelegate>,
    /// The delegate's serial queue; see [`TurnQueue`].
    queue: DispatchRetained<DispatchQueue>,
    inner: Arc<PeripheralInner>,
}

unsafe impl Send for PeripheralHandle {}
unsafe impl Sync for PeripheralHandle {}

/// Where a turn runs: the manager's serial queue, which the delegate -- and so
/// `power_down` -- runs on too.
///
/// A command whose order matters against a callback or another command is
/// issued in a turn here, never from the calling thread. Otherwise a request
/// can pass `submit`'s power check, lose its slot to `power_down`, and still
/// issue its command after power returns, completing a newer request's wait;
/// and a stop can overtake a start still waiting for its turn. Don't close
/// those windows with a lock the delegate queue also takes, held across the
/// command: CoreBluetooth waiting on its own queue would deadlock. Calls that
/// order against nothing queued stay on the calling thread: `updateValue:`
/// (serialized against `power_down` by `pending_notifies`),
/// `respondToRequest:withResult:`, and state reads.
trait TurnQueue {
    fn run(&self, turn: Box<dyn FnOnce() + Send>);
}

impl TurnQueue for DispatchQueue {
    fn run(&self, turn: Box<dyn FnOnce() + Send>) {
        self.exec_async(turn);
    }
}

/// A request CoreBluetooth answers with a delegate callback. `turn` submits it
/// on `queue`; the wait covers the time the turn spends queued.
async fn request_in_turn<T: Send + 'static>(
    queue: &impl TurnQueue,
    turn: impl FnOnce(Answer<T>) -> Turn + Send + 'static,
    timed_out: impl FnOnce() -> BlewError,
) -> BlewResult<T> {
    let (answer, rx) = answer_channel();
    queue.run(Box::new(move || {
        let turn = turn(answer);
        trace!(?turn, "manager turn");
    }));
    await_answer(rx, CALLBACK_TIMEOUT, timed_out).await
}

/// A command CoreBluetooth doesn't answer, issued in a turn on `queue`.
/// Returns once it has been.
async fn issue_in_turn(
    queue: &impl TurnQueue,
    command: impl FnOnce() + Send + 'static,
) -> BlewResult<()> {
    let (issued_tx, issued) = oneshot::channel();
    queue.run(Box::new(move || {
        command();
        let _ = issued_tx.send(());
    }));
    issued
        .await
        .map_err(|_| BlewError::Internal("the manager queue dropped a turn".into()))
}

pub struct ApplePeripheral(Arc<PeripheralHandle>);

impl ApplePeripheral {
    pub async fn with_config(config: PeripheralConfig) -> BlewResult<Self> {
        let (inner, mut powered_rx) = PeripheralInner::new();
        *inner.l2cap_config.lock() = config.l2cap.clone();
        let delegate = PeripheralDelegate::new(Arc::clone(&inner));

        let queue = DispatchQueue::new("blew.peripheral", DispatchQueueAttr::SERIAL);

        // State restoration (CBPeripheralManagerOptionRestoreIdentifierKey) is iOS-only;
        // passing it on macOS causes CoreBluetooth to throw an NSException.
        #[cfg(target_os = "ios")]
        let manager = ObjcSend(unsafe {
            if let Some(ref id) = config.restore_identifier {
                let key: &NSString = CBPeripheralManagerOptionRestoreIdentifierKey;
                let value = NSString::from_str(id);
                let v_any: &AnyObject = &value;
                let options = NSDictionary::from_slices(&[key], &[v_any]);
                CBPeripheralManager::initWithDelegate_queue_options(
                    CBPeripheralManager::alloc(),
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                    Some(&options),
                )
            } else {
                CBPeripheralManager::initWithDelegate_queue(
                    CBPeripheralManager::alloc(),
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                )
            }
        });
        #[cfg(not(target_os = "ios"))]
        let manager = ObjcSend(unsafe {
            CBPeripheralManager::initWithDelegate_queue(
                CBPeripheralManager::alloc(),
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&queue),
            )
        });
        let delegate = ObjcSend(delegate);

        let timeout = tokio::time::sleep(std::time::Duration::from_secs(15));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = powered_rx.changed() => {
                    let state = unsafe { manager.state() };
                    if state == CBManagerState::PoweredOn {
                        break;
                    }
                    if state == CBManagerState::Unsupported
                        || state == CBManagerState::Unauthorized
                    {
                        return Err(BlewError::AdapterNotFound);
                    }
                    // Unknown / Resetting / PoweredOff -> keep waiting
                }
                () = &mut timeout => {
                    if unsafe { manager.state() } == CBManagerState::PoweredOn {
                        break;
                    }
                    return Err(BlewError::NotPowered);
                }
            }
        }

        let handle = Arc::new(PeripheralHandle {
            manager,
            _delegate: delegate,
            queue,
            inner,
        });
        Ok(ApplePeripheral(handle))
    }
}

impl backend::private::Sealed for ApplePeripheral {}

impl PeripheralBackend for ApplePeripheral {
    type StateEvents = BroadcastEventStream<PeripheralStateEvent>;
    type Requests = UnboundedReceiverStream<PeripheralRequest>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        Self::with_config(PeripheralConfig::default()).await
    }

    fn is_powered(&self) -> impl Future<Output = BlewResult<bool>> + Send {
        let handle = Arc::clone(&self.0);
        async move {
            let state = unsafe { handle.manager.state() };
            Ok(state == CBManagerState::PoweredOn)
        }
    }

    fn add_service(&self, service: &GattService) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let service = service.clone();
        async move {
            debug!(service_uuid = %service.uuid, characteristics = service.characteristics.len(), "adding GATT service");
            let (cb_service, char_map) = {
                let svc_uuid = uuid_to_cbuuid(service.uuid);
                let cb_service = unsafe {
                    CBMutableService::initWithType_primary(
                        CBMutableService::alloc(),
                        &svc_uuid,
                        service.primary,
                    )
                };

                let mut cb_chars: Vec<Retained<CBMutableCharacteristic>> = vec![];
                let mut char_map: HashMap<Uuid, ObjcSend<CBMutableCharacteristic>> = HashMap::new();

                for ch in &service.characteristics {
                    let c_uuid = uuid_to_cbuuid(ch.uuid);
                    let props = our_props_to_cb(ch.properties);
                    let perms = our_perms_to_cb(ch.permissions);

                    let value = if ch.value.is_empty() {
                        None
                    } else {
                        Some(NSData::from_vec(ch.value.clone()))
                    };

                    let cb_char = unsafe {
                        CBMutableCharacteristic::initWithType_properties_value_permissions(
                            CBMutableCharacteristic::alloc(),
                            &c_uuid,
                            props,
                            value.as_deref(),
                            perms,
                        )
                    };
                    let retained_char = unsafe { retain_send(&*cb_char) };
                    char_map.insert(ch.uuid, retained_char);
                    cb_chars.push(cb_char);
                }

                let retained_refs: Vec<&CBCharacteristic> = cb_chars
                    .iter()
                    .map(|c| c.as_ref() as &CBCharacteristic)
                    .collect();
                let char_array = NSArray::from_slice(&retained_refs);
                unsafe { cb_service.setCharacteristics(Some(&char_array)) };

                (ObjcSend(cb_service), char_map)
            };
            let (h, key) = (Arc::clone(&handle), service.uuid);
            request_in_turn(
                &*handle.queue,
                move |answer| {
                    submit(
                        &h.inner.add_svc,
                        powered_on(&h.manager),
                        key,
                        char_map,
                        answer,
                        || BlewError::Peripheral {
                            source: "an earlier add_service for this service is still waiting \
                                     on CoreBluetooth to report it added"
                                .into(),
                        },
                        || unsafe { h.manager.addService(&cb_service) },
                    )
                },
                || BlewError::Peripheral {
                    source: "CoreBluetooth never reported the GATT service added".into(),
                },
            )
            .await
        }
    }

    fn start_advertising(
        &self,
        config: &AdvertisingConfig,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let config = config.clone();
        async move {
            if unsafe { handle.manager.isAdvertising() } {
                return Err(BlewError::AlreadyAdvertising);
            }
            debug!(local_name = ?config.local_name, "starting advertising");
            let adv_data = {
                let local_name = config.local_name.name().map(NSString::from_str);

                let service_uuids: Vec<Retained<CBUUID>> = config
                    .service_uuids
                    .iter()
                    .map(|u| uuid_to_cbuuid(*u))
                    .collect();
                let uuid_array = NSArray::from_retained_slice(&service_uuids);

                let mut keys = vec![unsafe { CBAdvertisementDataServiceUUIDsKey }];
                let mut values: Vec<&AnyObject> = vec![&uuid_array];
                if let Some(local_name) = &local_name {
                    keys.push(unsafe { CBAdvertisementDataLocalNameKey });
                    values.push(local_name);
                }

                ObjcSend(NSDictionary::from_slices(&keys, &values))
            };
            let h = Arc::clone(&handle);
            request_in_turn(
                &*handle.queue,
                move |answer| {
                    submit(
                        &h.inner.adv,
                        powered_on(&h.manager),
                        (),
                        (),
                        answer,
                        // A start still owed its answer is advertising, or about to be.
                        || BlewError::AlreadyAdvertising,
                        || unsafe { h.manager.startAdvertising(Some(&adv_data)) },
                    )
                },
                || BlewError::Peripheral {
                    source: "CoreBluetooth never reported whether advertising started".into(),
                },
            )
            .await
        }
    }

    fn stop_advertising(&self) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        async move {
            debug!("stopping advertising");
            let h = Arc::clone(&handle);
            issue_in_turn(&*handle.queue, move || unsafe {
                h.manager.stopAdvertising();
            })
            .await
        }
    }

    fn notify_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
    ) -> impl Future<Output = BlewResult<Delivery>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            trace!(device = %device_id, %char_uuid, len = value.len(), "notifying characteristic");
            // Attempt and enqueue under one lock so a readiness callback cannot
            // slip between them and leave this notification stranded.
            let rx = {
                let mut queue = handle.inner.pending_notifies.lock();
                match handle
                    .inner
                    .try_update_value(&handle.manager, &device_id, char_uuid, &value)
                {
                    NotifyOutcome::QueueFull => {
                        trace!(device = %device_id, %char_uuid, "transmit queue full; queueing notification");
                        let (tx, rx) = oneshot::channel();
                        queue.push_back(PendingNotify {
                            device_id,
                            char_uuid,
                            value,
                            done: tx,
                        });
                        rx
                    }
                    // A subscriber that disappeared between our caller's
                    // decision and now is a no-op, not an error.
                    outcome => return outcome.delivery(char_uuid),
                }
                // `queue` and every ObjC temporary drop here, before the await.
            };
            rx.await.unwrap_or(Err(BlewError::Internal(
                "peripheral dropped before notification could be sent".into(),
            )))
        }
    }

    fn l2cap_listener(
        &self,
    ) -> impl Future<
        Output = BlewResult<(
            Psm,
            impl Stream<Item = BlewResult<(DeviceId, L2capChannel)>> + Send + 'static,
        )>,
    > + Send {
        let handle = Arc::clone(&self.0);
        async move {
            let encryption = handle.inner.l2cap_config.lock().encryption;
            let encrypted = publish_encryption_flag(encryption)?;
            debug!(%encryption, "publishing L2CAP CoC channel");
            let (ch_tx, ch_rx) = mpsc::unbounded_channel::<BlewResult<(DeviceId, L2capChannel)>>();
            let h = Arc::clone(&handle);
            let psm = request_in_turn(
                &*handle.queue,
                move |answer| {
                    submit(
                        &h.inner.l2cap_publish,
                        powered_on(&h.manager),
                        (),
                        (),
                        answer,
                        || BlewError::L2cap {
                            source: "an earlier l2cap_listener is still waiting on \
                                     CoreBluetooth to report its channel published"
                                .into(),
                        },
                        || {
                            // In the publish's turn, so no power-down lands between them.
                            *h.inner.l2cap_channel_tx.lock() = Some(ch_tx);
                            unsafe { h.manager.publishL2CAPChannelWithEncryption(encrypted) };
                        },
                    )
                },
                || BlewError::L2cap {
                    source: "CoreBluetooth never reported the L2CAP channel published".into(),
                },
            )
            .await?;
            debug!(psm = psm.0, "L2CAP listener ready");
            Ok((psm, UnboundedReceiverStream::new(ch_rx)))
        }
    }

    fn state_events(&self) -> Self::StateEvents {
        BroadcastEventStream::new(self.0.inner.state_tx.subscribe())
    }

    fn take_requests(&self) -> Option<Self::Requests> {
        self.0
            .inner
            .request_rx
            .lock()
            .take()
            .map(UnboundedReceiverStream::new)
    }
}

impl ApplePeripheral {
    /// Consume the preserved-services payload captured from
    /// `peripheralManager:willRestoreState:` during `with_config`. Returns
    /// `None` after the first call.
    #[must_use]
    pub fn take_restored(&self) -> Option<Vec<Uuid>> {
        self.0.inner.restored.lock().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::callback_slots::Turn;

    const SVC: Uuid = Uuid::from_u128(0x1111);
    const CHR: Uuid = Uuid::from_u128(0x2222);

    /// A characteristic as `add_service` builds one. Allocating it needs no
    /// manager, radio, or Bluetooth permission.
    fn char_map() -> CharMap {
        let cb_uuid = uuid_to_cbuuid(CHR);
        let ch = unsafe {
            CBMutableCharacteristic::initWithType_properties_value_permissions(
                CBMutableCharacteristic::alloc(),
                &cb_uuid,
                CBCharacteristicProperties::Notify,
                None,
                CBAttributePermissions::Readable,
            )
        };
        HashMap::from([(CHR, ObjcSend(ch))])
    }

    fn published(inner: &PeripheralInner) -> bool {
        inner.chars.lock().contains_key(&CHR)
    }

    fn busy() -> BlewError {
        BlewError::Peripheral {
            source: "busy".into(),
        }
    }

    /// An `add_service` turn with the adapter on and no manager to issue to.
    fn add_turn(inner: &PeripheralInner) -> (Turn, oneshot::Receiver<BlewResult<()>>) {
        let (answer, rx) = answer_channel();
        let turn = submit(&inner.add_svc, true, SVC, char_map(), answer, busy, || {});
        (turn, rx)
    }

    /// A `start_advertising` or `l2cap_listener` turn, likewise.
    fn slot_turn<T>(
        slots: &Mutex<CallbackSlots<(), (), T>>,
    ) -> (Turn, oneshot::Receiver<BlewResult<T>>) {
        let (answer, rx) = answer_channel();
        let turn = submit(slots, true, (), (), answer, busy, || {});
        (turn, rx)
    }

    #[tokio::test]
    async fn a_service_joins_chars_only_once_added() {
        let (inner, _) = PeripheralInner::new();
        let (_, mut rx) = add_turn(&inner);
        assert!(
            !published(&inner),
            "not before CoreBluetooth has the service"
        );

        inner.service_added(SVC, None);

        assert!(rx.try_recv().unwrap().is_ok());
        assert!(published(&inner));
    }

    #[tokio::test]
    async fn a_rejected_service_leaves_nothing_to_notify_on() {
        let (inner, _) = PeripheralInner::new();
        let (_, mut rx) = add_turn(&inner);

        inner.service_added(SVC, Some("refused".into()));

        assert!(matches!(
            rx.try_recv().unwrap(),
            Err(BlewError::Internal(_))
        ));
        assert!(!published(&inner));
    }

    #[tokio::test]
    async fn a_retry_waits_for_the_answer_the_first_attempt_is_owed() {
        let (inner, _) = PeripheralInner::new();
        let (_, abandoned) = add_turn(&inner);
        drop(abandoned);

        assert_eq!(add_turn(&inner).0, Turn::Busy);

        // The late answer is consumed by the attempt it answers. The service
        // is in the database, so its characteristics are published anyway.
        inner.service_added(SVC, None);
        assert!(published(&inner));
        assert_eq!(add_turn(&inner).0, Turn::Issued);
    }

    #[tokio::test]
    async fn a_power_off_fails_every_waiter_and_keeps_the_database() {
        let (inner, _) = PeripheralInner::new();
        inner.chars.lock().extend(char_map());
        let (_, mut add) = add_turn(&inner);
        let (_, mut adv) = slot_turn(&inner.adv);
        let (_, mut publish) = slot_turn(&inner.l2cap_publish);
        let (ch_tx, mut accepts) = mpsc::unbounded_channel();
        *inner.l2cap_channel_tx.lock() = Some(ch_tx);
        let (done, mut notify) = oneshot::channel();
        inner.pending_notifies.lock().push_back(PendingNotify {
            device_id: DeviceId::from("central"),
            char_uuid: CHR,
            value: vec![1],
            done,
        });

        inner.power_down(CBManagerState::PoweredOff);

        assert!(matches!(
            add.try_recv().unwrap(),
            Err(BlewError::NotPowered)
        ));
        assert!(matches!(
            adv.try_recv().unwrap(),
            Err(BlewError::NotPowered)
        ));
        assert!(matches!(
            publish.try_recv().unwrap(),
            Err(BlewError::NotPowered)
        ));
        assert!(matches!(
            notify.try_recv().unwrap(),
            Err(BlewError::NotPowered)
        ));
        assert!(inner.pending_notifies.lock().is_empty());
        // The accept stream says why, then ends.
        assert!(matches!(
            accepts.recv().await,
            Some(Err(BlewError::NotPowered))
        ));
        assert!(accepts.recv().await.is_none());

        // Every slot is free for the requests an application makes next.
        assert_eq!(add_turn(&inner).0, Turn::Issued);
        assert_eq!(slot_turn(&inner.adv).0, Turn::Issued);
        assert_eq!(slot_turn(&inner.l2cap_publish).0, Turn::Issued);

        // CoreBluetooth keeps the local database across a power-off.
        assert!(published(&inner));
    }

    #[tokio::test]
    async fn a_reset_also_clears_the_database() {
        for state in [
            CBManagerState::Resetting,
            CBManagerState::Unauthorized,
            CBManagerState::Unsupported,
            CBManagerState::Unknown,
        ] {
            let (inner, _) = PeripheralInner::new();
            inner.chars.lock().extend(char_map());

            inner.power_down(state);

            assert!(!published(&inner), "state {}", state.0);
        }
    }

    /// Pins the residual noted in `util::callback_slots`.
    #[tokio::test]
    async fn an_answer_after_a_power_down_is_unclaimed() {
        let (inner, _) = PeripheralInner::new();
        let (_, _rx) = add_turn(&inner);
        inner.power_down(CBManagerState::PoweredOff);

        inner.service_added(SVC, None);

        assert!(!published(&inner));
    }

    /// A manager queue whose turns run only when told to.
    #[derive(Default)]
    struct FakeQueue(Mutex<VecDeque<Box<dyn FnOnce() + Send>>>);

    impl TurnQueue for FakeQueue {
        fn run(&self, turn: Box<dyn FnOnce() + Send>) {
            self.0.lock().push_back(turn);
        }
    }

    impl FakeQueue {
        fn run_all(&self) {
            loop {
                let next = self.0.lock().pop_front();
                let Some(turn) = next else { break };
                turn();
            }
        }
    }

    /// See [`TurnQueue`]. Drives the production scheduling, `request_in_turn`
    /// and `issue_in_turn`, with the manager's commands recorded instead.
    #[tokio::test]
    async fn a_stop_waits_for_a_start_queued_before_it() {
        let (inner, _) = PeripheralInner::new();
        let queue = Arc::new(FakeQueue::default());
        let platform = Arc::new(Mutex::new(Vec::new()));

        let start = tokio::spawn({
            let (queue, inner, platform) = (queue.clone(), inner.clone(), platform.clone());
            async move {
                request_in_turn(
                    &*queue,
                    move |answer| {
                        submit(&inner.adv, true, (), (), answer, busy, move || {
                            platform.lock().push("start");
                        })
                    },
                    busy,
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        let stop = tokio::spawn({
            let (queue, platform) = (queue.clone(), platform.clone());
            async move { issue_in_turn(&*queue, move || platform.lock().push("stop")).await }
        });
        tokio::task::yield_now().await;

        assert!(
            platform.lock().is_empty(),
            "nothing is issued before its turn"
        );
        assert!(!stop.is_finished(), "stop returns only once it is issued");

        queue.run_all();

        stop.await.unwrap().unwrap();
        assert_eq!(*platform.lock(), ["start", "stop"], "and so ends stopped");
        assert!(inner.adv.lock().take(&()).unwrap().answer(Ok(())));
        start.await.unwrap().unwrap();
    }

    #[test]
    fn encryption_maps_to_the_publish_flag() {
        assert!(!publish_encryption_flag(L2capEncryption::Insecure).unwrap());
        assert!(publish_encryption_flag(L2capEncryption::RequireEncryption).unwrap());
    }

    #[test]
    fn authentication_is_refused_rather_than_downgraded() {
        // CoreBluetooth can't demand MITM protection, and handing back a
        // merely-encrypted channel would be weaker than what was asked for.
        let err = publish_encryption_flag(L2capEncryption::RequireAuthentication).unwrap_err();
        assert!(matches!(
            err,
            BlewError::L2capEncryptionUnsupported {
                requested: L2capEncryption::RequireAuthentication,
                ..
            }
        ));
    }
}
