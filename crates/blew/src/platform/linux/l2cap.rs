//! Shared L2CAP transport construction for Linux platforms.
//!
//! `bluer::l2cap::Stream` already implements async byte-stream I/O, so Linux can
//! use the native stream directly without an intermediate bridge.

use crate::error::{BlewError, BlewResult};
use crate::l2cap::{L2capChannel, L2capEncryption};

pub(crate) fn bridge_l2cap(stream: bluer::l2cap::Stream) -> L2capChannel {
    L2capChannel::from_stream(stream)
}

/// Apply `BT_SECURITY` to a freshly created L2CAP socket, before bind/connect.
///
/// `key_size: 0` leaves the minimum encryption key length to the kernel default.
/// `Low` is the pre-existing behaviour and deliberately keeps BlueZ from
/// triggering a pairing request; the stronger levels will.
pub(crate) fn apply_security(
    socket: &bluer::l2cap::Socket<bluer::l2cap::Stream>,
    encryption: L2capEncryption,
) -> BlewResult<()> {
    let level = match encryption {
        L2capEncryption::Insecure => bluer::l2cap::SecurityLevel::Low,
        L2capEncryption::RequireEncryption => bluer::l2cap::SecurityLevel::Medium,
        L2capEncryption::RequireAuthentication => bluer::l2cap::SecurityLevel::High,
    };
    socket
        .set_security(bluer::l2cap::Security { level, key_size: 0 })
        .map_err(|e| BlewError::L2cap {
            source: Box::new(e),
        })
}
