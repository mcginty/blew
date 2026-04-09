pub mod backend;
pub mod types;

pub use types::{AdvertisingConfig, PeripheralEvent, ReadResponder, WriteResponder};

use crate::error::BlewResult;
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, types::Psm};
use crate::platform::PlatformPeripheral;
use crate::util::event_stream::EventStream;
use backend::PeripheralBackend;
use uuid::Uuid;

/// BLE peripheral (server in bluetooth-speak) role: advertiser, GATT server, L2CAP listener.
///
/// The type parameter `B` selects the platform backend and defaults to the correct backend for
/// the current build target.
///
/// ```rust
/// # async fn example() -> blew::error::BlewResult<()> {
/// use blew::peripheral::Peripheral;
///
/// let peripheral: Peripheral = Peripheral::new().await?;
/// # Ok(())
/// # }
/// ```
pub struct Peripheral<B: PeripheralBackend = PlatformPeripheral> {
    pub(crate) backend: B,
}

impl<B: PeripheralBackend> Peripheral<B> {
    /// Initialise the peripheral role, acquiring the platform BLE adapter.
    pub async fn new() -> BlewResult<Self> {
        Ok(Self {
            backend: B::new().await?,
        })
    }

    /// Returns `true` if the local Bluetooth adapter is powered on.
    pub async fn is_powered(&self) -> BlewResult<bool> {
        self.backend.is_powered().await
    }

    /// Register a GATT service. Must be called before [`start_advertising`](Self::start_advertising).
    pub async fn add_service(&self, service: &GattService) -> BlewResult<()> {
        self.backend.add_service(service).await
    }

    /// Begin advertising.
    pub async fn start_advertising(&self, config: &AdvertisingConfig) -> BlewResult<()> {
        self.backend.start_advertising(config).await
    }

    /// Stop advertising.
    pub async fn stop_advertising(&self) -> BlewResult<()> {
        self.backend.stop_advertising().await
    }

    /// Push a characteristic value update to all subscribed centrals.
    pub async fn notify_characteristic(&self, char_uuid: Uuid, value: Vec<u8>) -> BlewResult<()> {
        self.backend.notify_characteristic(char_uuid, value).await
    }

    /// Publish an L2CAP CoC channel.  Returns the OS-assigned PSM and a stream
    /// of incoming [`L2capChannel`] connections.
    pub async fn l2cap_listener(
        &self,
    ) -> BlewResult<(
        Psm,
        impl futures_core::Stream<Item = BlewResult<L2capChannel>> + Send + 'static,
    )> {
        self.backend.l2cap_listener().await
    }

    /// Subscribe to peripheral-role events. Each call returns an independent stream.
    ///
    /// Only one subscriber receives events at a time; a second call replaces the previous.
    pub fn events(&self) -> EventStream<PeripheralEvent, B::EventStream> {
        EventStream::new(self.backend.events())
    }
}
