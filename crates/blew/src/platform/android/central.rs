use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use jni::objects::{JObject, JObjectArray};
use jni::{jni_sig, jni_str};
use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::central::backend::{self, CentralBackend};
use crate::central::types::{CentralConfig, CentralEvent, DisconnectCause, ScanFilter, WriteType};
use crate::error::{BlewError, BlewResult};
use crate::gatt::props::CharacteristicProperties;
use crate::gatt::service::{GattCharacteristic, GattService};
use crate::l2cap::{L2capChannel, types::Psm};
use crate::types::{BleDevice, DeviceId};
use crate::util::BroadcastEventStream;
use crate::util::request_map::KeyedRequestMap;

use super::jni_globals::{central_class, jvm};

/// How long [`AndroidCentral::disconnect`] waits for the Kotlin-side
/// `onConnectionStateChange(DISCONNECTED)` callback before force-closing the
/// GATT handle. Short — we only need enough time for a well-behaved stack to
/// dispatch the callback; a dead callback is exactly the case we're guarding
/// against.
const DISCONNECT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);

struct CentralState {
    event_tx: broadcast::Sender<CentralEvent>,
    pending_ops: KeyedRequestMap<String, oneshot::Sender<BlewResult<Vec<u8>>>>,
    pending_connects: KeyedRequestMap<String, oneshot::Sender<BlewResult<()>>>,
    pending_disconnects: KeyedRequestMap<String, oneshot::Sender<()>>,
    pending_discover: KeyedRequestMap<String, oneshot::Sender<BlewResult<Vec<GattService>>>>,
    discovered: Mutex<Vec<BleDevice>>,
    mtu_cache: Mutex<HashMap<String, u16>>,
    connect_timeout: Mutex<Option<Duration>>,
}

static STATE: OnceLock<CentralState> = OnceLock::new();

fn state() -> &'static CentralState {
    STATE.get().expect("AndroidCentral not initialized")
}

fn init_statics(connect_timeout: Option<Duration>) {
    let (event_tx, _) = broadcast::channel(256);
    let first_init = STATE
        .set(CentralState {
            event_tx,
            pending_ops: KeyedRequestMap::new(),
            pending_connects: KeyedRequestMap::new(),
            pending_disconnects: KeyedRequestMap::new(),
            pending_discover: KeyedRequestMap::new(),
            discovered: Mutex::new(Vec::new()),
            mtu_cache: Mutex::new(HashMap::new()),
            connect_timeout: Mutex::new(connect_timeout),
        })
        .is_ok();
    if !first_init {
        // OnceLock is already populated (AndroidCentral initialised more than
        // once in the process). Update the timeout since the existing
        // callbacks/maps are still valid for the new instance.
        if let Some(s) = STATE.get() {
            *s.connect_timeout.lock() = connect_timeout;
        }
    }
    super::l2cap_state::init_statics();
}

pub(crate) fn send_event(event: CentralEvent) {
    if let Some(s) = STATE.get() {
        let _ = s.event_tx.send(event);
    }
}

pub(crate) fn update_discovered(device: BleDevice) {
    if let Some(s) = STATE.get() {
        let mut list = s.discovered.lock();
        if let Some(existing) = list.iter_mut().find(|d| d.id == device.id) {
            *existing = device;
        } else {
            list.push(device);
        }
    }
}

pub(crate) fn complete_connect(addr: &str, result: BlewResult<()>) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_connects.take(&addr.to_owned())
    {
        let _ = tx.send(result);
    }
}

pub(crate) fn complete_disconnect(addr: &str) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_disconnects.take(&addr.to_owned())
    {
        let _ = tx.send(());
    }
}

pub(crate) fn complete_discover_services(addr: &str, result: BlewResult<Vec<GattService>>) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_discover.take(&addr.to_owned())
    {
        let _ = tx.send(result);
    }
}

pub(crate) fn complete_pending(key: &str, result: BlewResult<Vec<u8>>) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_ops.take(&key.to_owned())
    {
        let _ = tx.send(result);
    }
}

pub(crate) fn set_mtu(addr: &str, mtu: u16) {
    if let Some(s) = STATE.get() {
        s.mtu_cache.lock().insert(addr.to_owned(), mtu);
    }
}

pub(crate) fn parse_services_json(json: &str) -> Vec<GattService> {
    let array: Vec<serde_json::Value> = match serde_json::from_str(json) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("failed to parse services JSON: {e}");
            return Vec::new();
        }
    };

    array
        .into_iter()
        .filter_map(|svc| {
            let uuid: Uuid = svc.get("uuid")?.as_str()?.parse().ok()?;
            let chars = svc.get("characteristics")?.as_array()?;
            let characteristics = chars
                .iter()
                .filter_map(|ch| {
                    let chr_uuid: Uuid = ch.get("uuid")?.as_str()?.parse().ok()?;
                    let props =
                        u16::try_from(ch.get("properties")?.as_i64().unwrap_or(0)).unwrap_or(0);
                    Some(GattCharacteristic {
                        uuid: chr_uuid,
                        properties: CharacteristicProperties::from_bits_truncate(props),
                        permissions: crate::gatt::props::AttributePermissions::empty(),
                        value: vec![],
                        descriptors: vec![],
                    })
                })
                .collect();
            Some(GattService {
                uuid,
                primary: true,
                characteristics,
            })
        })
        .collect()
}

/// Kotlin status codes returned from GATT operations.
const STATUS_SUCCESS: i32 = 0;
const STATUS_NOT_CONNECTED: i32 = 1;
const STATUS_CHAR_NOT_FOUND: i32 = 2;
const STATUS_GATT_BUSY: i32 = 3;
const STATUS_GATT_FAILED: i32 = 4;

/// Map a Kotlin GATT status code to a [`BlewError`].
fn gatt_status_to_error(status: i32, device_id: &DeviceId, char_uuid: Uuid) -> BlewError {
    match status {
        STATUS_NOT_CONNECTED => BlewError::NotConnected(device_id.clone()),
        STATUS_CHAR_NOT_FOUND => BlewError::CharacteristicNotFound {
            device_id: device_id.clone(),
            char_uuid,
        },
        STATUS_GATT_BUSY => BlewError::GattBusy(device_id.clone()),
        STATUS_GATT_FAILED => BlewError::Gatt {
            device_id: device_id.clone(),
            source: "GATT operation returned failure".into(),
        },
        other => BlewError::Internal(format!("unknown GATT status {other}")),
    }
}

pub struct AndroidCentral;

impl AndroidCentral {
    // No awaits in the body (all JNI work is synchronous), but the public API
    // mirrors the Apple/Linux async initializers for cross-platform parity.
    #[allow(clippy::unused_async)]
    pub async fn with_config(config: CentralConfig) -> crate::error::BlewResult<Self> {
        if !super::are_ble_permissions_granted() {
            return Err(BlewError::PermissionDenied);
        }
        init_statics(config.connect_timeout);
        debug!(connect_timeout = ?config.connect_timeout, "AndroidCentral initialized");
        Ok(AndroidCentral)
    }

    /// Clear the GATT cache for `device_id` by calling the hidden
    /// `BluetoothGatt.refresh()` method via reflection on the Kotlin side.
    ///
    /// Returns [`BlewError::NotSupported`] if the reflective call fails or
    /// no active GATT handle exists for the device.
    #[allow(clippy::unused_self)]
    pub(crate) fn refresh(
        &self,
        device_id: &DeviceId,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        async move {
            let ok = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("refresh"),
                        jni_sig!("(Ljava/lang/String;)Z"),
                        &[(&j_addr).into()],
                    )?;
                    result.z()
                })
                .map_err(|e| jni_err(&e))?;

            if ok {
                Ok(())
            } else {
                Err(BlewError::NotSupported)
            }
        }
    }
}

impl backend::private::Sealed for AndroidCentral {}

impl CentralBackend for AndroidCentral {
    type EventStream = BroadcastEventStream<CentralEvent>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        Self::with_config(CentralConfig::default()).await
    }

    async fn is_powered(&self) -> BlewResult<bool> {
        jvm()
            .attach_current_thread(|env| {
                let result = env.call_static_method(
                    central_class(),
                    jni_str!("isPowered"),
                    jni_sig!("()Z"),
                    &[],
                )?;
                result.z()
            })
            .map_err(|e| jni_err(&e))
    }

    async fn start_scan(&self, filter: ScanFilter) -> BlewResult<()> {
        let low_power = filter.mode == crate::central::types::ScanMode::LowPower;
        let service_count = i32::try_from(filter.services.len())
            .map_err(|_| BlewError::Internal("scan filter has too many services".into()))?;
        jvm()
            .attach_current_thread(|env| {
                let string_class = env.find_class(jni_str!("java/lang/String"))?;
                let uuids: JObjectArray =
                    env.new_object_array(service_count, &string_class, JObject::null())?;
                for (i, uuid) in filter.services.iter().enumerate() {
                    let s = env.new_string(uuid.to_string())?;
                    uuids.set_element(env, i, &s)?;
                }

                env.call_static_method(
                    central_class(),
                    jni_str!("startScan"),
                    jni_sig!("([Ljava/lang/String;Z)V"),
                    &[(&uuids).into(), low_power.into()],
                )?;

                Ok(())
            })
            .map_err(|e| jni_err(&e))?;

        Ok(())
    }

    async fn stop_scan(&self) -> BlewResult<()> {
        jvm()
            .attach_current_thread(|env| {
                env.call_static_method(
                    central_class(),
                    jni_str!("stopScan"),
                    jni_sig!("()V"),
                    &[],
                )?;
                Ok(())
            })
            .map_err(|e| jni_err(&e))?;
        Ok(())
    }

    async fn discovered_devices(&self) -> BlewResult<Vec<BleDevice>> {
        let list = STATE
            .get()
            .map(|s| s.discovered.lock().clone())
            .unwrap_or_default();
        Ok(list)
    }

    fn connect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let s = state();
            let (tx, rx) = oneshot::channel();

            if s.pending_connects.try_insert(addr.clone(), tx).is_err() {
                return Err(BlewError::ConnectInFlight(did));
            }

            if let Err(err) = jvm().attach_current_thread(|env| {
                let j_addr = env.new_string(&addr)?;
                env.call_static_method(
                    central_class(),
                    jni_str!("connect"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[(&j_addr).into()],
                )?;
                Ok(())
            }) {
                s.pending_connects.take(&addr);
                return Err(jni_err(&err));
            }

            let timeout = *s.connect_timeout.lock();
            match timeout {
                Some(dur) => match tokio::time::timeout(dur, rx).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(BlewError::DisconnectedDuringOperation(did)),
                    Err(_) => {
                        s.pending_connects.take(&addr);
                        force_close_gatt(&addr);
                        let _ = s.event_tx.send(CentralEvent::DeviceDisconnected {
                            device_id: did.clone(),
                            cause: DisconnectCause::Timeout,
                        });
                        Err(BlewError::ConnectTimedOut(did))
                    }
                },
                None => rx
                    .await
                    .map_err(|_| BlewError::DisconnectedDuringOperation(did))?,
            }
        }
    }

    fn disconnect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        async move {
            let s = state();
            let (tx, rx) = oneshot::channel();
            let awaits_callback = s.pending_disconnects.try_insert(addr.clone(), tx).is_ok();

            if let Err(err) = jvm().attach_current_thread(|env| {
                let j_addr = env.new_string(&addr)?;
                env.call_static_method(
                    central_class(),
                    jni_str!("disconnect"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[(&j_addr).into()],
                )?;
                Ok(())
            }) {
                if awaits_callback {
                    s.pending_disconnects.take(&addr);
                }
                return Err(jni_err(&err));
            }

            if !awaits_callback {
                return Ok(());
            }

            if tokio::time::timeout(DISCONNECT_CALLBACK_TIMEOUT, rx)
                .await
                .is_err()
            {
                warn!(device = %addr, "disconnect callback did not fire; force-closing GATT");
                s.pending_disconnects.take(&addr);
                force_close_gatt(&addr);
            }
            Ok(())
        }
    }

    fn discover_services(
        &self,
        device_id: &DeviceId,
    ) -> impl Future<Output = BlewResult<Vec<GattService>>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let (tx, rx) = oneshot::channel();

            let s = state();
            if let Some(evicted) = s.pending_discover.insert(addr.clone(), tx) {
                let _ = evicted.send(Err(BlewError::GattBusy(did.clone())));
            }

            let status = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("discoverServices"),
                        jni_sig!("(Ljava/lang/String;)I"),
                        &[(&j_addr).into()],
                    )?;
                    result.i()
                })
                .map_err(|e| jni_err(&e))?;

            if status != STATUS_SUCCESS {
                s.pending_discover.take(&addr);
                return Err(gatt_status_to_error(status, &did, Uuid::nil()));
            }

            rx.await
                .map_err(|_| BlewError::DisconnectedDuringOperation(did))?
        }
    }

    fn read_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<Vec<u8>>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let (tx, rx) = oneshot::channel();
            let key = format!("{addr}:read:{char_uuid}");

            let s = state();
            if let Some(evicted) = s.pending_ops.insert(key.clone(), tx) {
                let _ = evicted.send(Err(BlewError::GattBusy(did.clone())));
            }

            let status = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let j_uuid = env.new_string(char_uuid.to_string())?;
                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("readCharacteristic"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;)I"),
                        &[(&j_addr).into(), (&j_uuid).into()],
                    )?;

                    result.i()
                })
                .map_err(|e| jni_err(&e))?;

            if status != STATUS_SUCCESS {
                s.pending_ops.take(&key);
                return Err(gatt_status_to_error(status, &did, char_uuid));
            }

            rx.await
                .map_err(|_| BlewError::DisconnectedDuringOperation(did))?
        }
    }

    fn write_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
        write_type: WriteType,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let android_write_type = match write_type {
                WriteType::WithResponse => 2,    // WRITE_TYPE_DEFAULT
                WriteType::WithoutResponse => 1, // WRITE_TYPE_NO_RESPONSE
            };

            let s = state();

            // For write-without-response, don't wait for a callback.
            let (rx, pending_key) = if write_type == WriteType::WithResponse {
                let (tx, rx) = oneshot::channel();
                let key = format!("{addr}:write:{char_uuid}");
                if let Some(evicted) = s.pending_ops.insert(key.clone(), tx) {
                    let _ = evicted.send(Err(BlewError::GattBusy(did.clone())));
                }
                (Some(rx), Some(key))
            } else {
                (None, None)
            };

            let status = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let j_uuid = env.new_string(char_uuid.to_string())?;
                    let j_value = env.byte_array_from_slice(&value)?;

                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("writeCharacteristic"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;[BI)I"),
                        &[
                            (&j_addr).into(),
                            (&j_uuid).into(),
                            (&j_value).into(),
                            android_write_type.into(),
                        ],
                    )?;

                    result.i()
                })
                .map_err(|e| jni_err(&e))?;

            if status != STATUS_SUCCESS {
                if let Some(key) = pending_key {
                    s.pending_ops.take(&key);
                }
                return Err(gatt_status_to_error(status, &did, char_uuid));
            }

            if let Some(rx) = rx {
                rx.await
                    .map_err(|_| BlewError::DisconnectedDuringOperation(did))??;
            }

            Ok(())
        }
    }

    fn subscribe_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let status = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let j_uuid = env.new_string(char_uuid.to_string())?;

                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("subscribeCharacteristic"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;)I"),
                        &[(&j_addr).into(), (&j_uuid).into()],
                    )?;

                    result.i()
                })
                .map_err(|e| jni_err(&e))?;

            if status != STATUS_SUCCESS {
                return Err(gatt_status_to_error(status, &did, char_uuid));
            }

            Ok(())
        }
    }

    fn unsubscribe_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let addr = device_id.as_str().to_owned();
        let did = device_id.clone();
        async move {
            let status = jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    let j_uuid = env.new_string(char_uuid.to_string())?;

                    let result = env.call_static_method(
                        central_class(),
                        jni_str!("unsubscribeCharacteristic"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;)I"),
                        &[(&j_addr).into(), (&j_uuid).into()],
                    )?;

                    result.i()
                })
                .map_err(|e| jni_err(&e))?;

            if status != STATUS_SUCCESS {
                return Err(gatt_status_to_error(status, &did, char_uuid));
            }

            Ok(())
        }
    }

    fn mtu(&self, device_id: &DeviceId) -> impl Future<Output = u16> + Send {
        let addr = device_id.as_str().to_owned();
        async move {
            STATE
                .get()
                .and_then(|s| s.mtu_cache.lock().get(&addr).copied())
                .unwrap_or(23)
        }
    }

    fn open_l2cap_channel(
        &self,
        device_id: &DeviceId,
        psm: Psm,
    ) -> impl Future<Output = BlewResult<L2capChannel>> + Send {
        let addr = device_id.as_str().to_owned();
        let id_for_err = device_id.clone();
        async move {
            let (tx, rx) = oneshot::channel();
            super::l2cap_state::set_pending_open(addr.clone(), tx);

            jvm()
                .attach_current_thread(|env| {
                    let j_addr = env.new_string(&addr)?;
                    env.call_static_method(
                        central_class(),
                        jni_str!("openL2capChannel"),
                        jni_sig!("(Ljava/lang/String;I)V"),
                        &[(&j_addr).into(), i32::from(psm.value()).into()],
                    )?;
                    Ok(())
                })
                .map_err(|e| jni_err(&e))?;

            rx.await
                .map_err(|_| BlewError::DisconnectedDuringOperation(id_for_err))?
        }
    }

    fn events(&self) -> Self::EventStream {
        BroadcastEventStream::new(state().event_tx.subscribe())
    }
}

fn jni_err(e: &jni::errors::Error) -> BlewError {
    BlewError::Internal(format!("JNI error: {e}"))
}

/// Synchronously release the Kotlin-side GATT handle for `addr`. Used when
/// the normal disconnect callback path can't be trusted — connect timeout,
/// the status-133 zombie scenario, or a disconnect whose callback never
/// arrives. The Kotlin `forceClose` calls `refresh()` + `gatt.close()` and
/// removes the entry from `gattConnections`, freeing the client-IF slot.
fn force_close_gatt(addr: &str) {
    let result: Result<(), jni::errors::Error> = jvm().attach_current_thread(|env| {
        let j_addr = env.new_string(addr)?;
        env.call_static_method(
            central_class(),
            jni_str!("forceClose"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[(&j_addr).into()],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        warn!(device = %addr, error = %e, "force_close_gatt JNI call failed");
    }
}
