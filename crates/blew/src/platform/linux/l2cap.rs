//! Shared L2CAP stream bridging for Linux platforms.
//!
//! Both `LinuxCentral` and `LinuxPeripheral` use this to wrap a
//! `bluer::l2cap::Stream` into an async `L2capChannel`.

use crate::l2cap::L2capChannel;

const L2CAP_DUPLEX_BUF_SIZE: usize = 65536;

pub(crate) fn bridge_l2cap(stream: bluer::l2cap::Stream) -> L2capChannel {
    let (app_side, io_side) = tokio::io::duplex(L2CAP_DUPLEX_BUF_SIZE);
    let (mut io_read, mut io_write) = tokio::io::split(io_side);
    let (mut stream_read, mut stream_write) = tokio::io::split(stream);
    tokio::spawn(async move {
        if let Err(e) = tokio::io::copy(&mut stream_read, &mut io_write).await {
            tracing::debug!("L2CAP inbound copy ended: {e}");
        }
    });
    tokio::spawn(async move {
        if let Err(e) = tokio::io::copy(&mut io_read, &mut stream_write).await {
            tracing::debug!("L2CAP outbound copy ended: {e}");
        }
    });
    L2capChannel::from_duplex(app_side)
}
