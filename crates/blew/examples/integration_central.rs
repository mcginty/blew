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
//! 6. Run three 1 MiB L2CAP throughput checks: central->peripheral,
//!    peripheral->central, and bidirectional concurrent.
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
use indicatif::{ProgressBar, ProgressStyle};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

const SVC_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0001);
const STATUS_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0002);
const ECHO_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0003);
const PSM_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0004);
const STATUS_VALUE: &[u8] = b"BLEW-OK";
const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const OP_TIMEOUT: Duration = Duration::from_secs(15);
const SPEEDTEST_TIMEOUT: Duration = Duration::from_secs(120);
const BIDIRECTIONAL_SPEEDTEST_TIMEOUT: Duration = Duration::from_secs(180);
const ECHO_PAYLOAD: &[u8] = b"the quick brown fox jumps over a lazy dog";
const L2CAP_PAYLOAD_LEN: usize = 1024;
const SPEEDTEST_BYTES: usize = 1024 * 1024;
const SPEEDTEST_CHUNK_SIZE: usize = 4096;
const PROGRESS_YIELD_INTERVAL: usize = 64 * 1024;
const UPLOAD_PROGRESS_INTERVAL: usize = 64 * 1024;
const CMD_ECHO: u8 = 0x01;
const CMD_UPLOAD: u8 = 0x02;
const CMD_DOWNLOAD: u8 = 0x03;
const CMD_BIDIRECTIONAL: u8 = 0x04;
const CENTRAL_PATTERN: u8 = 0xC1;
const PERIPHERAL_PATTERN: u8 = 0xD2;

async fn write_command_header(
    ch: &mut blew::L2capChannel,
    cmd: u8,
    len: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let len = u32::try_from(len).map_err(|_| "payload too large for protocol header")?;
    ch.write_all(&[cmd]).await?;
    ch.write_all(&len.to_le_bytes()).await?;
    Ok(())
}

fn speed_kib_per_s(bytes: usize, elapsed: Duration) -> f64 {
    (bytes as f64 / 1024.0) / elapsed.as_secs_f64()
}

fn print_speed(label: &str, bytes: usize, elapsed: Duration) {
    let kib_per_s = speed_kib_per_s(bytes, elapsed);
    println!(
        "{label}: {} KiB in {:.2?} -> {:.1} KiB/s ({:.1} kbps)",
        bytes / 1024,
        elapsed,
        kib_per_s,
        kib_per_s * 8.0,
    );
}

fn live_speed_label(label: &str, bytes: usize, start: Instant) -> String {
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let mib_per_s = bytes as f64 / (1024.0 * 1024.0) / elapsed;
    format!("{label} ({mib_per_s:.2} MiB/s)")
}

fn spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("valid spinner template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn bytes_bar(message: &str, total: usize) -> ProgressBar {
    let pb = ProgressBar::new(u64::try_from(total).expect("progress total fits in u64"));
    pb.set_style(
        ProgressStyle::with_template(
            "{msg:<48} [{bar:30.cyan/blue}] {bytes:>8}/{total_bytes:8} ETA {eta:>4}",
        )
        .expect("valid bytes template")
        .progress_chars("=> "),
    );
    pb.set_message(message.to_string());
    pb
}

async fn run_echo(
    ch: &mut blew::L2capChannel,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    write_command_header(ch, CMD_ECHO, payload.len()).await?;
    ch.write_all(payload).await?;
    let mut received = vec![0_u8; payload.len()];
    ch.read_exact(&mut received).await?;
    if received != payload {
        return Err("L2CAP echo mismatch".into());
    }
    Ok(())
}

async fn run_upload_speedtest(
    ch: &mut blew::L2capChannel,
    progress: &ProgressBar,
) -> Result<Duration, Box<dyn std::error::Error>> {
    write_command_header(ch, CMD_UPLOAD, SPEEDTEST_BYTES).await?;
    let start = Instant::now();
    let label = "speedtest central->peripheral";
    let (mut reader, mut writer) = tokio::io::split(ch);

    let sender = async {
        let chunk = [CENTRAL_PATTERN; SPEEDTEST_CHUNK_SIZE];
        let mut remaining = SPEEDTEST_BYTES;
        let mut bytes_since_yield = 0_usize;
        while remaining > 0 {
            let n = remaining.min(SPEEDTEST_CHUNK_SIZE);
            writer.write_all(&chunk[..n]).await?;
            remaining -= n;
            bytes_since_yield += n;
            if bytes_since_yield >= PROGRESS_YIELD_INTERVAL {
                bytes_since_yield = 0;
                tokio::task::yield_now().await;
            }
        }
        Ok::<(), std::io::Error>(())
    };

    let receiver = async {
        let mut last_reported = 0_usize;
        while last_reported < SPEEDTEST_BYTES {
            let mut report = [0_u8; 4];
            reader.read_exact(&mut report).await?;
            let received =
                usize::try_from(u32::from_le_bytes(report)).expect("progress report fits in usize");
            if received < last_reported || received > SPEEDTEST_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid upload progress report",
                ));
            }
            progress.set_position(u64::try_from(received).expect("progress fits in u64"));
            progress.set_message(live_speed_label(label, received, start));
            last_reported = received;
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::try_join!(sender, receiver)?;
    if progress.position() < u64::try_from(SPEEDTEST_BYTES).expect("speedtest bytes fit in u64") {
        progress.set_position(u64::try_from(SPEEDTEST_BYTES).expect("speedtest bytes fit in u64"));
    }
    Ok(start.elapsed())
}

async fn run_download_speedtest(
    ch: &mut blew::L2capChannel,
    progress: &ProgressBar,
) -> Result<Duration, Box<dyn std::error::Error>> {
    write_command_header(ch, CMD_DOWNLOAD, SPEEDTEST_BYTES).await?;
    let start = Instant::now();
    let label = "speedtest peripheral->central";
    let mut remaining = SPEEDTEST_BYTES;
    let mut buf = vec![0_u8; SPEEDTEST_CHUNK_SIZE];
    while remaining > 0 {
        let n = ch
            .read(&mut buf[..remaining.min(SPEEDTEST_CHUNK_SIZE)])
            .await?;
        if n == 0 {
            return Err("download speedtest hit EOF early".into());
        }
        if buf[..n].iter().any(|&b| b != PERIPHERAL_PATTERN) {
            return Err("download speedtest received unexpected payload bytes".into());
        }
        progress.inc(u64::try_from(n).expect("chunk size fits in u64"));
        remaining -= n;
        let received = SPEEDTEST_BYTES - remaining;
        progress.set_message(live_speed_label(label, received, start));
    }
    Ok(start.elapsed())
}

async fn run_bidirectional_speedtest(
    ch: blew::L2capChannel,
    progress: ProgressBar,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let (mut reader, mut writer) = tokio::io::split(ch);
    let start = Instant::now();
    let label = "speedtest concurrent";
    let progress = Arc::new(progress);
    let sender_progress = Arc::clone(&progress);
    let sender_label = label;

    let sender = async move {
        let chunk = [CENTRAL_PATTERN; SPEEDTEST_CHUNK_SIZE];
        let mut remaining = SPEEDTEST_BYTES;
        let mut bytes_since_yield = 0_usize;
        while remaining > 0 {
            let n = remaining.min(SPEEDTEST_CHUNK_SIZE);
            writer.write_all(&chunk[..n]).await?;
            sender_progress.inc(u64::try_from(n).expect("chunk size fits in u64"));
            remaining -= n;
            bytes_since_yield += n;
            let transferred = usize::try_from(sender_progress.position())
                .expect("progress position fits in usize");
            sender_progress.set_message(live_speed_label(sender_label, transferred, start));
            if bytes_since_yield >= PROGRESS_YIELD_INTERVAL {
                bytes_since_yield = 0;
                tokio::task::yield_now().await;
            }
        }
        writer.shutdown().await?;
        Ok::<_, std::io::Error>(())
    };

    let progress = Arc::clone(&progress);
    let receiver_label = label;
    let receiver = async move {
        let mut remaining = SPEEDTEST_BYTES;
        let mut buf = vec![0_u8; SPEEDTEST_CHUNK_SIZE];
        while remaining > 0 {
            let n = reader
                .read(&mut buf[..remaining.min(SPEEDTEST_CHUNK_SIZE)])
                .await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "bidirectional speedtest hit EOF early",
                ));
            }
            if buf[..n].iter().any(|&b| b != PERIPHERAL_PATTERN) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bidirectional speedtest received unexpected payload bytes",
                ));
            }
            progress.inc(u64::try_from(n).expect("chunk size fits in u64"));
            remaining -= n;
            let transferred =
                usize::try_from(progress.position()).expect("progress position fits in usize");
            progress.set_message(live_speed_label(receiver_label, transferred, start));
        }
        Ok::<_, std::io::Error>(())
    };

    tokio::try_join!(sender, receiver)?;
    Ok(start.elapsed())
}

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

    let scan_pb = spinner("scan: waiting for integration peripheral");
    central
        .start_scan(ScanFilter {
            services: vec![SVC_UUID],
            ..Default::default()
        })
        .await?;

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
    scan_pb.finish_with_message(format!("scan: found {device_id}"));

    let connect_pb = spinner("connect: opening BLE link");
    timeout(OP_TIMEOUT, central.connect(&device_id))
        .await
        .map_err(|_| "connect timeout")??;
    connect_pb.finish_with_message("connect: ok");

    let discover_pb = spinner("discover: loading GATT services");
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
    discover_pb.finish_with_message(format!(
        "discover: ok ({} characteristics)",
        svc.characteristics.len()
    ));

    let status_pb = spinner("read status: reading STATUS_CHAR");
    let status = timeout(
        OP_TIMEOUT,
        central.read_characteristic(&device_id, STATUS_CHAR_UUID),
    )
    .await
    .map_err(|_| "read STATUS timeout")??;
    if status != STATUS_VALUE {
        return Err(format!("STATUS mismatch: got {status:?}, want {STATUS_VALUE:?}").into());
    }
    status_pb.finish_with_message("read status: ok");

    let gatt_pb = spinner("gatt echo: write + notify round-trip");
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
    gatt_pb.finish_with_message(format!("gatt echo: ok ({} bytes round-trip)", echo.len()));

    let psm_pb = spinner("l2cap: reading PSM and opening channel");
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
    psm_pb.finish_with_message("l2cap: channel open");

    let echo_pb = spinner("l2cap echo: 1 KiB round-trip");
    let payload: Vec<u8> = (0..L2CAP_PAYLOAD_LEN).map(|i| (i & 0xff) as u8).collect();
    timeout(OP_TIMEOUT, run_echo(&mut ch, &payload))
        .await
        .map_err(|_| "L2CAP echo timeout")??;
    echo_pb.finish_with_message(format!(
        "l2cap echo: ok ({L2CAP_PAYLOAD_LEN} bytes round-trip)"
    ));

    let upload_pb = bytes_bar("speedtest central->peripheral", SPEEDTEST_BYTES);
    let upload_elapsed = timeout(SPEEDTEST_TIMEOUT, run_upload_speedtest(&mut ch, &upload_pb))
        .await
        .map_err(|_| "central->peripheral speedtest timeout")??;
    upload_pb.finish_and_clear();
    print_speed(
        "speedtest central->peripheral",
        SPEEDTEST_BYTES,
        upload_elapsed,
    );

    let download_pb = bytes_bar("speedtest peripheral->central", SPEEDTEST_BYTES);
    let download_elapsed = timeout(
        SPEEDTEST_TIMEOUT,
        run_download_speedtest(&mut ch, &download_pb),
    )
    .await
    .map_err(|_| "peripheral->central speedtest timeout")??;
    download_pb.finish_and_clear();
    print_speed(
        "speedtest peripheral->central",
        SPEEDTEST_BYTES,
        download_elapsed,
    );

    write_command_header(&mut ch, CMD_BIDIRECTIONAL, SPEEDTEST_BYTES).await?;
    let concurrent_pb = bytes_bar("speedtest concurrent", SPEEDTEST_BYTES * 2);
    let bidirectional_elapsed = timeout(
        BIDIRECTIONAL_SPEEDTEST_TIMEOUT,
        run_bidirectional_speedtest(ch, concurrent_pb.clone()),
    )
    .await
    .map_err(|_| "bidirectional speedtest timeout")??;
    concurrent_pb.finish_and_clear();
    print_speed(
        "speedtest concurrent",
        SPEEDTEST_BYTES * 2,
        bidirectional_elapsed,
    );

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
