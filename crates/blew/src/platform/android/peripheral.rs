use jni::objects::{JObject, JObjectArray};
use jni::{jni_sig, jni_str};
use parking_lot::Mutex;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::error::{BlewError, BlewResult};
use crate::gatt::props::CharacteristicProperties;
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, types::Psm};
use crate::peripheral::backend::{self, PeripheralBackend};
use crate::peripheral::types::{
    AdvertisingConfig, PeripheralConfig, PeripheralRequest, PeripheralStateEvent,
};
use crate::types::DeviceId;
use crate::util::BroadcastEventStream;
use crate::util::advertise_state::{AdvertiseState, Advertising};

use super::jni_globals::{jvm, peripheral_class};

/// How long to wait for the stack's `AdvertiseCallback` before giving up.
/// Advertising either starts or fails almost immediately; this only bounds a
/// callback that never arrives at all.
const ADVERTISE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Kotlin's `BlePeripheralManager.ADVERTISE_OK`.
const ADVERTISE_OK: i32 = 0;
/// Kotlin's `BlePeripheralManager.ADVERTISE_ALREADY`. Kotlin guards
/// independently of the Rust slot, so this can still come back.
const ADVERTISE_ALREADY: i32 = 2;

struct PeripheralState {
    request_tx: mpsc::UnboundedSender<PeripheralRequest>,
    request_rx: Mutex<Option<mpsc::UnboundedReceiver<PeripheralRequest>>>,
    state_tx: broadcast::Sender<PeripheralStateEvent>,
    advertise: Mutex<AdvertiseState>,
}

/// Run `f` against the advertising state, if the backend is initialised.
fn with_advertise<T>(f: impl FnOnce(&mut AdvertiseState) -> T) -> Option<T> {
    let guard = STATE.lock();
    let s = guard.as_ref()?;
    let mut adv = s.advertise.lock();
    Some(f(&mut adv))
}

/// Claim the advertising slot, returning the request id and its waiter.
///
/// Android can only stop an advertisement by handing back the exact
/// `AdvertiseCallback` instance it was started with, so a second concurrent
/// start would strand the first one running with no way to stop it. Every
/// other backend already rejects this with `AlreadyAdvertising`.
fn register_advertise() -> BlewResult<(i32, oneshot::Receiver<BlewResult<()>>)> {
    with_advertise(AdvertiseState::register)
        .ok_or(BlewError::NotInitialized)?
        .ok_or(BlewError::AlreadyAdvertising)
}

/// Return the slot to `Idle` if `request_id` still owns it.
///
/// Applies to a request in either state: a caller that was dropped after the
/// stack confirmed its start still needs the advertisement torn down, since
/// nobody ever received the `Ok`.
fn release_advertise(request_id: i32) -> bool {
    with_advertise(|adv| adv.release(request_id)).unwrap_or(false)
}

/// Take the slot regardless of who owns it, for `stop_advertising`.
///
/// Returns the displaced state so the caller can wake a start that was still
/// in flight rather than leaving it to time out.
fn take_advertise() -> Advertising {
    with_advertise(AdvertiseState::take).unwrap_or(Advertising::Idle)
}

/// Releases the slot and tears down the stack-side request unless disarmed.
///
/// A guard rather than cleanup on the error path, because `start_advertising`
/// can also be *dropped* mid-await -- a `select!`, an outer timeout, a
/// cancelled task -- and that skips any cleanup written as ordinary code,
/// leaving the slot claimed and the radio advertising with nothing able to
/// reach it.
struct AdvertiseGuard {
    request_id: i32,
    armed: bool,
}

impl AdvertiseGuard {
    fn new(request_id: i32) -> Self {
        Self {
            request_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AdvertiseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        release_advertise(self.request_id);
        // Not conditional on that release succeeding. A stop landing between
        // this request claiming the slot and reaching Kotlin leaves Rust
        // `Idle` -- so the release reports someone else's slot -- while the
        // JNI call that follows can still start the radio, with nothing left
        // holding the id needed to stop it. Kotlin qualifies the teardown by
        // request id, so this is a no-op unless the live advertisement is ours.
        if let Err(e) = stop_platform_advertising(self.request_id) {
            warn!(
                "failed to stop advertising request {request_id}: {e}",
                request_id = self.request_id
            );
        }
    }
}

/// Ask Kotlin to tear down `request_id` if it is still the live one.
fn stop_platform_advertising(request_id: i32) -> Result<(), jni::errors::Error> {
    jvm().attach_current_thread(|env| {
        env.call_static_method(
            peripheral_class(),
            jni_str!("stopAdvertising"),
            jni_sig!("(I)V"),
            &[request_id.into()],
        )?;
        Ok::<_, jni::errors::Error>(())
    })
}

static STATE: Mutex<Option<PeripheralState>> = Mutex::new(None);

/// Deliver the stack's advertising outcome to a waiting `start_advertising`.
///
/// The `Starting` -> `Active` transition happens here, under the same lock
/// that takes the waiter and before the waiting task is woken. Doing it in the
/// woken task instead leaves a window where a `stop` can run against a state
/// that says "starting", and the resuming task then marks the slot active
/// after the platform has already been stopped -- permanently wedging every
/// later start on `AlreadyAdvertising`.
pub(crate) fn complete_advertise(request_id: i32, result: BlewResult<()>) {
    let tx = with_advertise(|adv| adv.complete(request_id, result.is_ok())).flatten();
    match tx {
        Some(tx) => {
            let _ = tx.send(result);
        }
        None => {
            tracing::debug!(
                request_id,
                "ignoring advertising result for a stale request"
            );
        }
    }
}

pub(crate) fn send_request(request: PeripheralRequest) {
    if let Some(s) = STATE.lock().as_ref() {
        let _ = s.request_tx.send(request);
    }
}

pub(crate) fn send_state_event(event: PeripheralStateEvent) {
    if let Some(s) = STATE.lock().as_ref() {
        let _ = s.state_tx.send(event);
    }
}

pub struct AndroidPeripheral;

impl AndroidPeripheral {
    pub async fn with_config(config: PeripheralConfig) -> BlewResult<Self> {
        let this = <Self as PeripheralBackend>::new().await?;
        super::l2cap_state::set_server_config(config.l2cap.clone());
        Ok(this)
    }
}

/// Issue one advertising request and wait for the stack's verdict.
async fn drive_advertising(
    config: &AdvertisingConfig,
    request_id: i32,
    rx: oneshot::Receiver<BlewResult<()>>,
) -> BlewResult<()> {
    let uuid_count = i32::try_from(config.service_uuids.len())
        .map_err(|_| BlewError::Internal("too many service UUIDs in AdvertisingConfig".into()))?;

    let code: i32 = jvm()
        .attach_current_thread(|env| {
            let name = env.new_string(&config.local_name)?;

            let string_class = env.find_class(jni_str!("java/lang/String"))?;
            let uuids: JObjectArray =
                env.new_object_array(uuid_count, &string_class, JObject::null())?;
            for (i, uuid) in config.service_uuids.iter().enumerate() {
                let s = env.new_string(uuid.to_string())?;
                uuids.set_element(env, i, &s)?;
            }

            env.call_static_method(
                peripheral_class(),
                jni_str!("startAdvertising"),
                jni_sig!("(Ljava/lang/String;[Ljava/lang/String;I)I"),
                &[(&name).into(), (&uuids).into(), request_id.into()],
            )?
            .i()
        })
        .map_err(|e| jni_err(&e))?;

    match code {
        ADVERTISE_OK => {}
        ADVERTISE_ALREADY => return Err(BlewError::AlreadyAdvertising),
        _ => {
            return Err(BlewError::Peripheral {
                source: "advertiser unavailable (is Bluetooth on?)".into(),
            });
        }
    }

    // The synchronous return only says the request reached the stack. Whether
    // advertising actually started is decided asynchronously, and frequently
    // is not -- too many advertisers, an unsupported payload size, a radio
    // that cannot advertise. Reporting Ok without waiting is how a peripheral
    // ends up silently invisible.
    match tokio::time::timeout(ADVERTISE_TIMEOUT, rx).await {
        Ok(Ok(result)) => {
            if result.is_ok() {
                debug!("advertising started");
            }
            result
        }
        Ok(Err(_)) => Err(BlewError::Peripheral {
            source: "advertising result dropped".into(),
        }),
        // Deliberately not BlewError::Timeout, which is reserved for
        // adapter-readiness waits.
        Err(_) => Err(BlewError::Peripheral {
            source: format!("advertising did not start within {ADVERTISE_TIMEOUT:?}").into(),
        }),
    }
}

impl backend::private::Sealed for AndroidPeripheral {}

impl PeripheralBackend for AndroidPeripheral {
    type StateEvents = BroadcastEventStream<PeripheralStateEvent>;
    type Requests = UnboundedReceiverStream<PeripheralRequest>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        // Distinguish "you forgot to register the plugin" from "the user said
        // no". Without this the former reports as PermissionDenied, because a
        // Kotlin manager with no Context answers the permission check exactly
        // as a denial does.
        if !super::jni_globals::is_initialized() {
            return Err(BlewError::NotInitialized);
        }
        if !super::are_ble_permissions_granted() {
            return Err(BlewError::PermissionDenied);
        }
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (state_tx, _) = broadcast::channel(256);
        *STATE.lock() = Some(PeripheralState {
            request_tx,
            request_rx: Mutex::new(Some(request_rx)),
            state_tx,
            advertise: Mutex::new(AdvertiseState::default()),
        });
        // The L2CAP statics are shared between the two roles but were only
        // initialised from the central path. A peripheral-only app would find
        // `set_server_config` silently doing nothing and `l2cap_listener`
        // panicking on the uninitialised state. Idempotent, so a process that
        // builds both roles is unaffected.
        super::l2cap_state::init_statics();
        debug!("AndroidPeripheral initialized");
        Ok(AndroidPeripheral)
    }

    async fn is_powered(&self) -> BlewResult<bool> {
        jvm()
            .attach_current_thread(|env| {
                let result = env.call_static_method(
                    peripheral_class(),
                    jni_str!("isPowered"),
                    jni_sig!("()Z"),
                    &[],
                )?;
                result.z()
            })
            .map_err(|e| jni_err(&e))
    }

    async fn add_service(&self, service: &GattService) -> BlewResult<()> {
        let service = service.clone();
        let n = service.characteristics.len();
        let n_i32 = i32::try_from(n)
            .map_err(|_| BlewError::Internal("too many characteristics for JNI".into()))?;
        jvm()
            .attach_current_thread(|env| {
                let service_uuid = env.new_string(service.uuid.to_string())?;

                let string_class = env.find_class(jni_str!("java/lang/String"))?;
                let byte_array_class = env.find_class(jni_str!("[B"))?;

                let char_uuids: JObjectArray =
                    env.new_object_array(n_i32, &string_class, JObject::null())?;
                let char_values: JObjectArray =
                    env.new_object_array(n_i32, &byte_array_class, JObject::null())?;
                let mut props_arr = vec![0_i32; n];
                let mut perms_arr = vec![0_i32; n];

                for (i, ch) in service.characteristics.iter().enumerate() {
                    let uuid_str = env.new_string(ch.uuid.to_string())?;
                    char_uuids.set_element(env, i, &uuid_str)?;

                    props_arr[i] = blew_props_to_android(ch.properties);
                    perms_arr[i] = blew_perms_to_android(ch.permissions);

                    let value = env.byte_array_from_slice(&ch.value)?;
                    char_values.set_element(env, i, &value)?;
                }

                let j_props = env.new_int_array(n)?;
                j_props.set_region(env, 0, &props_arr)?;

                let j_perms = env.new_int_array(n)?;
                j_perms.set_region(env, 0, &perms_arr)?;

                env.call_static_method(
                    peripheral_class(),
                    jni_str!("addService"),
                    jni_sig!("(Ljava/lang/String;[Ljava/lang/String;[I[I[[B)V"),
                    &[
                        (&service_uuid).into(),
                        (&char_uuids).into(),
                        (&j_props).into(),
                        (&j_perms).into(),
                        (&char_values).into(),
                    ],
                )?;

                Ok(())
            })
            .map_err(|e| jni_err(&e))?;

        debug!(uuid = %service.uuid, "added GATT service");
        Ok(())
    }

    async fn start_advertising(&self, config: &AdvertisingConfig) -> BlewResult<()> {
        // Claimed before the JNI call: AdvertiseCallback can fire before the
        // call has even returned.
        let (request_id, rx) = register_advertise()?;
        // Covers every way out, including this future being dropped mid-await.
        let mut guard = AdvertiseGuard::new(request_id);
        let result = drive_advertising(config, request_id, rx).await;
        if result.is_ok() {
            // `complete_advertise` already moved the slot to Active.
            guard.disarm();
        }
        result
    }

    async fn stop_advertising(&self) -> BlewResult<()> {
        // Take the slot whatever state it is in. Clearing only an `active`
        // flag left a start still in flight owning the slot, so a stop during
        // startup blocked every later start until that request timed out.
        let displaced = take_advertise();
        let request_id = displaced.request_id();
        if let Advertising::Starting(_, tx) = displaced {
            // Wake the start rather than leaving it on its deadline. Its
            // guard sees the error and tears the request down.
            let _ = tx.send(Err(BlewError::Peripheral {
                source: "advertising stopped before it started".into(),
            }));
        }

        // An empty slot means nothing of ours is advertising. Reaching into
        // Kotlin anyway would stop whatever is running there, and by this
        // point that can already be a newer start admitted after the slot was
        // freed.
        let Some(request_id) = request_id else {
            return Ok(());
        };

        stop_platform_advertising(request_id).map_err(|e| jni_err(&e))?;
        Ok(())
    }

    async fn notify_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
    ) -> BlewResult<()> {
        let device_addr = device_id.as_str().to_owned();
        // Retry loop: Kotlin returns 1 ("busy") when the previous
        // notification hasn't completed yet (onNotificationSent pending).
        // We retry with async sleep so we don't block the tokio thread.
        for attempt in 0..50_u32 {
            let status: i32 = jvm()
                .attach_current_thread(|env| {
                    let addr_str = env.new_string(&device_addr)?;
                    let uuid_str = env.new_string(char_uuid.to_string())?;
                    let j_value = env.byte_array_from_slice(&value)?;

                    let ret = env.call_static_method(
                        peripheral_class(),
                        jni_str!("notifyCharacteristic"),
                        jni_sig!("(Ljava/lang/String;Ljava/lang/String;[B)I"),
                        &[(&addr_str).into(), (&uuid_str).into(), (&j_value).into()],
                    )?;
                    ret.i()
                })
                .map_err(|e| jni_err(&e))?;

            match status {
                // 0 = success; 2 = no subscribers (not an error).
                0 | 2 => return Ok(()),
                1 => {
                    // Busy -- previous notification still in flight.
                    // Yield to tokio and retry after a short delay.
                    if attempt < 49 {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                3 => {
                    return Err(BlewError::LocalCharacteristicNotFound { char_uuid });
                }
                other => {
                    return Err(BlewError::Peripheral {
                        source: format!("notify returned unknown status {other}").into(),
                    });
                }
            }
        }
        // Exhausted retries -- treat as transient error.
        Err(BlewError::Peripheral {
            source: "notification busy after retries".into(),
        })
    }

    async fn l2cap_listener(
        &self,
    ) -> BlewResult<(
        Psm,
        impl futures_core::Stream<Item = BlewResult<(DeviceId, L2capChannel)>> + Send + 'static,
    )> {
        let (psm_tx, psm_rx) = oneshot::channel();
        super::l2cap_state::set_pending_server(psm_tx);

        let (accept_tx, accept_rx) = mpsc::unbounded_channel();
        super::l2cap_state::set_accept_tx(accept_tx);

        jvm()
            .attach_current_thread(|env| {
                env.call_static_method(
                    peripheral_class(),
                    jni_str!("openL2capServer"),
                    jni_sig!("()V"),
                    &[],
                )?;
                Ok(())
            })
            .map_err(|e| jni_err(&e))?;

        let psm = psm_rx
            .await
            .map_err(|_| BlewError::Internal("L2CAP server open cancelled".into()))??;

        Ok((psm, UnboundedReceiverStream::new(accept_rx)))
    }

    fn state_events(&self) -> Self::StateEvents {
        let receiver = STATE
            .lock()
            .as_ref()
            .expect("AndroidPeripheral not initialized")
            .state_tx
            .subscribe();
        BroadcastEventStream::new(receiver)
    }

    fn take_requests(&self) -> Option<Self::Requests> {
        let rx = {
            let guard = STATE.lock();
            guard.as_ref()?.request_rx.lock().take()
        };
        rx.map(UnboundedReceiverStream::new)
    }
}

fn jni_err(e: &jni::errors::Error) -> BlewError {
    BlewError::Internal(format!("JNI error: {e}"))
}

/// Convert blew CharacteristicProperties to Android BluetoothGattCharacteristic property bits.
fn blew_props_to_android(props: CharacteristicProperties) -> i32 {
    let mut out = 0_i32;
    if props.contains(CharacteristicProperties::BROADCAST) {
        out |= 0x01; // PROPERTY_BROADCAST
    }
    if props.contains(CharacteristicProperties::READ) {
        out |= 0x02; // PROPERTY_READ
    }
    if props.contains(CharacteristicProperties::WRITE_WITHOUT_RESPONSE) {
        out |= 0x04; // PROPERTY_WRITE_NO_RESPONSE
    }
    if props.contains(CharacteristicProperties::WRITE) {
        out |= 0x08; // PROPERTY_WRITE
    }
    if props.contains(CharacteristicProperties::NOTIFY) {
        out |= 0x10; // PROPERTY_NOTIFY
    }
    if props.contains(CharacteristicProperties::INDICATE) {
        out |= 0x20; // PROPERTY_INDICATE
    }
    out
}

/// Convert blew AttributePermissions to Android permission bits.
fn blew_perms_to_android(perms: crate::gatt::props::AttributePermissions) -> i32 {
    use crate::gatt::props::AttributePermissions;
    let mut out = 0_i32;
    if perms.contains(AttributePermissions::READ) {
        out |= 0x01; // PERMISSION_READ
    }
    if perms.contains(AttributePermissions::WRITE) {
        out |= 0x10; // PERMISSION_WRITE
    }
    if perms.contains(AttributePermissions::READ_ENCRYPTED) {
        out |= 0x02; // PERMISSION_READ_ENCRYPTED
    }
    if perms.contains(AttributePermissions::WRITE_ENCRYPTED) {
        out |= 0x20; // PERMISSION_WRITE_ENCRYPTED
    }
    out
}
