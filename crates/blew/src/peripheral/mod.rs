pub mod backend;
pub mod types;

pub use types::{AdvertisingConfig, PeripheralEvent, ReadResponder, WriteResponder};

use crate::error::{BlewError, BlewResult};
use crate::gatt::service::GattService;
use crate::l2cap::{L2capChannel, types::Psm};
use crate::platform::PlatformPeripheral;
use crate::types::DeviceId;
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
    #[cfg(debug_assertions)]
    pub(crate) events_taken: std::sync::atomic::AtomicBool,
}

impl<B: PeripheralBackend> Peripheral<B> {
    /// Initialise the peripheral role, acquiring the platform BLE adapter.
    pub async fn new() -> BlewResult<Self> {
        Ok(Self {
            backend: B::new().await?,
            #[cfg(debug_assertions)]
            events_taken: std::sync::atomic::AtomicBool::new(false),
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

    /// Push a characteristic value update to a single subscribed central.
    ///
    /// See [`PeripheralBackend::notify_characteristic`] for the per-device
    /// semantics and the Linux-only broadcast fallback.
    pub async fn notify_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
    ) -> BlewResult<()> {
        self.backend
            .notify_characteristic(device_id, char_uuid, value)
            .await
    }

    /// Publish an L2CAP CoC channel.  Returns the OS-assigned PSM and a stream
    /// of incoming [`L2capChannel`] connections.
    pub async fn l2cap_listener(
        &self,
    ) -> BlewResult<(
        Psm,
        impl futures_core::Stream<Item = BlewResult<(DeviceId, L2capChannel)>> + Send + 'static,
    )> {
        self.backend.l2cap_listener().await
    }

    /// Returns the peripheral event stream.
    ///
    /// # Single-consumer contract
    ///
    /// [`PeripheralEvent`] is `!Clone` because it carries RAII
    /// [`ReadResponder`]/[`WriteResponder`] handles. Because of that, this
    /// method is **single-consumer**: calling it a second time replaces the
    /// internal sender and silently steals the stream from the previous
    /// caller. Debug builds will panic on the second call to catch this
    /// early; release builds will not.
    pub fn events(&self) -> EventStream<PeripheralEvent, B::EventStream> {
        #[cfg(debug_assertions)]
        {
            let was_taken = self
                .events_taken
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                !was_taken,
                "Peripheral::events() called more than once — this is not \
                 supported (single-consumer contract); PeripheralEvent is !Clone \
                 and a second call would silently steal the stream"
            );
        }
        self.events_inner()
    }

    fn events_inner(&self) -> EventStream<PeripheralEvent, B::EventStream> {
        EventStream::new(self.backend.events())
    }

    /// Wait until the adapter is powered on, or return `BlewError::Timeout`.
    pub async fn wait_ready(&self, timeout: std::time::Duration) -> BlewResult<()> {
        if self.backend.is_powered().await.unwrap_or(false) {
            return Ok(());
        }
        // Uses events_inner() to bypass the single-consumer check since
        // wait_ready is an internal operation; calling it should not "use up"
        // the caller's one allowed events() call.
        let mut events = self.events_inner();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BlewError::Timeout);
            }
            match tokio::time::timeout(remaining, tokio_stream::StreamExt::next(&mut events)).await
            {
                Err(_) => return Err(BlewError::Timeout),
                Ok(None) => return Err(BlewError::StreamClosed),
                Ok(Some(PeripheralEvent::AdapterStateChanged { powered: true })) => return Ok(()),
                Ok(Some(_)) => {}
            }
        }
    }
}

#[cfg(all(test, feature = "testing", debug_assertions))]
mod single_consumer_test {
    #[test]
    #[should_panic(expected = "single-consumer contract")]
    fn events_twice_panics_in_debug() {
        let p = crate::testing::MockPeripheral::new_powered();
        let _ev1 = p.events();
        let _ev2 = p.events();
    }
}
