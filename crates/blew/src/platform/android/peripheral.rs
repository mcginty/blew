use std::collections::HashMap;
use std::sync::Arc;

use jni::objects::{JObject, JObjectArray};
use jni::{jni_sig, jni_str};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::error::{BlewError, BlewResult};
use crate::gatt::props::CharacteristicProperties;
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, L2capEncryption, types::Psm};
use crate::peripheral::backend::{self, PeripheralBackend};
use crate::peripheral::types::{
    AdvertisingConfig, Delivery, LocalName, PeripheralConfig, PeripheralRequest,
    PeripheralStateEvent,
};
use crate::types::DeviceId;
use crate::util::advertise_state::{AdvertiseState, Advertising};
use crate::util::{BroadcastEventStream, KeyedRequestMap};

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
/// Kotlin's `BlePeripheralManager.ADVERTISE_NAME_REJECTED`.
const ADVERTISE_NAME_REJECTED: i32 = 3;
/// Kotlin's `BlePeripheralManager.ADVERTISE_RENAME_BUSY`.
const ADVERTISE_RENAME_BUSY: i32 = 4;
/// Kotlin's `BlePeripheralManager.ADVERTISE_FAILED_RENAME_UNCONFIRMED`, reported
/// through `nativeOnAdvertisingResult` in place of an `AdvertiseCallback` error.
pub(super) const ADVERTISE_FAILED_RENAME_UNCONFIRMED: i32 = -1;

/// Kotlin's `BlePeripheralManager.NOTIFY_*` results from `notifyCharacteristic`.
const NOTIFY_SENT: i32 = 0;
const NOTIFY_INDICATED: i32 = 1;
const NOTIFY_NOT_SUBSCRIBED: i32 = 2;
const NOTIFY_CHAR_NOT_FOUND: i32 = 3;
const NOTIFY_REJECTED: i32 = 4;

/// How long a send may wait for `onNotificationSent`. Past the ATT transaction
/// timeout, so a central that never confirms normally ends the wait itself by
/// losing the link; this only bounds a stack that never reports at all.
const NOTIFY_COMPLETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);

/// How long `set_adapter_name` waits for Kotlin's verdict. Kotlin fails an
/// unconfirmed rename itself after a second; this only bounds a verdict that
/// never arrives.
const RENAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Kotlin's `BlePeripheralManager.RENAME_OK`.
const RENAME_OK: i32 = 0;
/// Kotlin's `BlePeripheralManager.RENAME_REJECTED`.
const RENAME_REJECTED: i32 = 2;
/// Kotlin's `BlePeripheralManager.RENAME_BUSY`.
const RENAME_BUSY: i32 = 3;

/// Refusal for a rename requested while another is still waiting to land.
const RENAME_IN_PROGRESS: &str = "a Bluetooth adapter rename is already waiting to take effect";

static NEXT_RENAME_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

struct PeripheralState {
    request_tx: mpsc::UnboundedSender<PeripheralRequest>,
    request_rx: Mutex<Option<mpsc::UnboundedReceiver<PeripheralRequest>>>,
    state_tx: broadcast::Sender<PeripheralStateEvent>,
    advertise: Mutex<AdvertiseState>,
    notifies: Mutex<Notifies>,
    renames: KeyedRequestMap<i32, oneshot::Sender<bool>>,
}

/// Per-device notification sends.
///
/// The stack takes one value per device until `onNotificationSent`; a second
/// sent before then is dropped or refused as busy. `gates` admits one send per
/// device, and `in_flight` is the send the next `onNotificationSent` for that
/// device belongs to.
#[derive(Default)]
struct Notifies {
    gates: HashMap<String, Arc<Semaphore>>,
    in_flight: HashMap<String, InFlightNotify>,
    next_token: u64,
}

struct InFlightNotify {
    /// Lets the sending task withdraw exactly its own entry. It never crosses
    /// JNI: the stack's callback names only the device.
    token: u64,
    done: oneshot::Sender<BlewResult<()>>,
    /// Released when the stack reports, not when the caller stops waiting: a
    /// caller dropped mid-send must not admit the next send while this one is
    /// still in the stack.
    _permit: OwnedSemaphorePermit,
}

fn with_notifies<T>(f: impl FnOnce(&mut Notifies) -> T) -> Option<T> {
    let guard = STATE.lock();
    let s = guard.as_ref()?;
    let mut notifies = s.notifies.lock();
    Some(f(&mut notifies))
}

/// Register `addr`'s send before handing it to Kotlin, which can report
/// `onNotificationSent` before the JNI call returns.
fn begin_notify(
    addr: &str,
    permit: OwnedSemaphorePermit,
) -> BlewResult<(u64, oneshot::Receiver<BlewResult<()>>)> {
    let (done, rx) = oneshot::channel();
    let token = with_notifies(|n| {
        let token = n.next_token;
        n.next_token += 1;
        n.in_flight.insert(
            addr.to_owned(),
            InFlightNotify {
                token,
                done,
                _permit: permit,
            },
        );
        token
    })
    .ok_or(BlewError::NotInitialized)?;
    Ok((token, rx))
}

/// Resolve `addr`'s in-flight send with the stack's verdict and admit the
/// next one.
pub(crate) fn finish_notify(addr: &str, result: BlewResult<()>) {
    with_notifies(|n| {
        if let Some(entry) = n.in_flight.remove(addr) {
            let _ = entry.done.send(result);
        }
        n.forget_idle_gate(addr);
    });
}

/// Withdraw a send the stack never accepted, or gave up on, if it is still
/// the one registered for `addr`.
fn abandon_notify(addr: &str, token: u64) {
    with_notifies(|n| {
        if n.in_flight.get(addr).is_some_and(|e| e.token == token) {
            n.in_flight.remove(addr);
        }
        n.forget_idle_gate(addr);
    });
}

impl Notifies {
    /// Forget `addr`'s gate once nobody holds or awaits it, so devices that
    /// come and go don't accumulate. Clones are only taken under the same lock.
    fn forget_idle_gate(&mut self, addr: &str) {
        if self
            .gates
            .get(addr)
            .is_some_and(|g| Arc::strong_count(g) == 1)
        {
            self.gates.remove(addr);
        }
    }
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

/// Deliver Kotlin's verdict to a waiting `set_adapter_name`.
pub(crate) fn complete_rename(request_id: i32, success: bool) {
    let tx = STATE
        .lock()
        .as_ref()
        .and_then(|s| s.renames.take(&request_id));
    if let Some(tx) = tx {
        let _ = tx.send(success);
    } else {
        debug!(request_id, "ignoring rename result for a stale request");
    }
}

/// Drops a rename's waiter however `set_adapter_name` exits, including being
/// dropped mid-await.
struct RenameGuard(i32);

impl Drop for RenameGuard {
    fn drop(&mut self) {
        if let Some(s) = STATE.lock().as_ref() {
            s.renames.take(&self.0);
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

pub struct AndroidPeripheral {
    /// Owned per instance, not stored in the process-global L2CAP state: a
    /// second `Peripheral` constructed with a different config must not be
    /// able to relax a listener this one published. See the warning on
    /// `l2cap_state::L2capState::client_config`.
    l2cap_encryption: L2capEncryption,
}

impl AndroidPeripheral {
    pub async fn with_config(config: PeripheralConfig) -> BlewResult<Self> {
        let mut this = <Self as PeripheralBackend>::new().await?;
        super::l2cap_state::set_server_config(config.l2cap.clone());
        this.l2cap_encryption = config.l2cap.encryption;
        Ok(this)
    }

    pub fn adapter_name(&self) -> BlewResult<Option<String>> {
        jvm()
            .attach_current_thread(|env| {
                let name = env
                    .call_static_method(
                        peripheral_class(),
                        jni_str!("getAdapterName"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let name = env.cast_local::<jni::objects::JString>(name)?;
                if name.is_null() {
                    return Ok(None);
                }
                Ok(Some(name.try_to_string(env)?))
            })
            .map_err(|e| jni_err(&e))
    }

    pub async fn set_adapter_name(&self, name: &str) -> BlewResult<()> {
        let request_id = NEXT_RENAME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // Registered before the JNI call: a name already in place is confirmed
        // before the call returns.
        STATE
            .lock()
            .as_ref()
            .ok_or(BlewError::NotInitialized)?
            .renames
            .insert(request_id, tx);
        let _guard = RenameGuard(request_id);

        let code: i32 = jvm()
            .attach_current_thread(|env| {
                let name = env.new_string(name)?;
                env.call_static_method(
                    peripheral_class(),
                    jni_str!("setAdapterName"),
                    jni_sig!("(Ljava/lang/String;I)I"),
                    &[(&name).into(), request_id.into()],
                )?
                .i()
            })
            .map_err(|e| jni_err(&e))?;
        match code {
            RENAME_OK => {}
            RENAME_REJECTED => {
                return Err(BlewError::Peripheral {
                    source: "Android refused to rename the Bluetooth adapter \
                             (is Bluetooth on, and BLUETOOTH_CONNECT granted?)"
                        .into(),
                });
            }
            RENAME_BUSY => {
                return Err(BlewError::Peripheral {
                    source: RENAME_IN_PROGRESS.into(),
                });
            }
            _ => return Err(BlewError::NotInitialized),
        }

        match tokio::time::timeout(RENAME_TIMEOUT, rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(BlewError::Peripheral {
                source: "the Bluetooth adapter rename didn't take effect".into(),
            }),
            Ok(Err(_)) => Err(BlewError::Peripheral {
                source: "adapter rename result dropped".into(),
            }),
            Err(_) => Err(BlewError::Peripheral {
                source: format!("adapter rename unconfirmed after {RENAME_TIMEOUT:?}").into(),
            }),
        }
    }
}

/// The name Kotlin may rename the adapter to, or `None` to leave it alone.
fn rename_target(local_name: &LocalName) -> BlewResult<Option<&str>> {
    match local_name {
        LocalName::None => Ok(None),
        LocalName::AllowPermanent(name) => Ok(Some(name)),
        LocalName::Temporary(_) => Err(BlewError::LocalNameUnsupported {
            reason: "Android can only advertise the adapter's own name; \
                     LocalName::AllowPermanent renames the adapter to do it",
        }),
    }
}

/// Issue one advertising request and wait for the stack's verdict.
async fn drive_advertising(
    config: &AdvertisingConfig,
    adapter_name: Option<&str>,
    request_id: i32,
    rx: oneshot::Receiver<BlewResult<()>>,
) -> BlewResult<()> {
    let uuid_count = i32::try_from(config.service_uuids.len())
        .map_err(|_| BlewError::Internal("too many service UUIDs in AdvertisingConfig".into()))?;

    let code: i32 = jvm()
        .attach_current_thread(|env| {
            let name = match adapter_name {
                Some(name) => JObject::from(env.new_string(name)?),
                None => JObject::null(),
            };

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
        ADVERTISE_NAME_REJECTED => {
            return Err(BlewError::Peripheral {
                source: "Android refused to rename the Bluetooth adapter".into(),
            });
        }
        ADVERTISE_RENAME_BUSY => {
            return Err(BlewError::Peripheral {
                source: RENAME_IN_PROGRESS.into(),
            });
        }
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
            notifies: Mutex::new(Notifies::default()),

            renames: KeyedRequestMap::new(),
        });
        // The L2CAP statics are shared between the two roles but were only
        // initialised from the central path. A peripheral-only app would find
        // `set_server_config` silently doing nothing and `l2cap_listener`
        // panicking on the uninitialised state. Idempotent, so a process that
        // builds both roles is unaffected.
        super::l2cap_state::init_statics();
        debug!("AndroidPeripheral initialized");
        Ok(AndroidPeripheral {
            l2cap_encryption: L2capEncryption::default(),
        })
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
        let adapter_name = rename_target(&config.local_name)?;
        // Claimed before the JNI call: AdvertiseCallback can fire before the
        // call has even returned.
        let (request_id, rx) = register_advertise()?;
        // Covers every way out, including this future being dropped mid-await.
        let mut guard = AdvertiseGuard::new(request_id);
        let result = drive_advertising(config, adapter_name, request_id, rx).await;
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
    ) -> BlewResult<Delivery> {
        let device_addr = device_id.as_str().to_owned();
        let gate = with_notifies(|n| {
            Arc::clone(
                n.gates
                    .entry(device_addr.clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(1))),
            )
        })
        .ok_or(BlewError::NotInitialized)?;
        let permit = gate
            .acquire_owned()
            .await
            .map_err(|_| BlewError::Internal("notification gate closed".into()))?;
        let (token, rx) = begin_notify(&device_addr, permit)?;

        let status = jvm()
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
            .map_err(|e| jni_err(&e));

        let delivery = match status {
            Ok(NOTIFY_SENT) => Delivery::Sent,
            Ok(NOTIFY_INDICATED) => Delivery::Confirmed,
            other => {
                abandon_notify(&device_addr, token);
                return match other? {
                    NOTIFY_NOT_SUBSCRIBED => Ok(Delivery::NoSubscriber),
                    NOTIFY_CHAR_NOT_FOUND => {
                        Err(BlewError::LocalCharacteristicNotFound { char_uuid })
                    }
                    NOTIFY_REJECTED => Err(BlewError::Peripheral {
                        source: "the stack refused the notification".into(),
                    }),
                    unknown => Err(BlewError::Peripheral {
                        source: format!("notify returned unknown status {unknown}").into(),
                    }),
                };
            }
        };

        match tokio::time::timeout(NOTIFY_COMPLETE_TIMEOUT, rx).await {
            Ok(Ok(result)) => result.map(|()| delivery),
            Ok(Err(_)) => Err(BlewError::Internal(
                "peripheral shut down before the notification completed".into(),
            )),
            Err(_) => {
                abandon_notify(&device_addr, token);
                Err(BlewError::Peripheral {
                    source: format!(
                        "stack did not report the notification within {NOTIFY_COMPLETE_TIMEOUT:?}"
                    )
                    .into(),
                })
            }
        }
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

        let secure = super::l2cap_state::secure_flag(self.l2cap_encryption);
        jvm()
            .attach_current_thread(|env| {
                env.call_static_method(
                    peripheral_class(),
                    jni_str!("openL2capServer"),
                    jni_sig!("(Z)V"),
                    &[secure.into()],
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
