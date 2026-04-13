//! `blew` is a cross-platform BLE central + peripheral library.
//!
//! Supports iOS, macOS, Linux, and Android. Each role is initialised
//! independently, and in its simplest form:
//!
//! ```rust
//! # async fn example() -> blew::error::BlewResult<()> {
//! use blew::central::{Central, ScanFilter};
//! use blew::peripheral::Peripheral;
//!
//! let central: Central = Central::new().await?;
//! central.start_scan(ScanFilter::default()).await?;
//!
//! let peripheral: Peripheral = Peripheral::new().await?;
//! # Ok(())
//! # }
//! ```

pub mod central;
pub mod error;
pub mod gatt;
pub mod l2cap;
pub mod peripheral;
pub mod platform;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;
pub mod util;

pub use central::{Central, CentralConfig, DisconnectCause};
pub use error::{BlewError, BlewResult};
pub use l2cap::L2capChannel;
pub use peripheral::Peripheral;
pub use types::{BleDevice, DeviceId};
