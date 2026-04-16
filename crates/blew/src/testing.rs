//! In-memory mock backends for testing without BLE hardware.
//!
//! ```rust,no_run
//! use blew::testing::MockLink;
//! use blew::{Central, Peripheral};
//!
//! # async fn example() -> blew::BlewResult<()> {
//! let (central_link, peripheral_link) = MockLink::pair();
//! let central: Central<_> = Central::from_backend(central_link.central);
//! let peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::central::backend::{self, CentralBackend};
use crate::central::types::{CentralEvent, DisconnectCause, ScanFilter, WriteType};
use crate::error::{BlewError, BlewResult};
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, types::Psm};
use crate::peripheral::backend::{self as periph_backend, PeripheralBackend};
use crate::peripheral::types::{AdvertisingConfig, PeripheralEvent, ReadResponder, WriteResponder};
use crate::types::{BleDevice, DeviceId};
/// Policy for injecting L2CAP failures in mock tests.
#[derive(Debug, Clone, Default)]
pub struct MockL2capPolicy {
    pub listener_error: Option<MockErrorKind>,
    pub open_error: Option<MockErrorKind>,
}

/// Error kinds for [`MockL2capPolicy`]. Avoids `BlewError`'s non-`Clone` variants.
#[derive(Debug, Clone, Copy)]
pub enum MockErrorKind {
    /// Maps to [`BlewError::NotSupported`].
    NotSupported,
    /// Maps to [`BlewError::Internal`] with a test-supplied message.
    Internal,
}

impl MockErrorKind {
    fn to_error(self) -> BlewError {
        match self {
            Self::NotSupported => BlewError::NotSupported,
            Self::Internal => BlewError::Internal("mock l2cap policy rejected".into()),
        }
    }
}
struct LinkState {
    services: Vec<GattService>,
    char_values: HashMap<Uuid, Vec<u8>>,
    subscriptions: HashMap<Uuid, mpsc::UnboundedSender<Bytes>>,
    advertising: bool,
    adv_config: Option<AdvertisingConfig>,
    connected: bool,
    l2cap_policy: MockL2capPolicy,
    l2cap_psm: Option<crate::l2cap::types::Psm>,
    l2cap_accept_tx: Option<tokio::sync::mpsc::UnboundedSender<BlewResult<(DeviceId, L2capChannel)>>>,
}

type SharedLink = Arc<Mutex<LinkState>>;
/// Central side of a [`MockLink`].
pub struct MockLinkEndpoint {
    pub central: MockCentral,
}

/// Peripheral side of a [`MockLink`].
pub struct MockPeripheralEndpoint {
    pub peripheral: MockPeripheral,
}

/// Factory for paired mock central + peripheral over an in-memory link.
pub struct MockLink;

impl MockLink {
    /// Create a mock central and mock peripheral wired together.
    #[must_use]
    pub fn pair() -> (MockLinkEndpoint, MockPeripheralEndpoint) {
        let link = Arc::new(Mutex::new(LinkState {
            services: Vec::new(),
            char_values: HashMap::new(),
            subscriptions: HashMap::new(),
            advertising: false,
            adv_config: None,
            connected: false,
            l2cap_policy: MockL2capPolicy::default(),
            l2cap_psm: None,
            l2cap_accept_tx: None,
        }));

        let (central_event_tx, central_event_rx) = mpsc::unbounded_channel();
        let (periph_event_tx, periph_event_rx) = mpsc::unbounded_channel();

        let _ = central_event_tx.send(CentralEvent::AdapterStateChanged { powered: true });

        let central = MockCentral {
            link: Arc::clone(&link),
            event_tx: central_event_tx.clone(),
            periph_sender_keepalive: Arc::new(Mutex::new(Some(periph_event_tx.clone()))),
            central_rx: Arc::new(Mutex::new(Some(central_event_rx))),
            periph_event_tx,
            powered: Arc::new(Mutex::new(true)),
        };

        let peripheral = MockPeripheral {
            link: Arc::clone(&link),
            event_tx: Arc::new(Mutex::new(Some(periph_event_rx))),
            emit_tx: None,
            central_sender_keepalive: central_event_tx.clone(),
            powered: Arc::new(Mutex::new(true)),
        };

        (
            MockLinkEndpoint { central },
            MockPeripheralEndpoint { peripheral },
        )
    }

    /// Like [`pair`](Self::pair) but with a custom L2CAP failure-injection policy.
    #[must_use]
    pub fn pair_with_policy(policy: MockL2capPolicy) -> (MockLinkEndpoint, MockPeripheralEndpoint) {
        let (central_ep, periph_ep) = Self::pair();
        central_ep.central.link.lock().unwrap().l2cap_policy = policy;
        (central_ep, periph_ep)
    }
}
/// In-memory mock [`CentralBackend`].
pub struct MockCentral {
    link: SharedLink,
    event_tx: mpsc::UnboundedSender<CentralEvent>,
    periph_sender_keepalive: Arc<Mutex<Option<mpsc::UnboundedSender<PeripheralEvent>>>>,
    central_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<CentralEvent>>>>,
    periph_event_tx: mpsc::UnboundedSender<PeripheralEvent>,
    powered: Arc<Mutex<bool>>,
}

impl Clone for MockCentral {
    fn clone(&self) -> Self {
        Self {
            link: Arc::clone(&self.link),
            event_tx: self.event_tx.clone(),
            periph_sender_keepalive: Arc::clone(&self.periph_sender_keepalive),
            central_rx: Arc::clone(&self.central_rx),
            periph_event_tx: self.periph_event_tx.clone(),
            powered: Arc::clone(&self.powered),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl MockCentral {
    /// Construct a standalone powered mock central (not linked to any peripheral).
    #[must_use]
    pub fn new_powered() -> crate::central::Central<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (periph_event_tx, _periph_event_rx) = mpsc::unbounded_channel();
        let link = Arc::new(Mutex::new(LinkState {
            services: Vec::new(),
            char_values: std::collections::HashMap::new(),
            subscriptions: std::collections::HashMap::new(),
            advertising: false,
            adv_config: None,
            connected: false,
            l2cap_policy: MockL2capPolicy::default(),
            l2cap_psm: None,
            l2cap_accept_tx: None,
        }));
        crate::central::Central::from_backend(Self {
            link,
            event_tx,
            periph_sender_keepalive: Arc::new(Mutex::new(None)),
            central_rx: Arc::new(Mutex::new(Some(event_rx))),
            periph_event_tx,
            powered: Arc::new(Mutex::new(true)),
        })
    }

    /// Construct a standalone unpowered mock central (not linked to any peripheral).
    #[must_use]
    pub fn new_unpowered() -> crate::central::Central<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (periph_event_tx, _periph_event_rx) = mpsc::unbounded_channel();
        let link = Arc::new(Mutex::new(LinkState {
            services: Vec::new(),
            char_values: std::collections::HashMap::new(),
            subscriptions: std::collections::HashMap::new(),
            advertising: false,
            adv_config: None,
            connected: false,
            l2cap_policy: MockL2capPolicy::default(),
            l2cap_psm: None,
            l2cap_accept_tx: None,
        }));
        crate::central::Central::from_backend(Self {
            link,
            event_tx,
            periph_sender_keepalive: Arc::new(Mutex::new(None)),
            central_rx: Arc::new(Mutex::new(Some(event_rx))),
            periph_event_tx,
            powered: Arc::new(Mutex::new(false)),
        })
    }

    /// Emit an `AdapterStateChanged` event and update the powered flag.
    pub fn mock_emit_adapter_state(&self, powered: bool) {
        *self.powered.lock().unwrap() = powered;
        let _ = self
            .event_tx
            .send(CentralEvent::AdapterStateChanged { powered });
    }
}

impl backend::private::Sealed for MockCentral {}

impl CentralBackend for MockCentral {
    type EventStream = tokio_stream::wrappers::UnboundedReceiverStream<CentralEvent>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        Err(BlewError::Internal("use MockLink::pair() instead".into()))
    }

    fn is_powered(&self) -> impl Future<Output = BlewResult<bool>> + Send {
        let powered = *self.powered.lock().unwrap();
        async move { Ok(powered) }
    }

    fn start_scan(&self, _filter: ScanFilter) -> impl Future<Output = BlewResult<()>> + Send {
        let link = self.link.lock().unwrap();
        let advertising = link.advertising;
        let adv_config = link.adv_config.clone();
        let services = link.services.iter().map(|s| s.uuid).collect::<Vec<_>>();
        let tx = self.event_tx.clone();
        async move {
            if advertising {
                let name = adv_config.map(|c| c.local_name);
                let _ = tx.send(CentralEvent::DeviceDiscovered(BleDevice {
                    id: DeviceId::from("mock-peripheral"),
                    name,
                    rssi: Some(-50),
                    services,
                }));
            }
            Ok(())
        }
    }

    async fn stop_scan(&self) -> BlewResult<()> {
        Ok(())
    }

    async fn discovered_devices(&self) -> BlewResult<Vec<BleDevice>> {
        Ok(vec![])
    }

    fn connect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let mut link = self.link.lock().unwrap();
        link.connected = true;
        let id = device_id.clone();
        let tx = self.event_tx.clone();
        async move {
            let _ = tx.send(CentralEvent::DeviceConnected { device_id: id });
            Ok(())
        }
    }

    fn disconnect(&self, device_id: &DeviceId) -> impl Future<Output = BlewResult<()>> + Send {
        let mut link = self.link.lock().unwrap();
        link.connected = false;
        link.subscriptions.clear();
        let id = device_id.clone();
        let tx = self.event_tx.clone();
        async move {
            let _ = tx.send(CentralEvent::DeviceDisconnected {
                device_id: id,
                cause: DisconnectCause::Unknown(0),
            });
            Ok(())
        }
    }

    fn discover_services(
        &self,
        _device_id: &DeviceId,
    ) -> impl Future<Output = BlewResult<Vec<GattService>>> + Send {
        let services = self.link.lock().unwrap().services.clone();
        async move { Ok(services) }
    }

    fn read_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<Vec<u8>>> + Send {
        let link = self.link.lock().unwrap();
        let static_value = link.char_values.get(&char_uuid).cloned();
        let has_service_char = link
            .services
            .iter()
            .any(|s| s.characteristics.iter().any(|c| c.uuid == char_uuid));
        let device_id = device_id.clone();
        let periph_tx = self.periph_event_tx.clone();

        async move {
            if let Some(value) = static_value
                && !value.is_empty()
            {
                return Ok(value);
            }

            if !has_service_char {
                return Err(BlewError::CharacteristicNotFound {
                    device_id,
                    char_uuid,
                });
            }

            let (tx, rx) = oneshot::channel();
            let responder = ReadResponder::new(tx);
            let _ = periph_tx.send(PeripheralEvent::ReadRequest {
                client_id: DeviceId::from("mock-central"),
                service_uuid: Uuid::nil(),
                char_uuid,
                offset: 0,
                responder,
            });
            match rx.await {
                Ok(Ok(data)) => Ok(data),
                _ => Err(BlewError::Gatt {
                    device_id,
                    source: "read failed".into(),
                }),
            }
        }
    }

    fn write_characteristic(
        &self,
        _device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
        write_type: WriteType,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let periph_tx = self.periph_event_tx.clone();
        async move {
            let (responder, rx) = match write_type {
                WriteType::WithResponse => {
                    let (tx, rx) = oneshot::channel::<bool>();
                    (Some(WriteResponder::new(tx)), Some(rx))
                }
                WriteType::WithoutResponse => (None, None),
            };

            let _ = periph_tx.send(PeripheralEvent::WriteRequest {
                client_id: DeviceId::from("mock-central"),
                service_uuid: Uuid::nil(),
                char_uuid,
                value,
                responder,
            });

            if let Some(rx) = rx {
                match rx.await {
                    Ok(true) => Ok(()),
                    _ => Err(BlewError::Gatt {
                        device_id: DeviceId::from("mock-central"),
                        source: "write rejected".into(),
                    }),
                }
            } else {
                Ok(())
            }
        }
    }

    fn subscribe_characteristic(
        &self,
        _device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        self.link
            .lock()
            .unwrap()
            .subscriptions
            .insert(char_uuid, tx);

        let event_tx = self.event_tx.clone();
        let device_id = DeviceId::from("mock-peripheral");
        tokio::spawn(async move {
            while let Some(value) = rx.recv().await {
                let _ = event_tx.send(CentralEvent::CharacteristicNotification {
                    device_id: device_id.clone(),
                    char_uuid,
                    value,
                });
            }
        });

        let _ = self
            .periph_event_tx
            .send(PeripheralEvent::SubscriptionChanged {
                client_id: DeviceId::from("mock-central"),
                char_uuid,
                subscribed: true,
            });

        async { Ok(()) }
    }

    fn unsubscribe_characteristic(
        &self,
        _device_id: &DeviceId,
        char_uuid: Uuid,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        self.link.lock().unwrap().subscriptions.remove(&char_uuid);
        async { Ok(()) }
    }

    async fn mtu(&self, _device_id: &DeviceId) -> u16 {
        512
    }

    fn open_l2cap_channel(
        &self,
        device_id: &DeviceId,
        psm: Psm,
    ) -> impl Future<Output = BlewResult<L2capChannel>> + Send {
        let link = Arc::clone(&self.link);
        let device_id = device_id.clone();
        async move {
            let accept_tx = {
                let guard = link.lock().unwrap();
                if let Some(kind) = guard.l2cap_policy.open_error {
                    return Err(kind.to_error());
                }
                if guard.l2cap_psm != Some(psm) {
                    return Err(BlewError::Internal(format!(
                        "no l2cap listener for psm {psm:?}"
                    )));
                }
                guard
                    .l2cap_accept_tx
                    .clone()
                    .ok_or_else(|| BlewError::Internal("listener accept tx missing".into()))?
            };
            let (central_side, peripheral_side) = L2capChannel::pair(64 * 1024);
            accept_tx
                .send(Ok((device_id, peripheral_side)))
                .map_err(|_| BlewError::Internal("listener stream closed".into()))?;
            Ok(central_side)
        }
    }

    fn events(&self) -> Self::EventStream {
        let rx = self
            .central_rx
            .lock()
            .unwrap()
            .take()
            .expect("events() called more than once on MockCentral");
        tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
    }
}
/// In-memory mock [`PeripheralBackend`].
pub struct MockPeripheral {
    link: SharedLink,
    event_tx: Arc<Mutex<Option<mpsc::UnboundedReceiver<PeripheralEvent>>>>,
    /// Sender for standalone (non-paired) mock peripherals; `None` in pair mode.
    emit_tx: Option<mpsc::UnboundedSender<PeripheralEvent>>,
    central_sender_keepalive: mpsc::UnboundedSender<CentralEvent>,
    powered: Arc<Mutex<bool>>,
}

impl Clone for MockPeripheral {
    fn clone(&self) -> Self {
        Self {
            link: Arc::clone(&self.link),
            event_tx: Arc::clone(&self.event_tx),
            emit_tx: self.emit_tx.clone(),
            central_sender_keepalive: self.central_sender_keepalive.clone(),
            powered: Arc::clone(&self.powered),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl MockPeripheral {
    fn make_link() -> SharedLink {
        Arc::new(Mutex::new(LinkState {
            services: Vec::new(),
            char_values: std::collections::HashMap::new(),
            subscriptions: std::collections::HashMap::new(),
            advertising: false,
            adv_config: None,
            connected: false,
            l2cap_policy: MockL2capPolicy::default(),
            l2cap_psm: None,
            l2cap_accept_tx: None,
        }))
    }

    /// Construct a standalone powered mock peripheral (not linked to any central).
    #[must_use]
    pub fn new_powered() -> crate::peripheral::Peripheral<Self> {
        let (central_event_tx, _central_event_rx) = mpsc::unbounded_channel::<CentralEvent>();
        let (periph_event_tx, periph_event_rx) = mpsc::unbounded_channel();
        crate::peripheral::Peripheral::from_backend(Self {
            link: Self::make_link(),
            event_tx: Arc::new(Mutex::new(Some(periph_event_rx))),
            emit_tx: Some(periph_event_tx),
            central_sender_keepalive: central_event_tx,
            powered: Arc::new(Mutex::new(true)),
        })
    }

    /// Construct a standalone unpowered mock peripheral (not linked to any central).
    #[must_use]
    pub fn new_unpowered() -> crate::peripheral::Peripheral<Self> {
        let (central_event_tx, _central_event_rx) = mpsc::unbounded_channel::<CentralEvent>();
        let (periph_event_tx, periph_event_rx) = mpsc::unbounded_channel();
        crate::peripheral::Peripheral::from_backend(Self {
            link: Self::make_link(),
            event_tx: Arc::new(Mutex::new(Some(periph_event_rx))),
            emit_tx: Some(periph_event_tx),
            central_sender_keepalive: central_event_tx,
            powered: Arc::new(Mutex::new(false)),
        })
    }

    /// Emit an `AdapterStateChanged` event and update the powered flag.
    pub fn mock_emit_adapter_state(&self, powered: bool) {
        *self.powered.lock().unwrap() = powered;
        if let Some(tx) = &self.emit_tx {
            let _ = tx.send(PeripheralEvent::AdapterStateChanged { powered });
        }
    }
}

impl periph_backend::private::Sealed for MockPeripheral {}

impl PeripheralBackend for MockPeripheral {
    type EventStream = tokio_stream::wrappers::UnboundedReceiverStream<PeripheralEvent>;

    async fn new() -> BlewResult<Self>
    where
        Self: Sized,
    {
        Err(BlewError::Internal("use MockLink::pair() instead".into()))
    }

    fn is_powered(&self) -> impl Future<Output = BlewResult<bool>> + Send {
        let powered = *self.powered.lock().unwrap();
        async move { Ok(powered) }
    }

    fn add_service(&self, service: &GattService) -> impl Future<Output = BlewResult<()>> + Send {
        let mut link = self.link.lock().unwrap();
        for ch in &service.characteristics {
            if !ch.value.is_empty() {
                link.char_values.insert(ch.uuid, ch.value.clone());
            }
        }
        link.services.push(service.clone());
        async { Ok(()) }
    }

    fn start_advertising(
        &self,
        config: &AdvertisingConfig,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let mut link = self.link.lock().unwrap();
        if link.advertising {
            return std::future::ready(Err(BlewError::AlreadyAdvertising));
        }
        link.advertising = true;
        link.adv_config = Some(config.clone());
        std::future::ready(Ok(()))
    }

    fn stop_advertising(&self) -> impl Future<Output = BlewResult<()>> + Send {
        let mut link = self.link.lock().unwrap();
        link.advertising = false;
        link.adv_config = None;
        async { Ok(()) }
    }

    fn notify_characteristic(
        &self,
        _device_id: &crate::types::DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
    ) -> impl Future<Output = BlewResult<()>> + Send {
        let link = self.link.lock().unwrap();
        if let Some(tx) = link.subscriptions.get(&char_uuid) {
            let _ = tx.send(Bytes::from(value));
        }
        async { Ok(()) }
    }

    fn l2cap_listener(
        &self,
    ) -> impl Future<
        Output = BlewResult<(
            crate::l2cap::types::Psm,
            impl futures_core::Stream<Item = BlewResult<(DeviceId, L2capChannel)>> + Send + 'static,
        )>,
    > + Send {
        let link = Arc::clone(&self.link);
        async move {
            let mut guard = link.lock().unwrap();
            if let Some(kind) = guard.l2cap_policy.listener_error {
                return Err(kind.to_error());
            }
            // Fixed PSM for deterministic tests.
            let psm = crate::l2cap::types::Psm(0x1001);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            guard.l2cap_psm = Some(psm);
            guard.l2cap_accept_tx = Some(tx);
            Ok((
                psm,
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
        }
    }

    fn events(&self) -> Self::EventStream {
        let rx = self
            .event_tx
            .lock()
            .unwrap()
            .take()
            .expect("events() called more than once on MockPeripheral");
        tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
    }
}
impl<B: CentralBackend> crate::central::Central<B> {
    /// Create a `Central` from a pre-constructed backend (for testing).
    pub fn from_backend(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: CentralBackend + Clone> Clone for crate::central::Central<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
        }
    }
}

impl crate::central::Central<MockCentral> {
    /// Emit an `AdapterStateChanged` event and update the powered flag.
    pub fn mock_emit_adapter_state(&self, powered: bool) {
        self.backend.mock_emit_adapter_state(powered);
    }
}

impl<B: PeripheralBackend> crate::peripheral::Peripheral<B> {
    /// Create a `Peripheral` from a pre-constructed backend (for testing).
    pub fn from_backend(backend: B) -> Self {
        Self {
            backend,
            #[cfg(debug_assertions)]
            events_taken: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl<B: PeripheralBackend + Clone> Clone for crate::peripheral::Peripheral<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            #[cfg(debug_assertions)]
            events_taken: std::sync::atomic::AtomicBool::new(
                self.events_taken.load(std::sync::atomic::Ordering::SeqCst),
            ),
        }
    }
}

impl crate::peripheral::Peripheral<MockPeripheral> {
    /// Emit an `AdapterStateChanged` event and update the powered flag.
    pub fn mock_emit_adapter_state(&self, powered: bool) {
        self.backend.mock_emit_adapter_state(powered);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::Central;
    use crate::gatt::props::{AttributePermissions, CharacteristicProperties};
    use crate::gatt::service::GattCharacteristic;
    use crate::peripheral::Peripheral;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn test_mock_link_discovery() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let svc_uuid = Uuid::from_u128(0x1234);
        peripheral
            .add_service(&GattService {
                uuid: svc_uuid,
                primary: true,
                characteristics: vec![],
            })
            .await
            .unwrap();

        peripheral
            .start_advertising(&AdvertisingConfig {
                local_name: "test".into(),
                service_uuids: vec![svc_uuid],
            })
            .await
            .unwrap();

        let mut events = central.events();
        // Drain the initial AdapterStateChanged event emitted on construction.
        assert!(matches!(
            events.next().await.unwrap(),
            CentralEvent::AdapterStateChanged { powered: true }
        ));
        central.start_scan(ScanFilter::default()).await.unwrap();

        let event = events.next().await.expect("should get discovery event");
        match event {
            CentralEvent::DeviceDiscovered(device) => {
                assert_eq!(device.name.as_deref(), Some("test"));
                assert!(device.services.contains(&svc_uuid));
            }
            other => panic!("expected DeviceDiscovered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_mock_link_connect_disconnect() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let _peripheral = Peripheral::from_backend(p.peripheral);

        let mut events = central.events();
        // Drain the initial AdapterStateChanged event emitted on construction.
        assert!(matches!(
            events.next().await.unwrap(),
            CentralEvent::AdapterStateChanged { powered: true }
        ));
        let device_id = DeviceId::from("mock-peripheral");

        central.connect(&device_id).await.unwrap();
        let ev = events.next().await.unwrap();
        assert!(matches!(ev, CentralEvent::DeviceConnected { .. }));

        central.disconnect(&device_id).await.unwrap();
        let ev = events.next().await.unwrap();
        assert!(matches!(ev, CentralEvent::DeviceDisconnected { .. }));
    }

    #[tokio::test]
    async fn test_mock_link_read_static_value() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xAAAA);
        peripheral
            .add_service(&GattService {
                uuid: Uuid::from_u128(0x1234),
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: b"static-data".to_vec(),
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let device_id = DeviceId::from("mock-peripheral");
        let data = central
            .read_characteristic(&device_id, char_uuid)
            .await
            .unwrap();
        assert_eq!(data, b"static-data");
    }

    #[tokio::test]
    async fn test_mock_link_read_dynamic_value() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xBBBB);
        peripheral
            .add_service(&GattService {
                uuid: Uuid::from_u128(0x1234),
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let mut events = peripheral.events();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let PeripheralEvent::ReadRequest { responder, .. } = event {
                    responder.respond(b"dynamic-response".to_vec());
                }
            }
        });

        let device_id = DeviceId::from("mock-peripheral");
        let data = central
            .read_characteristic(&device_id, char_uuid)
            .await
            .unwrap();
        assert_eq!(data, b"dynamic-response");
    }

    #[tokio::test]
    async fn test_mock_link_write_with_response() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xCCCC);
        peripheral
            .add_service(&GattService {
                uuid: Uuid::from_u128(0x1234),
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::WRITE,
                    permissions: AttributePermissions::WRITE,
                    value: vec![],
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let received = Arc::new(Mutex::new(Vec::new()));
        let received2 = received.clone();
        let mut events = peripheral.events();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let PeripheralEvent::WriteRequest {
                    value, responder, ..
                } = event
                {
                    received2.lock().unwrap().push(value);
                    if let Some(r) = responder {
                        r.success();
                    }
                }
            }
        });

        let device_id = DeviceId::from("mock-peripheral");
        central
            .write_characteristic(
                &device_id,
                char_uuid,
                b"hello".to_vec(),
                WriteType::WithResponse,
            )
            .await
            .unwrap();

        tokio::task::yield_now().await;
        let values = received.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], b"hello");
    }

    #[tokio::test]
    async fn test_mock_link_write_without_response() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xDDDD);
        let received = Arc::new(Mutex::new(Vec::new()));
        let received2 = received.clone();
        let mut events = peripheral.events();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let PeripheralEvent::WriteRequest {
                    value, responder, ..
                } = event
                {
                    received2.lock().unwrap().push(value);
                    assert!(responder.is_none());
                }
            }
        });

        let device_id = DeviceId::from("mock-peripheral");
        central
            .write_characteristic(
                &device_id,
                char_uuid,
                b"fire-and-forget".to_vec(),
                WriteType::WithoutResponse,
            )
            .await
            .unwrap();

        tokio::task::yield_now().await;
        let values = received.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], b"fire-and-forget");
    }

    #[tokio::test]
    async fn test_mock_link_notifications() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xEEEE);
        let device_id = DeviceId::from("mock-peripheral");

        let mut events = central.events();
        // Drain the initial AdapterStateChanged event emitted on construction.
        assert!(matches!(
            events.next().await.unwrap(),
            CentralEvent::AdapterStateChanged { powered: true }
        ));
        central
            .subscribe_characteristic(&device_id, char_uuid)
            .await
            .unwrap();

        peripheral
            .notify_characteristic(&crate::types::DeviceId::from("mock-central"), char_uuid, b"notify-data".to_vec())
            .await
            .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), events.next())
            .await
            .expect("should receive notification")
            .expect("stream should not end");

        match ev {
            CentralEvent::CharacteristicNotification { value, .. } => {
                assert_eq!(&value[..], b"notify-data");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_mock_peripheral_already_advertising() {
        let (_c, p) = MockLink::pair();
        let peripheral = Peripheral::from_backend(p.peripheral);

        let config = AdvertisingConfig {
            local_name: "test".into(),
            service_uuids: vec![],
        };

        peripheral.start_advertising(&config).await.unwrap();
        let result = peripheral.start_advertising(&config).await;
        assert!(matches!(result, Err(BlewError::AlreadyAdvertising)));
    }

    #[tokio::test]
    async fn test_mock_peripheral_is_powered() {
        let (_c, p) = MockLink::pair();
        let peripheral = Peripheral::from_backend(p.peripheral);
        assert!(peripheral.is_powered().await.unwrap());
    }

    #[tokio::test]
    async fn test_mock_link_discover_services() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let svc_uuid = Uuid::from_u128(0x1111);
        let char_uuid = Uuid::from_u128(0x2222);
        peripheral
            .add_service(&GattService {
                uuid: svc_uuid,
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::READ | CharacteristicProperties::WRITE,
                    permissions: AttributePermissions::READ | AttributePermissions::WRITE,
                    value: vec![],
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let device_id = DeviceId::from("mock-peripheral");
        let services = central.discover_services(&device_id).await.unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].uuid, svc_uuid);
        assert_eq!(services[0].characteristics.len(), 1);
        assert_eq!(services[0].characteristics[0].uuid, char_uuid);
    }

    // Backend contract tests: behavioral guarantees that all platform backends must satisfy.
    #[tokio::test]
    async fn contract_write_without_response_returns_immediately() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let _peripheral = Peripheral::from_backend(p.peripheral);

        // BLE spec: Write Command has no ATT response, so central must not block.
        let device_id = DeviceId::from("mock-peripheral");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            central.write_characteristic(
                &device_id,
                Uuid::from_u128(0x1234),
                b"data".to_vec(),
                WriteType::WithoutResponse,
            ),
        )
        .await;

        assert!(result.is_ok(), "write-without-response must not block");
        assert!(result.unwrap().is_ok());
    }
    #[tokio::test]
    async fn contract_write_with_response_blocks_until_ack() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0x5555);

        let device_id = DeviceId::from("mock-peripheral");
        let write_fut = central.write_characteristic(
            &device_id,
            char_uuid,
            b"need-ack".to_vec(),
            WriteType::WithResponse,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), write_fut).await;
        assert!(
            result.is_err(),
            "write-with-response should block until ACK"
        );

        let mut events = peripheral.events();
        let ev = tokio::time::timeout(std::time::Duration::from_millis(50), events.next())
            .await
            .expect("should have event")
            .expect("stream should not end");

        match ev {
            PeripheralEvent::WriteRequest { responder, .. } => {
                responder.unwrap().success();
            }
            other => panic!("expected WriteRequest, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn contract_read_request_routed_to_peripheral() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0x6666);
        peripheral
            .add_service(&GattService {
                uuid: Uuid::from_u128(0x1234),
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let mut events = peripheral.events();
        let expected_char = char_uuid;
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let PeripheralEvent::ReadRequest {
                    char_uuid,
                    responder,
                    ..
                } = event
                {
                    if char_uuid == expected_char {
                        responder.respond(b"correct-char".to_vec());
                    } else {
                        responder.error();
                    }
                }
            }
        });

        let device_id = DeviceId::from("mock-peripheral");
        let data = central
            .read_characteristic(&device_id, char_uuid)
            .await
            .unwrap();
        assert_eq!(data, b"correct-char");
    }
    #[tokio::test]
    async fn contract_subscribe_emits_subscription_event() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0x7777);
        let device_id = DeviceId::from("mock-peripheral");

        let mut events = peripheral.events();

        central
            .subscribe_characteristic(&device_id, char_uuid)
            .await
            .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), events.next())
            .await
            .expect("should receive subscription event")
            .expect("stream should not end");

        match ev {
            PeripheralEvent::SubscriptionChanged {
                char_uuid: uuid,
                subscribed,
                ..
            } => {
                assert_eq!(uuid, char_uuid);
                assert!(subscribed, "should indicate subscription");
            }
            other => panic!("expected SubscriptionChanged, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn contract_notification_before_subscribe_is_dropped() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0x8888);
        let mut events = central.events();
        // Drain the initial AdapterStateChanged event emitted on construction.
        assert!(matches!(
            events.next().await.unwrap(),
            CentralEvent::AdapterStateChanged { powered: true }
        ));

        peripheral
            .notify_characteristic(&crate::types::DeviceId::from("mock-central"), char_uuid, b"too-early".to_vec())
            .await
            .unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), events.next()).await;
        assert!(
            result.is_err(),
            "notification before subscribe should not arrive"
        );

        let device_id = DeviceId::from("mock-peripheral");
        central
            .subscribe_characteristic(&device_id, char_uuid)
            .await
            .unwrap();

        peripheral
            .notify_characteristic(&crate::types::DeviceId::from("mock-central"), char_uuid, b"after-sub".to_vec())
            .await
            .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), events.next())
            .await
            .expect("should receive after subscribe")
            .expect("stream should not end");

        match ev {
            CentralEvent::CharacteristicNotification { value, .. } => {
                assert_eq!(&value[..], b"after-sub");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn contract_stop_then_restart_advertising() {
        let (_c, p) = MockLink::pair();
        let peripheral = Peripheral::from_backend(p.peripheral);

        let config = AdvertisingConfig {
            local_name: "test".into(),
            service_uuids: vec![],
        };

        peripheral.start_advertising(&config).await.unwrap();
        peripheral.stop_advertising().await.unwrap();
        peripheral.start_advertising(&config).await.unwrap();
    }
    #[tokio::test]
    async fn contract_disconnect_clears_subscriptions() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0x9999);
        let device_id = DeviceId::from("mock-peripheral");

        central
            .subscribe_characteristic(&device_id, char_uuid)
            .await
            .unwrap();

        central.disconnect(&device_id).await.unwrap();

        let mut events = central.events();
        peripheral
            .notify_characteristic(&crate::types::DeviceId::from("mock-central"), char_uuid, b"after-disconnect".to_vec())
            .await
            .unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), events.next()).await;

        match result {
            Err(_)
            | Ok(Some(
                CentralEvent::DeviceDisconnected { .. } | CentralEvent::AdapterStateChanged { .. },
            )) => {}
            Ok(Some(CentralEvent::CharacteristicNotification { .. })) => {
                panic!("notification should not arrive after disconnect");
            }
            Ok(other) => panic!("unexpected event: {other:?}"),
        }
    }
    #[tokio::test]
    async fn contract_read_unknown_characteristic_errors() {
        let (c, _p) = MockLink::pair();
        let central = Central::from_backend(c.central);

        let device_id = DeviceId::from("mock-peripheral");
        let unknown_uuid = Uuid::from_u128(0xDEAD_BEEF);

        let result = central.read_characteristic(&device_id, unknown_uuid).await;

        assert!(
            result.is_err(),
            "reading unknown characteristic should error"
        );
        match result.unwrap_err() {
            BlewError::CharacteristicNotFound { char_uuid, .. } => {
                assert_eq!(char_uuid, unknown_uuid);
            }
            other => panic!("expected CharacteristicNotFound, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn contract_dropped_read_responder_returns_error() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xAAAA);
        peripheral
            .add_service(&GattService {
                uuid: Uuid::from_u128(0x1234),
                primary: true,
                characteristics: vec![GattCharacteristic {
                    uuid: char_uuid,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                }],
            })
            .await
            .unwrap();

        let mut events = peripheral.events();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let PeripheralEvent::ReadRequest { responder, .. } = event {
                    drop(responder); // RAII: auto-sends error
                }
            }
        });

        let device_id = DeviceId::from("mock-peripheral");
        let result = central.read_characteristic(&device_id, char_uuid).await;
        assert!(
            result.is_err(),
            "dropped responder should cause read to fail"
        );
    }

    #[tokio::test]
    async fn l2cap_listener_returns_psm_and_stream() {
        use crate::peripheral::backend::PeripheralBackend;
        use tokio_stream::StreamExt;

        let (_central_ep, periph_ep) = MockLink::pair();
        let (psm, mut stream) = periph_ep.peripheral.l2cap_listener().await.unwrap();
        assert!(psm.value() >= 0x1001, "psm should be in the dynamic range");
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            _ = stream.next() => panic!("stream yielded unexpectedly"),
        }
    }

    #[tokio::test]
    async fn l2cap_listener_respects_policy_error() {
        use crate::peripheral::backend::PeripheralBackend;

        let policy = MockL2capPolicy {
            listener_error: Some(MockErrorKind::NotSupported),
            ..Default::default()
        };
        let (_central_ep, periph_ep) = MockLink::pair_with_policy(policy);
        let result = periph_ep.peripheral.l2cap_listener().await;
        assert!(matches!(result, Err(BlewError::NotSupported)));
    }
    #[tokio::test]
    async fn contract_notifications_arrive_in_order() {
        let (c, p) = MockLink::pair();
        let central = Central::from_backend(c.central);
        let peripheral = Peripheral::from_backend(p.peripheral);

        let char_uuid = Uuid::from_u128(0xBBBB);
        let device_id = DeviceId::from("mock-peripheral");

        let mut events = central.events();
        // Drain the initial AdapterStateChanged event emitted on construction.
        assert!(matches!(
            events.next().await.unwrap(),
            CentralEvent::AdapterStateChanged { powered: true }
        ));
        central
            .subscribe_characteristic(&device_id, char_uuid)
            .await
            .unwrap();

        for i in 0..10_u8 {
            peripheral
                .notify_characteristic(&crate::types::DeviceId::from("mock-central"), char_uuid, vec![i])
                .await
                .unwrap();
        }

        for expected in 0..10_u8 {
            let ev = tokio::time::timeout(std::time::Duration::from_millis(100), events.next())
                .await
                .unwrap_or_else(|_| panic!("should receive notification {expected}"))
                .expect("stream should not end");

            match ev {
                CentralEvent::CharacteristicNotification { value, .. } => {
                    assert_eq!(&value[..], &[expected]);
                }
                other => panic!("expected notification {expected}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn open_l2cap_channel_delivers_pair_to_listener() {
        use crate::central::backend::CentralBackend;
        use crate::peripheral::backend::PeripheralBackend;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_stream::StreamExt;

        let (central_ep, periph_ep) = MockLink::pair();

        let (psm, mut listener) = periph_ep.peripheral.l2cap_listener().await.unwrap();

        let device_id = DeviceId::from("mock-peripheral");
        let open_fut = central_ep.central.open_l2cap_channel(&device_id, psm);
        let accept_fut = listener.next();

        let (central_side, accepted) = tokio::join!(open_fut, accept_fut);
        let mut central_side = central_side.unwrap();
        let (accepted_device_id, mut periph_side) = accepted.unwrap().unwrap();
        assert_eq!(accepted_device_id, DeviceId::from("mock-peripheral"));

        central_side.write_all(b"ping").await.unwrap();
        let mut buf = [0_u8; 4];
        periph_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        periph_side.write_all(b"pong").await.unwrap();
        central_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn open_l2cap_channel_respects_policy_error() {
        use crate::central::backend::CentralBackend;
        use crate::peripheral::backend::PeripheralBackend;

        let policy = MockL2capPolicy {
            open_error: Some(MockErrorKind::NotSupported),
            ..Default::default()
        };
        let (central_ep, periph_ep) = MockLink::pair_with_policy(policy);
        let (psm, _listener) = periph_ep.peripheral.l2cap_listener().await.unwrap();
        let device_id = DeviceId::from("mock-peripheral");
        let result = central_ep.central.open_l2cap_channel(&device_id, psm).await;
        assert!(matches!(result, Err(BlewError::NotSupported)));
    }

    #[tokio::test]
    async fn open_l2cap_channel_fails_when_no_listener() {
        use crate::central::backend::CentralBackend;

        let (central_ep, _periph_ep) = MockLink::pair();
        let device_id = DeviceId::from("mock-peripheral");
        let result = central_ep
            .central
            .open_l2cap_channel(&device_id, crate::l2cap::types::Psm(0x1001))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_central_emits_adapter_state_on_init() {
        let (central_ep, _periph_ep) = MockLink::pair();
        let central: crate::Central<_> = crate::Central::from_backend(central_ep.central);
        let mut events = central.events();
        match events.next().await {
            Some(CentralEvent::AdapterStateChanged { powered: true }) => {}
            other => panic!("expected AdapterStateChanged(powered=true), got {other:?}"),
        }
    }
}
