//! Integration-test central (pairs with `integration_peripheral`).
//!
//! Walks a scripted protocol that exercises the core BLE operations and exits
//! with status 0 on success, non-zero on any mismatch or timeout.
//!
//! Steps:
//!
//! 1. Scan for `integration_peripheral`'s service UUID (60 s timeout).
//! 2. Connect and discover services. Verify all three characteristics exist.
//! 3. Read `STATUS_CHAR`, assert it equals `b"BLEW-OK"`.
//! 4. Subscribe to `ECHO_CHAR`, write a fixed ASCII payload, assert the
//!    notification carries the same bytes back.
//! 5. Read `PSM_CHAR`, open an L2CAP CoC to that PSM, echo 1 KiB, assert
//!    the echo matches.
//!
//! Start the peripheral on host A, then run this on host B. See the
//! `Testing on real hardware` section of the README.
//!
//! ```sh
//! cargo run --example integration_central -p blew
//! ```

use blew::Central;
use blew::central::{CentralEvent, ScanFilter, WriteType};
use blew::l2cap::Psm;
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

const SVC_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0001);
const STATUS_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0002);
const ECHO_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0003);
const PSM_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0004);
const STATUS_VALUE: &[u8] = b"BLEW-OK";
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);
const OP_TIMEOUT: Duration = Duration::from_secs(15);
const ECHO_PAYLOAD: &[u8] = b"the quick brown fox jumps over a lazy dog";
const L2CAP_PAYLOAD_LEN: usize = 1024;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match run().await {
        Ok(()) => {
            println!("\nintegration-central: PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nintegration-central: FAIL -- {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let central: Central = Central::new().await?;
    let mut events = central.events();

    central
        .start_scan(ScanFilter {
            services: vec![SVC_UUID],
            ..Default::default()
        })
        .await?;
    println!(
        "scan: waiting up to {:?} for service {SVC_UUID}",
        SCAN_TIMEOUT
    );

    let device_id = timeout(SCAN_TIMEOUT, async {
        loop {
            match events.next().await {
                Some(CentralEvent::DeviceDiscovered(d)) if d.services.contains(&SVC_UUID) => {
                    return Ok::<_, Box<dyn std::error::Error>>(d.id);
                }
                Some(_) => {}
                None => return Err("event stream closed before device found".into()),
            }
        }
    })
    .await
    .map_err(|_| "scan timeout")??;

    central.stop_scan().await?;
    println!("scan: found {device_id}");

    timeout(OP_TIMEOUT, central.connect(&device_id))
        .await
        .map_err(|_| "connect timeout")??;
    println!("connect: ok");

    let services = timeout(OP_TIMEOUT, central.discover_services(&device_id))
        .await
        .map_err(|_| "discover_services timeout")??;
    let svc = services
        .iter()
        .find(|s| s.uuid == SVC_UUID)
        .ok_or("peer missing service")?;
    for want in [STATUS_CHAR_UUID, ECHO_CHAR_UUID, PSM_CHAR_UUID] {
        if !svc.characteristics.iter().any(|c| c.uuid == want) {
            return Err(format!("peer missing characteristic {want}").into());
        }
    }
    println!(
        "discover: ok ({} characteristics)",
        svc.characteristics.len()
    );

    let status = timeout(
        OP_TIMEOUT,
        central.read_characteristic(&device_id, STATUS_CHAR_UUID),
    )
    .await
    .map_err(|_| "read STATUS timeout")??;
    if status != STATUS_VALUE {
        return Err(format!("STATUS mismatch: got {status:?}, want {STATUS_VALUE:?}").into());
    }
    println!("read status: ok");

    timeout(
        OP_TIMEOUT,
        central.subscribe_characteristic(&device_id, ECHO_CHAR_UUID),
    )
    .await
    .map_err(|_| "subscribe timeout")??;

    timeout(
        OP_TIMEOUT,
        central.write_characteristic(
            &device_id,
            ECHO_CHAR_UUID,
            ECHO_PAYLOAD.to_vec(),
            WriteType::WithResponse,
        ),
    )
    .await
    .map_err(|_| "write ECHO timeout")??;

    let echo = timeout(OP_TIMEOUT, async {
        loop {
            match events.next().await {
                Some(CentralEvent::CharacteristicNotification {
                    char_uuid, value, ..
                }) if char_uuid == ECHO_CHAR_UUID => {
                    return Ok::<_, Box<dyn std::error::Error>>(value);
                }
                Some(_) => {}
                None => return Err("event stream closed before echo".into()),
            }
        }
    })
    .await
    .map_err(|_| "echo notify timeout")??;
    if echo != ECHO_PAYLOAD {
        return Err(format!(
            "ECHO mismatch: got {} bytes, want {}",
            echo.len(),
            ECHO_PAYLOAD.len()
        )
        .into());
    }
    println!("gatt echo: ok ({} bytes round-trip)", echo.len());

    let psm_data = timeout(
        OP_TIMEOUT,
        central.read_characteristic(&device_id, PSM_CHAR_UUID),
    )
    .await
    .map_err(|_| "read PSM timeout")??;
    let psm_bytes: [u8; 2] = psm_data
        .as_slice()
        .try_into()
        .map_err(|_| format!("PSM char must be 2 bytes, got {}", psm_data.len()))?;
    let psm = Psm(u16::from_le_bytes(psm_bytes));

    let mut ch = timeout(OP_TIMEOUT, central.open_l2cap_channel(&device_id, psm))
        .await
        .map_err(|_| "open L2CAP timeout")??;

    let payload: Vec<u8> = (0..L2CAP_PAYLOAD_LEN).map(|i| (i & 0xff) as u8).collect();
    ch.write_all(&payload).await?;
    let mut received = vec![0u8; L2CAP_PAYLOAD_LEN];
    timeout(OP_TIMEOUT, ch.read_exact(&mut received))
        .await
        .map_err(|_| "L2CAP read timeout")??;
    if received != payload {
        return Err("L2CAP echo mismatch".into());
    }
    println!("l2cap echo: ok ({L2CAP_PAYLOAD_LEN} bytes round-trip)");

    drop(ch);

    if let Err(e) = timeout(
        OP_TIMEOUT,
        central.unsubscribe_characteristic(&device_id, ECHO_CHAR_UUID),
    )
    .await
    {
        eprintln!("cleanup: unsubscribe timeout: {e}");
    }

    if let Err(e) = timeout(OP_TIMEOUT, central.disconnect(&device_id)).await {
        eprintln!("cleanup: disconnect timeout: {e}");
    }

    drop(events);
    drop(central);

    Ok(())
}
