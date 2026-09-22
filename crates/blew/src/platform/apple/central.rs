//! Apple (macOS / iOS) implementation of [`CentralBackend`].
//!
//! Architecture:
//! - A private GCD serial queue receives all CoreBluetooth delegate callbacks.
//! - CoreBluetooth method calls are dispatched from Tokio tasks directly;
//!   CoreBluetooth is thread-safe on macOS 10.15+ / iOS 13+.
//! - `oneshot` channels carry operation results from CB callbacks to async callers.
//! - A `tokio::sync::broadcast` channel fans `CentralEvent` (which is `Clone`) to all subscribers.

#![allow(
    non_snake_case,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    unsafe_op_in_unsafe_fn
)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use bytes::Bytes;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::define_class;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass};
#[cfg(target_os = "ios")]
use objc2_core_bluetooth::CBCentralManagerOptionRestoreIdentifierKey;
use objc2_core_bluetooth::CBCentralManagerRestoredStatePeripheralsKey;
use objc2_core_bluetooth::{
    CBAdvertisementDataLocalNameKey, CBAdvertisementDataManufacturerDataKey,
    CBAdvertisementDataServiceDataKey, CBAdvertisementDataServiceUUIDsKey, CBCentralManager,
    CBCentralManagerDelegate, CBCharacteristic, CBCharacteristicProperties,
    CBCharacteristicWriteType, CBError, CBErrorDomain, CBL2CAPChannel, CBManagerState,
    CBPeripheral, CBPeripheralDelegate, CBService, CBUUID,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSNumber, NSObjectProtocol, NSString,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, broadcast, oneshot, watch};
use uuid::Uuid;

use tracing::{debug, trace, warn};

use crate::central::backend::{self, CentralBackend};
use crate::central::types::{CentralConfig, CentralEvent, DisconnectCause, ScanFilter, WriteType};
use crate::error::{BlewError, BlewResult};
use crate::gatt::props::{AttributePermissions, CharacteristicProperties};
use crate::gatt::service::{GattCharacteristic, GattService};
use crate::l2cap::{L2capChannel, L2capEncryption, types::Psm};
use crate::platform::apple::helpers::{
    ObjcSend, cbuuid_to_uuid, peripheral_device_id, retain_send, uuid_to_cbuuid,
};
use crate::platform::apple::l2cap::bridge_l2cap_channel;
use crate::types::{BleDevice, DeviceId};
use crate::util::BroadcastEventStream;
use crate::util::request_map::KeyedRequestMap;

fn cb_props_to_ours(props: CBCharacteristicProperties) -> CharacteristicProperties {
    use crate::gatt::props::CharacteristicProperties as P;
    let mut out = P::empty();
    if props.contains(CBCharacteristicProperties::Broadcast) {
        out |= P::BROADCAST;
    }
    if props.contains(CBCharacteristicProperties::Read) {
        out |= P::READ;
    }
    if props.contains(CBCharacteristicProperties::WriteWithoutResponse) {
        out |= P::WRITE_WITHOUT_RESPONSE;
    }
    if props.contains(CBCharacteristicProperties::Write) {
        out |= P::WRITE;
    }
    if props.contains(CBCharacteristicProperties::Notify) {
        out |= P::NOTIFY;
    }
    if props.contains(CBCharacteristicProperties::Indicate) {
        out |= P::INDICATE;
    }
    out
}

struct DiscoveryState {
    services: HashMap<Uuid, GattService>,
    pending: usize,
    tx: oneshot::Sender<BlewResult<Vec<GattService>>>,
}

struct CentralInner {
    peripherals: Mutex<HashMap<DeviceId, ObjcSend<CBPeripheral>>>,
    discovered: Mutex<HashMap<DeviceId, BleDevice>>,
    connects: KeyedRequestMap<DeviceId, oneshot::Sender<BlewResult<()>>>,
    connect_timeout: Mutex<Option<std::time::Duration>>,
    l2cap_config: Mutex<crate::l2cap::L2capConfig>,
    // `discoveries` keeps a mutable `DiscoveryState` per device (services
    // accumulate across multiple didDiscoverCharacteristicsForService
    // callbacks), so it needs `get_mut` and can't use KeyedRequestMap.
    discoveries: Mutex<HashMap<DeviceId, DiscoveryState>>,
    reads: KeyedRequestMap<(DeviceId, Uuid), oneshot::Sender<BlewResult<Vec<u8>>>>,
    writes: KeyedRequestMap<(DeviceId, Uuid), oneshot::Sender<BlewResult<()>>>,
    notify_states: KeyedRequestMap<(DeviceId, Uuid), oneshot::Sender<BlewResult<()>>>,
    /// Woken by `peripheralIsReadyToSendWriteWithoutResponse:` and by a
    /// disconnect, for writes waiting on `canSendWriteWithoutResponse`.
    write_ready: Notify,
    /// Held across the readiness check and the write it admits, so two writers
    /// can't both pass one check.
    write_gate: Mutex<()>,
    /// Pending `open_l2cap_channel` results, keyed by device ID.
    l2cap_pendings: KeyedRequestMap<DeviceId, oneshot::Sender<BlewResult<L2capChannel>>>,
    event_tx: broadcast::Sender<CentralEvent>,
    /// Populated once by `willRestoreState:`; drained exactly once via
    /// [`AppleCentral::take_restored`]. Buffered so callers can observe the
    /// restored peripheral list after construction returns — broadcast
    /// delivery would race the callback.
    restored: Mutex<Option<Vec<BleDevice>>>,
    powered_tx: watch::Sender<bool>,
    /// Tokio runtime handle, captured at construction time so GCD callbacks
    /// (which run off the Tokio thread) can spawn tasks onto the runtime.
    runtime: Handle,
}

impl CentralInner {
    fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (event_tx, _) = broadcast::channel(256);
        let (powered_tx, powered_rx) = watch::channel(false);
        let inner = Arc::new(Self {
            peripherals: Default::default(),
            discovered: Default::default(),
            connects: Default::default(),
            connect_timeout: Mutex::new(None),
            l2cap_config: Mutex::new(crate::l2cap::L2capConfig::default()),
            discoveries: Default::default(),
            reads: Default::default(),
            writes: Default::default(),
            notify_states: Default::default(),
            write_ready: Notify::new(),
            write_gate: Mutex::new(()),
            l2cap_pendings: Default::default(),
            event_tx,
            restored: Mutex::new(None),
            powered_tx,
            runtime: Handle::current(),
        });
        (inner, powered_rx)
    }

    fn emit(&self, event: CentralEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Fail every operation still pending on `device_id`.
    ///
    /// CoreBluetooth delivers no completion callback for an in-flight request
    /// when the peer drops — `didUpdateValueForCharacteristic:` and friends
    /// simply never fire again. None of these paths carries its own deadline
    /// (only `connect` does), so without this the awaiting future never
    /// resolves and its entry leaks for the lifetime of the process.
    fn fail_pending(&self, device_id: &DeviceId) {
        if let Some(tx) = self.connects.take(device_id) {
            let _ = tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
        if let Some(ds) = self.discoveries.lock().remove(device_id) {
            let _ = ds.tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
        for (_, tx) in self.reads.take_matching(|(id, _)| id == device_id) {
            let _ = tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
        for (_, tx) in self.writes.take_matching(|(id, _)| id == device_id) {
            let _ = tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
        for (_, tx) in self.notify_states.take_matching(|(id, _)| id == device_id) {
            let _ = tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
        if let Some(tx) = self.l2cap_pendings.take(device_id) {
            let _ = tx.send(Err(BlewError::DisconnectedDuringOperation(
                device_id.clone(),
            )));
        }
    }
}

/// How long a write without response waits for CoreBluetooth to have room for
/// it. Normally one connection event; this bound is only a backstop for a
/// `peripheralIsReadyToSendWriteWithoutResponse:` that never comes.
const WRITE_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Send a write without response once CoreBluetooth has room for it. Sent while
/// `canSendWriteWithoutResponse` is false, delivery is best-effort (the
/// `writeValue:forCharacteristic:type:` header), so the write would be lost
/// with nothing reported.
async fn write_without_response(
    inner: &CentralInner,
    device_id: &DeviceId,
    char_uuid: Uuid,
    value: &[u8],
) -> BlewResult<()> {
    let sent = retry_when_ready(&inner.write_ready, WRITE_READY_TIMEOUT, || {
        try_write_without_response(inner, device_id, char_uuid, value)
    })
    .await?;
    if !sent {
        warn!(%device_id, %char_uuid, "CoreBluetooth never had room for a write without response");
        return Err(BlewError::Gatt {
            device_id: device_id.clone(),
            source: "CoreBluetooth never became ready to send a write without response".into(),
        });
    }
    Ok(())
}

/// Run `attempt` until it succeeds, again each time `ready` is notified.
/// `Ok(false)` once `timeout` passes without success. The wait is created before
/// each attempt: `notify_waiters` reaches every `Notified` that already exists,
/// so a notification landing between a failed attempt and the wait isn't missed.
async fn retry_when_ready(
    ready: &Notify,
    timeout: Duration,
    mut attempt: impl FnMut() -> BlewResult<bool>,
) -> BlewResult<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let notified = ready.notified();
        if attempt()? {
            return Ok(true);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Ok(false);
        }
    }
}

/// One attempt: `false` when CoreBluetooth has no room yet.
fn try_write_without_response(
    inner: &CentralInner,
    device_id: &DeviceId,
    char_uuid: Uuid,
    value: &[u8],
) -> BlewResult<bool> {
    let peripheral = inner
        .peripherals
        .lock()
        .get(device_id)
        .map(|p| unsafe { retain_send(&**p) })
        .ok_or_else(|| BlewError::NotConnected(device_id.clone()))?;
    let characteristic =
        unsafe { find_characteristic(&peripheral, char_uuid) }.ok_or_else(|| {
            BlewError::CharacteristicNotFound {
                device_id: device_id.clone(),
                char_uuid,
            }
        })?;
    let cb_type = CBCharacteristicWriteType::WithoutResponse;
    // Oversized payloads raise NSInvalidArgumentException; see write_characteristic.
    let got = value.len();
    let max = unsafe { peripheral.maximumWriteValueLengthForType(cb_type) };
    if got > max {
        return Err(BlewError::ValueTooLarge { got, max });
    }
    let _gate = inner.write_gate.lock();
    if !unsafe { peripheral.canSendWriteWithoutResponse() } {
        return Ok(false);
    }
    let data = NSData::with_bytes(value);
    unsafe { peripheral.writeValue_forCharacteristic_type(&data, &characteristic, cb_type) };
    Ok(true)
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "BlewCentralDelegate"]
    #[ivars = Arc<CentralInner>]
    struct CentralDelegate;

    unsafe impl NSObjectProtocol for CentralDelegate {}

    unsafe impl CBCentralManagerDelegate for CentralDelegate {
        #[unsafe(method(centralManagerDidUpdateState:))]
        unsafe fn centralManagerDidUpdateState(&self, central: &CBCentralManager) {
            let powered = central.state() == CBManagerState::PoweredOn;
            debug!(powered, "central adapter state changed");
            let inner = self.ivars();
            let _ = inner.powered_tx.send(powered);
            inner.emit(CentralEvent::AdapterStateChanged { powered });
        }

        #[unsafe(method(centralManager:didDiscoverPeripheral:advertisementData:RSSI:))]
        unsafe fn centralManager_didDiscoverPeripheral_advertisementData_RSSI(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            advertisement_data: &NSDictionary<NSString, AnyObject>,
            rssi: &NSNumber,
        ) {
            let id = peripheral_device_id(peripheral);

            // Prefer the advertised local name over the cached peripheral name,
            // which can be stale from a previous connection.
            let name = advertisement_data
                .objectForKey(CBAdvertisementDataLocalNameKey)
                .and_then(|obj| {
                    let ns: &NSString = (*obj).downcast_ref::<NSString>()?;
                    Some(ns.to_string())
                })
                .or_else(|| peripheral.name().map(|s| s.to_string()));

            let services = advertisement_data
                .objectForKey(CBAdvertisementDataServiceUUIDsKey)
                .map(|obj| {
                    // SAFETY: CoreBluetooth guarantees this value is NSArray<CBUUID>.
                    let arr: Retained<NSArray<CBUUID>> = Retained::cast_unchecked(obj);
                    arr.to_vec()
                        .iter()
                        .filter_map(|cbuuid| cbuuid_to_uuid(cbuuid))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // A manufacturer-data field is a 2-byte little-endian company
            // identifier followed by the vendor payload. Anything shorter has
            // no identifier and is discarded.
            let manufacturer_data = advertisement_data
                .objectForKey(CBAdvertisementDataManufacturerDataKey)
                .and_then(|obj| {
                    let data: &NSData = (*obj).downcast_ref::<NSData>()?;
                    let bytes = data.to_vec();
                    let (company, payload) = bytes.split_at_checked(2)?;
                    Some(HashMap::from([(
                        u16::from_le_bytes([company[0], company[1]]),
                        payload.to_vec(),
                    )]))
                })
                .unwrap_or_default();

            let service_data = advertisement_data
                .objectForKey(CBAdvertisementDataServiceDataKey)
                .map(|obj| {
                    // SAFETY: CoreBluetooth guarantees this key's value is
                    // NSDictionary<CBUUID, NSData>.
                    let dict: Retained<NSDictionary<CBUUID, NSData>> =
                        Retained::cast_unchecked(obj);
                    let mut out = HashMap::new();
                    for key in dict.allKeys().to_vec() {
                        if let Some(uuid) = cbuuid_to_uuid(&key)
                            && let Some(value) = dict.objectForKey(&key)
                        {
                            out.insert(uuid, value.to_vec());
                        }
                    }
                    out
                })
                .unwrap_or_default();

            let rssi_val = rssi.integerValue() as i16;
            let device = BleDevice {
                id: id.clone(),
                name,
                rssi: Some(rssi_val),
                services,
                manufacturer_data,
                service_data,
            };
            debug!(device_id = %id, name = ?device.name, rssi = rssi_val, "device discovered");
            let inner = self.ivars();
            inner.peripherals.lock().insert(id.clone(), retain_send(peripheral));
            inner.discovered.lock().insert(id.clone(), device.clone());
            inner.emit(CentralEvent::DeviceDiscovered(device));
        }

        #[unsafe(method(centralManager:didConnectPeripheral:))]
        unsafe fn centralManager_didConnectPeripheral(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
        ) {
            let id = peripheral_device_id(peripheral);
            debug!(device_id = %id, "device connected");
            let inner = self.ivars();
            if let Some(tx) = inner.connects.take(&id) {
                let _ = tx.send(Ok(()));
            }
            inner.emit(CentralEvent::DeviceConnected { device_id: id });
            // Request 2M PHY for higher throughput (BLE 5, available macOS 10.13+).
            // CBPeripheralPHY2M = 2. Fire-and-forget -- negotiation completes before
            // the caller reaches service discovery and L2CAP open.
            // Guard with respondsToSelector: -- msg_send! panics on missing selectors.
            let sel = objc2::sel!(setPreferredPHY:rx:);
            let responds: bool = objc2::msg_send![peripheral, respondsToSelector: sel];
            if responds {
                let _: () = objc2::msg_send![peripheral, setPreferredPHY: 2_isize, rx: 2_isize];
            }
        }

        #[unsafe(method(centralManager:didFailToConnectPeripheral:error:))]
        unsafe fn centralManager_didFailToConnectPeripheral_error(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let err_msg = error.map(|e| e.localizedDescription().to_string());
            warn!(device_id = %id, error = ?err_msg, "device connection failed");
            let inner = self.ivars();
            let err = err_msg.map_or_else(
                || BlewError::Internal("connection failed".into()),
                BlewError::Internal,
            );
            if let Some(tx) = inner.connects.take(&id) {
                let _ = tx.send(Err(err));
            }
        }

        #[unsafe(method(centralManager:didDisconnectPeripheral:error:))]
        unsafe fn centralManager_didDisconnectPeripheral_error(
            &self,
            central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            debug!(device_id = %id, "device disconnected");
            let inner = self.ivars();
            inner.peripherals.lock().remove(&id);
            inner.fail_pending(&id);
            inner.write_ready.notify_waiters();
            let cause = if central.state() == CBManagerState::PoweredOn {
                match error {
                    Some(err) => {
                        let code = err.code();
                        let is_cb_domain = &*err.domain() == unsafe { CBErrorDomain };
                        if is_cb_domain {
                            match CBError(code) {
                                CBError::ConnectionTimeout => DisconnectCause::Timeout,
                                CBError::PeripheralDisconnected => DisconnectCause::RemoteClose,
                                CBError::ConnectionFailed => DisconnectCause::LinkLoss,
                                _ => DisconnectCause::Unknown(
                                    i32::try_from(code).unwrap_or(i32::MIN),
                                ),
                            }
                        } else {
                            DisconnectCause::Unknown(i32::try_from(code).unwrap_or(i32::MIN))
                        }
                    }
                    None => DisconnectCause::LocalClose,
                }
            } else {
                DisconnectCause::AdapterOff
            };
            inner.emit(CentralEvent::DeviceDisconnected { device_id: id, cause });
        }

        #[unsafe(method(centralManager:willRestoreState:))]
        unsafe fn centralManager_willRestoreState(
            &self,
            _central: &CBCentralManager,
            dict: &NSDictionary<NSString, AnyObject>,
        ) {
            let key = unsafe { CBCentralManagerRestoredStatePeripheralsKey };
            let Some(obj) = dict.objectForKey(key) else {
                return;
            };
            // SAFETY: CoreBluetooth guarantees this key's value is NSArray<CBPeripheral>.
            let arr: Retained<NSArray<CBPeripheral>> = Retained::cast_unchecked(obj);
            let mut recovered = Vec::new();
            let inner = self.ivars();
            for peripheral in arr.to_vec() {
                let id = peripheral_device_id(&peripheral);
                let name = peripheral.name().map(|n| n.to_string());
                // Restoration hands back peripherals, not advertisements, so
                // there is no advertisement payload to recover here.
                let device = BleDevice {
                    id: id.clone(),
                    name,
                    rssi: None,
                    services: vec![],
                    manufacturer_data: HashMap::new(),
                    service_data: HashMap::new(),
                };
                inner
                    .peripherals

.lock()
                    .insert(id.clone(), unsafe { retain_send(&*peripheral) });
                inner
                    .discovered

.lock()
                    .insert(id.clone(), device.clone());
                recovered.push(device);
            }
            debug!(
                count = recovered.len(),
                "OS-level state restoration recovered peripherals"
            );
            *inner.restored.lock() = Some(recovered);
        }
    }

    unsafe impl CBPeripheralDelegate for CentralDelegate {
        #[unsafe(method(peripheral:didDiscoverServices:))]
        unsafe fn peripheral_didDiscoverServices(
            &self,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let inner = self.ivars();

            if let Some(e) = error {
                let err = BlewError::DiscoveryFailed {
                    device_id: id.clone(),
                    reason: e.localizedDescription().to_string(),
                };
                if let Some(ds) = inner.discoveries.lock().remove(&id) {
                    let _ = ds.tx.send(Err(err));
                }
                return;
            }

            let services = match peripheral.services() {
                Some(s) if s.count() > 0 => s,
                _ => {
                    if let Some(ds) = inner.discoveries.lock().remove(&id) {
                        let _ = ds.tx.send(Ok(vec![]));
                    }
                    return;
                }
            };

            let svc_vec = services.to_vec();
            debug!(device_id = %id, count = svc_vec.len(), "services discovered, fetching characteristics");
            let mut lock = inner.discoveries.lock();
            let Some(ds) = lock.get_mut(&id) else {
                return;
            };

            ds.pending = svc_vec.len();
            for svc in &svc_vec {
                if let Some(svc_uuid) = cbuuid_to_uuid(&svc.UUID()) {
                    ds.services.insert(
                        svc_uuid,
                        GattService {
                            uuid: svc_uuid,
                            primary: svc.isPrimary(),
                            characteristics: vec![],
                        },
                    );
                    peripheral.discoverCharacteristics_forService(None, svc);
                } else {
                    ds.pending = ds.pending.saturating_sub(1);
                }
            }
            if ds.pending == 0 {
                let svcs: Vec<_> = ds.services.drain().map(|(_, v)| v).collect();
                if let Some(ds) = lock.remove(&id) {
                    let _ = ds.tx.send(Ok(svcs));
                }
            }
        }

        #[unsafe(method(peripheral:didDiscoverCharacteristicsForService:error:))]
        unsafe fn peripheral_didDiscoverCharacteristicsForService_error(
            &self,
            peripheral: &CBPeripheral,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let Some(svc_uuid) = cbuuid_to_uuid(&service.UUID()) else {
                return;
            };
            let inner = self.ivars();
            let mut lock = inner.discoveries.lock();
            let Some(ds) = lock.get_mut(&id) else {
                return;
            };

            if error.is_none()
                && let Some(chars) = service.characteristics() {
                    let built: Vec<_> = chars.to_vec()
                        .iter()
                        .filter_map(|c| {
                            let c_uuid = cbuuid_to_uuid(&c.UUID())?;
                            Some(GattCharacteristic {
                                uuid: c_uuid,
                                properties: cb_props_to_ours(c.properties()),
                                permissions: AttributePermissions::empty(),
                                value: c.value().map(|d| d.to_vec()).unwrap_or_default(),
                                descriptors: vec![],
                            })
                        })
                        .collect();
                    if let Some(svc) = ds.services.get_mut(&svc_uuid) {
                        svc.characteristics = built;
                    }
                }

            ds.pending = ds.pending.saturating_sub(1);
            if ds.pending == 0 {
                let svcs: Vec<_> = ds.services.drain().map(|(_, v)| v).collect();
                if let Some(ds) = lock.remove(&id) {
                    let _ = ds.tx.send(Ok(svcs));
                }
            }
        }

        #[unsafe(method(peripheral:didUpdateValueForCharacteristic:error:))]
        unsafe fn peripheral_didUpdateValueForCharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let Some(char_uuid) = cbuuid_to_uuid(&characteristic.UUID()) else {
                return;
            };
            let inner = self.ivars();

            // Read response?
            if let Some(tx) = inner.reads.take(&(id.clone(), char_uuid)) {
                let result = if let Some(e) = error {
                    Err(BlewError::Internal(e.localizedDescription().to_string()))
                } else {
                    Ok(characteristic.value().map(|d| d.to_vec()).unwrap_or_default())
                };
                let _ = tx.send(result);
                return;
            }

            // Otherwise it's a notification/indication.
            if error.is_some() {
                return;
            }
            let value =
                Bytes::from(characteristic.value().map(|d| d.to_vec()).unwrap_or_default());
            trace!(device_id = %id, %char_uuid, len = value.len(), "characteristic notification");
            inner.emit(CentralEvent::CharacteristicNotification {
                device_id: id,
                char_uuid,
                value,
            });
        }

        #[unsafe(method(peripheral:didWriteValueForCharacteristic:error:))]
        unsafe fn peripheral_didWriteValueForCharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let Some(char_uuid) = cbuuid_to_uuid(&characteristic.UUID()) else {
                return;
            };
            let inner = self.ivars();
            if let Some(tx) = inner.writes.take(&(id, char_uuid)) {
                let result = error.map_or(Ok(()), |e| {
                    Err(BlewError::Internal(e.localizedDescription().to_string()))
                });
                let _ = tx.send(result);
            }
        }

        #[unsafe(method(peripheralIsReadyToSendWriteWithoutResponse:))]
        unsafe fn peripheralIsReadyToSendWriteWithoutResponse(&self, _peripheral: &CBPeripheral) {
            self.ivars().write_ready.notify_waiters();
        }

        #[unsafe(method(peripheral:didUpdateNotificationStateForCharacteristic:error:))]
        unsafe fn peripheral_didUpdateNotificationStateForCharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let Some(char_uuid) = cbuuid_to_uuid(&characteristic.UUID()) else {
                return;
            };
            let inner = self.ivars();
            if let Some(tx) = inner.notify_states.take(&(id, char_uuid)) {
                let result = error.map_or(Ok(()), |e| {
                    Err(BlewError::Internal(e.localizedDescription().to_string()))
                });
                let _ = tx.send(result);
            }
        }

        /// Fires when an L2CAP channel to a peripheral opens (or fails).
        #[unsafe(method(peripheral:didOpenL2CAPChannel:error:))]
        unsafe fn peripheral_didOpenL2CAPChannel_error(
            &self,
            peripheral: &CBPeripheral,
            channel: Option<&CBL2CAPChannel>,
            error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            let inner = self.ivars();
            let Some(tx) = inner.l2cap_pendings.take(&id) else { return };

            if let Some(e) = error {
                warn!(device_id = %id, error = %e.localizedDescription(), "L2CAP channel open failed");
                let _ = tx.send(Err(BlewError::Internal(e.localizedDescription().to_string())));
                return;
            }
            let Some(ch) = channel else {
                warn!(device_id = %id, "L2CAP channel open returned no channel");
                let _ = tx.send(Err(BlewError::Internal("no L2CAP channel".into())));
                return;
            };
            debug!(device_id = %id, "L2CAP channel opened");
            let config = inner.l2cap_config.lock().clone();
            let result = bridge_l2cap_channel(ch, &inner.runtime, &config).map_err(|reason| {
                BlewError::L2cap {
                    source: format!("{reason:?}").into(),
                }
            });
            let _ = tx.send(result);
        }
    }
);

impl CentralDelegate {
    fn new(inner: Arc<CentralInner>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(inner);
        unsafe { objc2::msg_send![super(this), init] }
    }
}

struct CentralHandle {
    manager: ObjcSend<CBCentralManager>,
    /// Retained here so the CB manager's weak-ref delegate stays alive.
    delegate: ObjcSend<CentralDelegate>,
    inner: Arc<CentralInner>,
}

unsafe impl Send for CentralHandle {}
unsafe impl Sync for CentralHandle {}

pub struct AppleCentral(Arc<CentralHandle>);

impl backend::private::Sealed for AppleCentral {}

impl CentralBackend for AppleCentral {
    type EventStream = BroadcastEventStream<CentralEvent>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        Self::with_config(CentralConfig::default()).await
    }

    fn is_powered(&self) -> impl Future<Output = BlewResult<bool>> + Send {
        let handle = Arc::clone(&self.0);
        async move {
            let state = unsafe { handle.manager.state() };
            Ok(state == CBManagerState::PoweredOn)
        }
    }

    fn start_scan(&self, filter: ScanFilter) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        async move {
            debug!(service_filter = ?filter.services, "starting BLE scan");
            let uuids: Option<Retained<NSArray<CBUUID>>> = if filter.services.is_empty() {
                None
            } else {
                let cbuuids: Vec<Retained<CBUUID>> =
                    filter.services.iter().map(|u| uuid_to_cbuuid(*u)).collect();
                Some(NSArray::from_retained_slice(&cbuuids))
            };
            unsafe {
                handle
                    .manager
                    .scanForPeripheralsWithServices_options(uuids.as_deref(), None);
            }
            Ok(())
        }
    }

    fn stop_scan(&self) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        async move {
            debug!("stopping BLE scan");
            unsafe { handle.manager.stopScan() };
            Ok(())
        }
    }

    fn discovered_devices(&self) -> impl Future<Output = BlewResult<Vec<BleDevice>>> + Send {
        let handle = Arc::clone(&self.0);
        async move { Ok(handle.inner.discovered.lock().values().cloned().collect()) }
    }

    fn connect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, "connecting to device");
            // All ObjC ops in a synchronous block to avoid holding !Send types across .await.
            let id_for_err = device_id.clone();
            let peripheral_to_cancel;
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::DeviceNotFound(device_id));
                };

                let (tx, rx) = oneshot::channel();
                if handle
                    .inner
                    .connects
                    .try_insert(device_id.clone(), tx)
                    .is_err()
                {
                    return Err(BlewError::ConnectInFlight(device_id));
                }

                unsafe {
                    peripheral.setDelegate(Some(ProtocolObject::from_ref(&*handle.delegate)));
                    handle.manager.connectPeripheral_options(&peripheral, None);
                }
                peripheral_to_cancel = peripheral;
                rx
            };

            let timeout = *handle.inner.connect_timeout.lock();
            let result = match timeout {
                Some(dur) => match tokio::time::timeout(dur, rx).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(BlewError::DisconnectedDuringOperation(id_for_err.clone())),
                    Err(_) => {
                        handle.inner.connects.take(&id_for_err);
                        unsafe {
                            handle
                                .manager
                                .cancelPeripheralConnection(&peripheral_to_cancel);
                        }
                        handle.inner.emit(CentralEvent::DeviceDisconnected {
                            device_id: id_for_err.clone(),
                            cause: DisconnectCause::Timeout,
                        });
                        Err(BlewError::ConnectTimedOut(id_for_err))
                    }
                },
                None => rx
                    .await
                    .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err))),
            };
            drop(peripheral_to_cancel);
            result
        }
    }

    fn disconnect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, "disconnecting from device");
            let peripheral = handle
                .inner
                .peripherals
                .lock()
                .get(&device_id)
                .map(|p| unsafe { retain_send(&**p) });
            let Some(peripheral) = peripheral else {
                return Err(BlewError::NotConnected(device_id));
            };
            unsafe { handle.manager.cancelPeripheralConnection(&peripheral) };
            Ok(())
        }
    }

    fn discover_services(
        &self,
        device_id: &DeviceId,
    ) -> impl Future<Output = BlewResult<Vec<GattService>>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, "discovering GATT services");
            let id_for_err = device_id.clone();
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::NotConnected(device_id.clone()));
                };
                let (tx, rx) = oneshot::channel();
                handle.inner.discoveries.lock().insert(
                    device_id,
                    DiscoveryState {
                        services: HashMap::new(),
                        pending: 0,
                        tx,
                    },
                );
                unsafe { peripheral.discoverServices(None) };
                rx
            };
            rx.await
                .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err)))
        }
    }

    fn read_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<Vec<u8>>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, %char_uuid, "reading characteristic");
            let id_for_err = device_id.clone();
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::NotConnected(device_id.clone()));
                };
                let characteristic = unsafe { find_characteristic(&peripheral, char_uuid) }
                    .ok_or_else(|| BlewError::CharacteristicNotFound {
                        device_id: device_id.clone(),
                        char_uuid,
                    })?;

                let (tx, rx) = oneshot::channel();
                let evicted = handle.inner.reads.insert((device_id, char_uuid), tx);
                if evicted.is_some() {
                    warn!(%char_uuid, "concurrent read evicted pending waiter");
                }
                unsafe { peripheral.readValueForCharacteristic(&characteristic) };
                rx
                // peripheral and characteristic drop here, before .await
            };
            rx.await
                .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err)))
        }
    }

    fn write_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
        write_type: WriteType,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            trace!(device_id = %device_id, %char_uuid, len = value.len(), ?write_type, "writing characteristic");
            if write_type == WriteType::WithoutResponse {
                return write_without_response(&handle.inner, &device_id, char_uuid, &value).await;
            }
            let id_for_err = device_id.clone();
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::NotConnected(device_id.clone()));
                };
                let characteristic = unsafe { find_characteristic(&peripheral, char_uuid) }
                    .ok_or_else(|| BlewError::CharacteristicNotFound {
                        device_id: device_id.clone(),
                        char_uuid,
                    })?;

                let cb_type = match write_type {
                    WriteType::WithResponse => CBCharacteristicWriteType::WithResponse,
                    WriteType::WithoutResponse => CBCharacteristicWriteType::WithoutResponse,
                };

                // CoreBluetooth raises NSInvalidArgumentException (→ SIGABRT) when
                // writeValue:forCharacteristic:type: receives a payload that
                // exceeds the negotiated capacity for the given write type.
                // Clamp both .withResponse and .withoutResponse paths — the
                // framework's long-write support for .withResponse has its own
                // bugs (FB13596337), so staying under the reported max is safer.
                let got = value.len();
                let max = unsafe { peripheral.maximumWriteValueLengthForType(cb_type) };
                if got > max {
                    return Err(BlewError::ValueTooLarge { got, max });
                }

                let data = NSData::from_vec(value);

                let (tx, rx) = oneshot::channel();
                let evicted = handle.inner.writes.insert((device_id, char_uuid), tx);
                if evicted.is_some() {
                    warn!(%char_uuid, "concurrent write evicted pending waiter");
                }
                unsafe {
                    peripheral.writeValue_forCharacteristic_type(&data, &characteristic, cb_type);
                };
                rx
                // all ObjC objects drop here, before .await
            };
            rx.await
                .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err)))
        }
    }

    fn subscribe_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        AppleCentral::set_notify_impl(Arc::clone(&self.0), device_id.clone(), char_uuid, true)
    }

    fn unsubscribe_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        AppleCentral::set_notify_impl(Arc::clone(&self.0), device_id.clone(), char_uuid, false)
    }

    fn mtu(&self, device_id: &DeviceId) -> impl Future<Output = u16> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return 23_u16;
                };
                // maximumWriteValueLengthForType returns the payload capacity (no ATT header).
                // Add 3 to match the ATT MTU convention used elsewhere.
                let max = unsafe {
                    peripheral
                        .maximumWriteValueLengthForType(CBCharacteristicWriteType::WithoutResponse)
                };
                (max as u16).saturating_add(3)
            }
        }
    }

    fn open_l2cap_channel(
        &self,
        device_id: &DeviceId,
        psm: Psm,
    ) -> impl Future<Output = BlewResult<L2capChannel>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, psm = psm.0, "opening L2CAP channel");
            let encryption = handle.inner.l2cap_config.lock().encryption;
            if encryption != L2capEncryption::Insecure {
                return Err(BlewError::L2capEncryptionUnsupported {
                    requested: encryption,
                    reason: "CoreBluetooth exposes no way to raise link security \
                             on demand, so an opening central can only accept \
                             whatever the peer's PSM insists on",
                });
            }
            let id_for_err = device_id.clone();
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::DeviceNotFound(device_id));
                };
                let (tx, rx) = oneshot::channel();
                let evicted = handle.inner.l2cap_pendings.insert(device_id, tx);
                if evicted.is_some() {
                    warn!("concurrent L2CAP open evicted pending waiter");
                }
                unsafe { peripheral.openL2CAPChannel(psm.0) };
                rx
            };
            rx.await
                .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err)))
        }
    }

    fn events(&self) -> Self::EventStream {
        BroadcastEventStream::new(self.0.inner.event_tx.subscribe())
    }
}

impl AppleCentral {
    /// Consume the preserved-peripherals payload captured from
    /// `centralManager:willRestoreState:` during `with_config`. Returns `None`
    /// after the first call.
    #[must_use]
    pub fn take_restored(&self) -> Option<Vec<BleDevice>> {
        self.0.inner.restored.lock().take()
    }

    pub async fn with_config(config: CentralConfig) -> BlewResult<Self> {
        let (inner, mut powered_rx) = CentralInner::new();
        *inner.connect_timeout.lock() = config.connect_timeout;
        *inner.l2cap_config.lock() = config.l2cap.clone();
        let delegate = CentralDelegate::new(Arc::clone(&inner));
        let queue = DispatchQueue::new("blew.central", DispatchQueueAttr::SERIAL);

        // State restoration (CBCentralManagerOptionRestoreIdentifierKey) is an iOS-only
        // feature; passing it on macOS causes CoreBluetooth to throw an NSException.
        #[cfg(target_os = "ios")]
        let manager = ObjcSend(unsafe {
            if let Some(ref id) = config.restore_identifier {
                let key: &NSString = CBCentralManagerOptionRestoreIdentifierKey;
                let value = NSString::from_str(id);
                let v_any: &AnyObject = &value;
                let options = NSDictionary::from_slices(&[key], &[v_any]);
                CBCentralManager::initWithDelegate_queue_options(
                    CBCentralManager::alloc(),
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                    Some(&options),
                )
            } else {
                CBCentralManager::initWithDelegate_queue(
                    CBCentralManager::alloc(),
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                )
            }
        });
        #[cfg(not(target_os = "ios"))]
        let manager = ObjcSend(unsafe {
            CBCentralManager::initWithDelegate_queue(
                CBCentralManager::alloc(),
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&queue),
            )
        });
        let delegate = ObjcSend(delegate);

        let timeout = tokio::time::sleep(std::time::Duration::from_secs(15));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                res = powered_rx.changed() => {
                    if res.is_err() { break; }
                    if *powered_rx.borrow() { break; }
                    let state = unsafe { manager.state() };
                    if matches!(
                        state,
                        CBManagerState::Unsupported | CBManagerState::Unauthorized
                    ) {
                        return Err(BlewError::AdapterNotFound);
                    }
                }
                () = &mut timeout => {
                    if unsafe { manager.state() } == CBManagerState::PoweredOn {
                        break;
                    }
                    return Err(BlewError::NotPowered);
                }
            }
        }

        Ok(AppleCentral(Arc::new(CentralHandle {
            manager,
            delegate,
            inner,
        })))
    }

    async fn set_notify_impl(
        handle: Arc<CentralHandle>,
        device_id: DeviceId,
        char_uuid: Uuid,
        enabled: bool,
    ) -> BlewResult<()> {
        let id_for_err = device_id.clone();
        let rx = {
            let peripheral = handle
                .inner
                .peripherals
                .lock()
                .get(&device_id)
                .map(|p| unsafe { retain_send(&**p) });
            let Some(peripheral) = peripheral else {
                return Err(BlewError::NotConnected(device_id.clone()));
            };
            let characteristic = unsafe { find_characteristic(&peripheral, char_uuid) }
                .ok_or_else(|| BlewError::CharacteristicNotFound {
                    device_id: device_id.clone(),
                    char_uuid,
                })?;

            let (tx, rx) = oneshot::channel();
            let evicted = handle
                .inner
                .notify_states
                .insert((device_id, char_uuid), tx);
            if evicted.is_some() {
                warn!(%char_uuid, "concurrent notify-state change evicted pending waiter");
            }
            unsafe { peripheral.setNotifyValue_forCharacteristic(enabled, &characteristic) };
            rx
        };
        rx.await
            .unwrap_or(Err(BlewError::DisconnectedDuringOperation(id_for_err)))
    }
}

/// Find a characteristic by UUID across all discovered services.
///
/// # Safety
/// Must be called while the peripheral is still connected with services discovered.
unsafe fn find_characteristic(
    peripheral: &CBPeripheral,
    char_uuid: Uuid,
) -> Option<Retained<CBCharacteristic>> {
    let target = uuid_to_cbuuid(char_uuid);
    let services = peripheral.services()?;
    for svc in services.to_vec() {
        // Skip services whose characteristics haven't been discovered (e.g. system
        // services with 16-bit UUIDs that were not enumerated).
        let Some(chars) = svc.characteristics() else {
            continue;
        };
        for c in chars.to_vec() {
            if c.UUID() == target {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_ready_write_goes_out_at_once() {
        let ready = Notify::new();
        assert!(
            retry_when_ready(&ready, TIMEOUT, || Ok(true))
                .await
                .unwrap()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_waits_for_the_ready_callback() {
        let ready = Arc::new(Notify::new());
        let room = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let write = tokio::spawn({
            let (ready, room, attempts) = (ready.clone(), room.clone(), attempts.clone());
            async move {
                retry_when_ready(&ready, TIMEOUT, || {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(room.load(Ordering::SeqCst))
                })
                .await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!write.is_finished());

        room.store(true, Ordering::SeqCst);
        ready.notify_waiters();
        assert!(write.await.unwrap().unwrap());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_callback_between_attempt_and_wait_is_not_missed() {
        let ready = Notify::new();
        let mut first = true;
        let sent = retry_when_ready(&ready, TIMEOUT, || {
            if first {
                first = false;
                ready.notify_waiters();
                return Ok(false);
            }
            Ok(true)
        })
        .await
        .unwrap();
        assert!(sent);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_callback_that_never_comes_times_out() {
        let ready = Notify::new();
        assert!(
            !retry_when_ready(&ready, TIMEOUT, || Ok(false))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_attempt_error_ends_the_wait() {
        let ready = Notify::new();
        let err = retry_when_ready(&ready, TIMEOUT, || {
            Err(BlewError::NotConnected(DeviceId::from("gone")))
        })
        .await;
        assert!(matches!(err, Err(BlewError::NotConnected(_))));
    }
}
