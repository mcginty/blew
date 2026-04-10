use crate::types::{BleDevice, DeviceId};
use bytes::Bytes;
use uuid::Uuid;

/// Events emitted by the central (scanner/client) role.
///
/// `CentralEvent` is `Clone`; notification payloads use [`Bytes`] to avoid copies
/// when fanning out to multiple subscribers.
#[derive(Debug, Clone)]
pub enum CentralEvent {
    /// The local Bluetooth adapter was powered on or off.
    AdapterStateChanged { powered: bool },
    /// A new peripheral was discovered or its advertising data was updated.
    DeviceDiscovered(BleDevice),
    /// A connection to a peripheral was established.
    DeviceConnected { device_id: DeviceId },
    /// A peripheral disconnected.
    DeviceDisconnected { device_id: DeviceId },
    /// A subscribed characteristic sent a notification or indication.
    CharacteristicNotification {
        device_id: DeviceId,
        char_uuid: Uuid,
        value: Bytes,
    },
}

/// Scan duty cycle / power trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanMode {
    /// Highest duty cycle -- discovers devices fastest but uses the most power
    /// and radio bandwidth.
    #[default]
    LowLatency,
    /// Reduced duty cycle -- suitable for background scanning while active
    /// connections are in progress.
    LowPower,
}

/// Filter applied during scanning.
#[derive(Debug, Clone, Default)]
pub struct ScanFilter {
    /// Only report peripherals advertising at least one of these service UUIDs.
    /// Empty means report all.
    pub services: Vec<Uuid>,
    pub mode: ScanMode,
}

/// How a characteristic write is delivered to the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteType {
    /// ATT Write Request -- the peripheral sends an acknowledgement.
    WithResponse,
    /// ATT Write Command -- no acknowledgement; lower latency.
    WithoutResponse,
}
