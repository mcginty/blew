//! Apple (macOS / iOS) implementation of [`CentralBackend`].
//!
//! Architecture:
//! - A private GCD serial queue receives all CoreBluetooth delegate callbacks.
//! - CoreBluetooth method calls are dispatched from Tokio tasks directly;
//!   CoreBluetooth is thread-safe on macOS 10.15+ / iOS 13+.
//! - `oneshot` channels carry operation results from CB callbacks to async callers.
//! - `EventFanout` fans `CentralEvent` (which is `Clone`) to all subscribers.

#![allow(
    non_snake_case,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    unsafe_op_in_unsafe_fn
)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::define_class;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass};
use objc2_core_bluetooth::{
    CBAdvertisementDataLocalNameKey, CBAdvertisementDataServiceUUIDsKey, CBCentralManager,
    CBCentralManagerDelegate, CBCharacteristic, CBCharacteristicProperties,
    CBCharacteristicWriteType, CBL2CAPChannel, CBManagerState, CBPeripheral, CBPeripheralDelegate,
    CBService, CBUUID,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSNumber, NSObjectProtocol, NSString,
};
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use tracing::{debug, trace, warn};

use crate::central::backend::{self, CentralBackend};
use crate::central::types::{CentralEvent, ScanFilter, WriteType};
use crate::error::{BlewError, BlewResult};
use crate::gatt::props::{AttributePermissions, CharacteristicProperties};
use crate::gatt::service::{GattCharacteristic, GattService};
use crate::l2cap::{L2capChannel, types::Psm};
use crate::platform::apple::helpers::{
    ObjcSend, cbuuid_to_uuid, peripheral_device_id, retain_send, uuid_to_cbuuid,
};
use crate::platform::apple::l2cap::bridge_l2cap_channel;
use crate::types::{BleDevice, DeviceId};
use crate::util::event_fanout::{EventFanout, EventFanoutTx};

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

type PendingMap<K, V> = Mutex<HashMap<K, oneshot::Sender<BlewResult<V>>>>;

struct CentralInner {
    peripherals: Mutex<HashMap<DeviceId, ObjcSend<CBPeripheral>>>,
    discovered: Mutex<HashMap<DeviceId, BleDevice>>,
    connects: PendingMap<DeviceId, ()>,
    discoveries: Mutex<HashMap<DeviceId, DiscoveryState>>,
    reads: PendingMap<(DeviceId, Uuid), Vec<u8>>,
    writes: PendingMap<(DeviceId, Uuid), ()>,
    notify_states: PendingMap<(DeviceId, Uuid), ()>,
    /// Pending `open_l2cap_channel` results, keyed by device ID.
    l2cap_pendings: PendingMap<DeviceId, L2capChannel>,
    event_tx: EventFanoutTx<CentralEvent>,
    event_fanout: EventFanout<CentralEvent>,
    powered_tx: watch::Sender<bool>,
    /// Tokio runtime handle, captured at construction time so GCD callbacks
    /// (which run off the Tokio thread) can spawn tasks onto the runtime.
    runtime: Handle,
}

impl CentralInner {
    fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (event_tx, event_fanout) = EventFanout::new(128);
        let (powered_tx, powered_rx) = watch::channel(false);
        let inner = Arc::new(Self {
            peripherals: Default::default(),
            discovered: Default::default(),
            connects: Default::default(),
            discoveries: Default::default(),
            reads: Default::default(),
            writes: Default::default(),
            notify_states: Default::default(),
            l2cap_pendings: Default::default(),
            event_tx,
            event_fanout,
            powered_tx,
            runtime: Handle::current(),
        });
        (inner, powered_rx)
    }

    fn emit(&self, event: CentralEvent) {
        self.event_tx.send(event);
    }
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

            let rssi_val = rssi.integerValue() as i16;
            let device = BleDevice {
                id: id.clone(),
                name,
                rssi: Some(rssi_val),
                services,
            };
            debug!(device_id = %id, name = ?device.name, rssi = rssi_val, "device discovered");
            let inner = self.ivars();
            inner.peripherals.lock().unwrap().insert(id.clone(), retain_send(peripheral));
            inner.discovered.lock().unwrap().insert(id.clone(), device.clone());
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
            if let Some(tx) = inner.connects.lock().unwrap().remove(&id) {
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
            if let Some(tx) = inner.connects.lock().unwrap().remove(&id) {
                let _ = tx.send(Err(err));
            }
        }

        #[unsafe(method(centralManager:didDisconnectPeripheral:error:))]
        unsafe fn centralManager_didDisconnectPeripheral_error(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            _error: Option<&NSError>,
        ) {
            let id = peripheral_device_id(peripheral);
            debug!(device_id = %id, "device disconnected");
            let inner = self.ivars();
            inner.peripherals.lock().unwrap().remove(&id);
            inner.emit(CentralEvent::DeviceDisconnected { device_id: id });
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
                let err = BlewError::Internal(e.localizedDescription().to_string());
                if let Some(ds) = inner.discoveries.lock().unwrap().remove(&id) {
                    let _ = ds.tx.send(Err(err));
                }
                return;
            }

            let services = match peripheral.services() {
                Some(s) if s.count() > 0 => s,
                _ => {
                    if let Some(ds) = inner.discoveries.lock().unwrap().remove(&id) {
                        let _ = ds.tx.send(Ok(vec![]));
                    }
                    return;
                }
            };

            let svc_vec = services.to_vec();
            debug!(device_id = %id, count = svc_vec.len(), "services discovered, fetching characteristics");
            let mut lock = inner.discoveries.lock().unwrap();
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
            let mut lock = inner.discoveries.lock().unwrap();
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
            if let Some(tx) = inner.reads.lock().unwrap().remove(&(id.clone(), char_uuid)) {
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
            if let Some(tx) = inner.writes.lock().unwrap().remove(&(id, char_uuid)) {
                let result = error.map_or(Ok(()), |e| {
                    Err(BlewError::Internal(e.localizedDescription().to_string()))
                });
                let _ = tx.send(result);
            }
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
            if let Some(tx) = inner.notify_states.lock().unwrap().remove(&(id, char_uuid)) {
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
            let Some(tx) = inner.l2cap_pendings.lock().unwrap().remove(&id) else { return };

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
            let l2cap = bridge_l2cap_channel(ch, &inner.runtime);
            let _ = tx.send(Ok(l2cap));
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
    type EventStream = ReceiverStream<CentralEvent>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        let (inner, mut powered_rx) = CentralInner::new();
        let delegate = CentralDelegate::new(Arc::clone(&inner));
        let queue = DispatchQueue::new("blew.central", DispatchQueueAttr::SERIAL);

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
        async move {
            Ok(handle
                .inner
                .discovered
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect())
        }
    }

    fn connect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let handle = Arc::clone(&self.0);
        let device_id = device_id.clone();
        async move {
            debug!(device_id = %device_id, "connecting to device");
            // All ObjC ops in a synchronous block to avoid holding !Send types across .await.
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .unwrap()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::DeviceNotFound(device_id));
                };

                let (tx, rx) = oneshot::channel();
                handle.inner.connects.lock().unwrap().insert(device_id, tx);

                unsafe {
                    peripheral.setDelegate(Some(ProtocolObject::from_ref(&*handle.delegate)));
                    handle.manager.connectPeripheral_options(&peripheral, None);
                }
                rx
            };
            rx.await
                .unwrap_or(Err(BlewError::Internal("connect dropped".into())))
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
                .unwrap()
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
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .unwrap()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::NotConnected(device_id.clone()));
                };
                let (tx, rx) = oneshot::channel();
                handle.inner.discoveries.lock().unwrap().insert(
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
                .unwrap_or(Err(BlewError::Internal("discover_services dropped".into())))
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
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .unwrap()
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
                handle
                    .inner
                    .reads
                    .lock()
                    .unwrap()
                    .insert((device_id, char_uuid), tx);
                unsafe { peripheral.readValueForCharacteristic(&characteristic) };
                rx
                // peripheral and characteristic drop here, before .await
            };
            rx.await
                .unwrap_or(Err(BlewError::Internal("read dropped".into())))
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
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .unwrap()
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

                let data = NSData::from_vec(value);
                let cb_type = match write_type {
                    WriteType::WithResponse => CBCharacteristicWriteType::WithResponse,
                    WriteType::WithoutResponse => CBCharacteristicWriteType::WithoutResponse,
                };

                if write_type == WriteType::WithoutResponse {
                    unsafe {
                        peripheral.writeValue_forCharacteristic_type(
                            &data,
                            &characteristic,
                            cb_type,
                        );
                    };
                    return Ok(());
                }

                let (tx, rx) = oneshot::channel();
                handle
                    .inner
                    .writes
                    .lock()
                    .unwrap()
                    .insert((device_id, char_uuid), tx);
                unsafe {
                    peripheral.writeValue_forCharacteristic_type(&data, &characteristic, cb_type);
                };
                rx
                // all ObjC objects drop here, before .await
            };
            rx.await
                .unwrap_or(Err(BlewError::Internal("write dropped".into())))
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
                    .unwrap()
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
            let rx = {
                let peripheral = handle
                    .inner
                    .peripherals
                    .lock()
                    .unwrap()
                    .get(&device_id)
                    .map(|p| unsafe { retain_send(&**p) });
                let Some(peripheral) = peripheral else {
                    return Err(BlewError::DeviceNotFound(device_id));
                };
                let (tx, rx) = oneshot::channel();
                handle
                    .inner
                    .l2cap_pendings
                    .lock()
                    .unwrap()
                    .insert(device_id, tx);
                unsafe { peripheral.openL2CAPChannel(psm.0) };
                rx
            };
            rx.await
                .unwrap_or(Err(BlewError::Internal("l2cap channel dropped".into())))
        }
    }

    fn events(&self) -> Self::EventStream {
        let rx = self.0.inner.event_fanout.subscribe(128);
        ReceiverStream::new(rx)
    }
}

impl AppleCentral {
    async fn set_notify_impl(
        handle: Arc<CentralHandle>,
        device_id: DeviceId,
        char_uuid: Uuid,
        enabled: bool,
    ) -> BlewResult<()> {
        let rx = {
            let peripheral = handle
                .inner
                .peripherals
                .lock()
                .unwrap()
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
            handle
                .inner
                .notify_states
                .lock()
                .unwrap()
                .insert((device_id, char_uuid), tx);
            unsafe { peripheral.setNotifyValue_forCharacteristic(enabled, &characteristic) };
            rx
        };
        rx.await
            .unwrap_or(Err(BlewError::Internal("set_notify dropped".into())))
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
