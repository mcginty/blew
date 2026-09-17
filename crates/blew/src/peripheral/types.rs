use crate::l2cap::L2capConfig;
use crate::types::DeviceId;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Configuration for initialising the peripheral role.
///
/// Construct with `..Default::default()` so a new field costs you one
/// recompile rather than an edit at every call site.
#[derive(Debug, Clone, Default)]
pub struct PeripheralConfig {
    /// On Apple platforms, passed as `CBPeripheralManagerOptionRestoreIdentifierKey` to
    /// `initWithDelegate:queue:options:`, enabling state restoration for the app's
    /// background BLE peripheral session. Ignored on all other platforms.
    ///
    /// See the crate-level "State restoration" docs for the iOS usage contract
    /// (entitlements, event-drain rules, L2CAP re-open requirement).
    pub restore_identifier: Option<String>,
    /// Tuning applied to L2CAP channels accepted by this peripheral.
    pub l2cap: L2capConfig,
}

/// Non-request events from the peripheral role. Clone-able; multiple subscribers welcome.
#[derive(Debug, Clone)]
pub enum PeripheralStateEvent {
    /// The local Bluetooth adapter was powered on or off.
    AdapterStateChanged { powered: bool },

    /// A remote central subscribed to or unsubscribed from a characteristic's notifications.
    SubscriptionChanged {
        client_id: DeviceId,
        char_uuid: Uuid,
        subscribed: bool,
    },
}

/// Inbound GATT requests from remote centrals.
///
/// Each variant carries an owned responder that must be consumed exactly once,
/// or dropped (which sends an ATT Application Error automatically). Single-consumer:
/// the request stream is handed out via
/// [`Peripheral::take_requests`](crate::peripheral::Peripheral::take_requests).
#[derive(Debug)]
pub enum PeripheralRequest {
    /// A remote central sent an ATT Read Request.
    Read {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        offset: u16,
        responder: ReadResponder,
    },

    /// A remote central sent an ATT Write Request or Write Command.
    /// `responder` is `Some` for Write Request (needs an ATT response) and
    /// `None` for Write Without Response.
    Write {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        /// Byte offset into the characteristic value at which `value` starts.
        ///
        /// Zero for an ordinary write. A central writing a payload larger than
        /// `MTU - 3` splits it across several requests with ascending offsets,
        /// so an application reassembling long writes must splice `value` in at
        /// `offset` rather than treating each request as a whole value.
        offset: u16,
        value: Vec<u8>,
        responder: Option<WriteResponder>,
    },
}

pub(crate) type ReadResponseTx = oneshot::Sender<Result<Vec<u8>, ()>>;
pub(crate) type WriteResponseTx = oneshot::Sender<bool>;

#[derive(Debug)]
struct ResponderInner<T: Send + Clone + 'static> {
    tx: Option<oneshot::Sender<T>>,
    error_value: T,
}

impl<T: Send + Clone + 'static> ResponderInner<T> {
    fn new(tx: oneshot::Sender<T>, error_value: T) -> Self {
        Self {
            tx: Some(tx),
            error_value,
        }
    }

    fn send(&mut self, value: T) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(value);
        }
    }
}

impl<T: Send + Clone + 'static> Drop for ResponderInner<T> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(self.error_value.clone());
        }
    }
}

/// Owned handle for responding to a GATT read request.
///
/// Call [`respond`](Self::respond) to send a value or [`error`](Self::error) to send an ATT
/// Application Error. If this is dropped without being consumed, an ATT Application Error is
/// sent automatically.
#[derive(Debug)]
pub struct ReadResponder(ResponderInner<Result<Vec<u8>, ()>>);

impl ReadResponder {
    pub(crate) fn new(tx: ReadResponseTx) -> Self {
        Self(ResponderInner::new(tx, Err(())))
    }

    /// Send `value` as the read response.
    pub fn respond(mut self, value: Vec<u8>) {
        self.0.send(Ok(value));
    }

    /// Send an ATT Application Error response.
    pub fn error(mut self) {
        self.0.send(Err(()));
    }
}

/// Owned handle for acknowledging a GATT write request.
///
/// Call [`success`](Self::success) or [`error`](Self::error). Dropping without consuming
/// sends an error response automatically.
#[derive(Debug)]
pub struct WriteResponder(ResponderInner<bool>);

impl WriteResponder {
    pub(crate) fn new(tx: WriteResponseTx) -> Self {
        Self(ResponderInner::new(tx, false))
    }

    /// Acknowledge the write successfully.
    pub fn success(mut self) {
        self.0.send(true);
    }

    /// Send an ATT Application Error response.
    pub fn error(mut self) {
        self.0.send(false);
    }
}

/// Configuration for a BLE advertisement.
///
/// Construct with `..Default::default()` so a new field costs you one
/// recompile rather than an edit at every call site.
///
/// # Platform caveats
///
/// Manufacturer-specific data is deliberately absent. CoreBluetooth's
/// `startAdvertising:` accepts only `CBAdvertisementDataLocalNameKey` and
/// `CBAdvertisementDataServiceUUIDsKey`, so an Apple peripheral cannot
/// advertise it at all; exposing the field here would make it a silent no-op
/// on one of the three backends. Manufacturer data *received* while scanning
/// is available on [`BleDevice::manufacturer_data`](crate::BleDevice).
#[derive(Debug, Clone, Default)]
pub struct AdvertisingConfig {
    /// Name to advertise, and how long it may last. See [`LocalName`].
    pub local_name: LocalName,
    pub service_uuids: Vec<Uuid>,
}

/// Name to advertise, and how long its effects may last.
///
/// Apple and Linux put the name in the advertisement itself, so it lasts only
/// as long as the advertisement. Android can't do that: `AdvertiseData` offers
/// only `setIncludeDeviceName(boolean)`, which reads `BluetoothAdapter.getName()`.
/// The only way to advertise a chosen name there is to rename the adapter. That
/// name is device-wide: the car, the headphones and every pairing dialog show
/// it. A rename also persists until something changes it back, so the variants
/// differ in how long the name is allowed to last.
///
/// A backend that can't provide the requested variant returns
/// [`BlewError::LocalNameUnsupported`](crate::error::BlewError::LocalNameUnsupported).
/// It never quietly advertises no name, because a peer matching on the name
/// would then never find this peripheral.
///
/// # Platform mapping
///
/// | | `None` | `Temporary` | `AllowPermanent` |
/// |---|---|---|---|
/// | Apple | no `CBAdvertisementDataLocalNameKey` | `CBAdvertisementDataLocalNameKey` | as `Temporary` |
/// | Linux | no BlueZ `LocalName` | BlueZ `LocalName` | as `Temporary` |
/// | Android | `setIncludeDeviceName(false)` | unsupported | adapter rename, left in place |
///
/// On Apple and Linux, `AllowPermanent` behaves exactly like `Temporary`.
///
/// **iOS:** CoreBluetooth drops the local name from advertisements while the
/// app is in the background, whichever variant is used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LocalName {
    /// No name in the advertising data, which leaves its bytes for the service
    /// UUIDs; peers identify the peripheral by those. After connecting, a peer
    /// may still read the OS's device name from the GAP Device Name
    /// characteristic.
    #[default]
    None,

    /// Advertise the name only in the advertisement, with no effect beyond it.
    /// It stops appearing when advertising stops, and nothing else on the
    /// device changes.
    ///
    /// Unsupported on Android, which has no way to name a single advertisement.
    /// Use [`AllowPermanent`](Self::AllowPermanent) there if a device-wide
    /// rename is acceptable.
    Temporary(String),

    /// Advertise the name, even if a platform has to rename the whole device
    /// to do it, and leave that rename in place.
    ///
    /// On Android this sets the Bluetooth adapter's name, which every app and
    /// paired device sees, and it stays set after advertising stops and after
    /// the app exits. blew doesn't record or restore the previous name: a
    /// restore can't be made reliable from inside one app (an uninstall, a
    /// killed process, or another app renaming the adapter in the meantime
    /// all defeat it), so putting a name back is left to the application. The
    /// Android-only `Peripheral::adapter_name` and `Peripheral::set_adapter_name`
    /// read and write it for that.
    ///
    /// Android applies a rename asynchronously, and an advertisement carries
    /// whichever name is in place when it starts, so blew only starts
    /// advertising once the new name has taken effect. If it hasn't within a
    /// second, [`Peripheral::start_advertising`](crate::Peripheral::start_advertising)
    /// fails rather than advertising the previous name.
    ///
    /// On Apple and Linux this is identical to [`Temporary`](Self::Temporary).
    AllowPermanent(String),
}

impl LocalName {
    /// The name to put on air, for backends where both naming variants mean
    /// the same thing. Android distinguishes them, so it doesn't use this.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Temporary(name) | Self::AllowPermanent(name) => Some(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_read_responder_respond() {
        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);
        responder.respond(b"hello".to_vec());
        assert_eq!(rx.await.unwrap().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn test_write_responder_success() {
        let (tx, rx) = oneshot::channel();
        let responder = WriteResponder::new(tx);
        responder.success();
        assert!(rx.await.unwrap());
    }

    use proptest::collection::vec;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum ReadAction {
        Drop,
        Respond(Vec<u8>),
        Error,
    }

    fn read_action() -> impl Strategy<Value = ReadAction> {
        prop_oneof![
            Just(ReadAction::Drop),
            vec(any::<u8>(), 0..32).prop_map(ReadAction::Respond),
            Just(ReadAction::Error),
        ]
    }

    #[derive(Debug, Clone)]
    enum WriteAction {
        Drop,
        Success,
        Error,
    }

    fn write_action() -> impl Strategy<Value = WriteAction> {
        prop_oneof![
            Just(WriteAction::Drop),
            Just(WriteAction::Success),
            Just(WriteAction::Error),
        ]
    }

    proptest! {
        /// For any `ReadResponder` action (respond / error / drop), the receiver
        /// observes exactly one value with the correct polarity. Drop is
        /// equivalent to `error()`.
        #[test]
        fn read_responder_value_matches_action(actions in vec(read_action(), 0..64)) {
            for action in actions {
                let (tx, mut rx) = oneshot::channel::<Result<Vec<u8>, ()>>();
                let responder = ReadResponder::new(tx);
                let expected: Result<Vec<u8>, ()> = match action {
                    ReadAction::Drop => {
                        drop(responder);
                        Err(())
                    }
                    ReadAction::Respond(v) => {
                        let expected = Ok(v.clone());
                        responder.respond(v);
                        expected
                    }
                    ReadAction::Error => {
                        responder.error();
                        Err(())
                    }
                };
                let got = rx
                    .try_recv()
                    .expect("responder must deliver a value synchronously");
                prop_assert_eq!(got, expected);
            }
        }

        /// For any `WriteResponder` action (success / error / drop), the receiver
        /// observes exactly one `bool` with the correct polarity. Drop is
        /// equivalent to `error()` (both deliver `false`).
        #[test]
        fn write_responder_value_matches_action(actions in vec(write_action(), 0..64)) {
            for action in actions {
                let (tx, mut rx) = oneshot::channel::<bool>();
                let responder = WriteResponder::new(tx);
                let expected = match action {
                    WriteAction::Drop => {
                        drop(responder);
                        false
                    }
                    WriteAction::Success => {
                        responder.success();
                        true
                    }
                    WriteAction::Error => {
                        responder.error();
                        false
                    }
                };
                let got = rx
                    .try_recv()
                    .expect("responder must deliver a value synchronously");
                prop_assert_eq!(got, expected);
            }
        }
    }
}
