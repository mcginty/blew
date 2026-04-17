//! Integration-test peripheral (fixture for `integration_central`).
//!
//! Advertises a fixed service with three characteristics and an L2CAP
//! listener:
//!
//! * `STATUS_CHAR` (READ)        -- responds with `b"BLEW-OK"` on read
//! * `ECHO_CHAR` (WRITE, NOTIFY) -- echoes each write back via notify
//! * `PSM_CHAR` (READ)           -- returns the assigned L2CAP PSM (u16 LE)
//!
//! Plus an L2CAP CoC that echoes every byte received.
//!
//! Exists as the real-hardware counterpart to `integration_central`. Start
//! this on one host, then run the central on a second host. See the
//! `Testing on real hardware` section of the README.
//!
//! ```sh
//! cargo run --example integration_peripheral -p blew
//! ```

use blew::Peripheral;
use blew::gatt::props::{AttributePermissions, CharacteristicProperties};
use blew::gatt::service::{GattCharacteristic, GattService};
use blew::peripheral::{AdvertisingConfig, PeripheralRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt as _;
use uuid::Uuid;

const SVC_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0001);
const STATUS_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0002);
const ECHO_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0003);
const PSM_CHAR_UUID: Uuid = Uuid::from_u128(0x626c_6577_0000_0000_0000_0000_0000_0004);
const STATUS_VALUE: &[u8] = b"BLEW-OK";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let peripheral: Peripheral = Peripheral::new().await?;
    peripheral
        .add_service(&GattService {
            uuid: SVC_UUID,
            primary: true,
            characteristics: vec![
                GattCharacteristic {
                    uuid: STATUS_CHAR_UUID,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                },
                GattCharacteristic {
                    uuid: ECHO_CHAR_UUID,
                    properties: CharacteristicProperties::WRITE | CharacteristicProperties::NOTIFY,
                    permissions: AttributePermissions::WRITE,
                    value: vec![],
                    descriptors: vec![],
                },
                GattCharacteristic {
                    uuid: PSM_CHAR_UUID,
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                },
            ],
        })
        .await?;

    let mut requests = peripheral.take_requests().expect("requests already taken");
    let (psm, mut l2cap_channels) = peripheral.l2cap_listener().await?;
    let psm_bytes = psm.value().to_le_bytes().to_vec();
    println!("L2CAP PSM = {}", psm.value());

    peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "blew-integration".into(),
            service_uuids: vec![SVC_UUID],
        })
        .await?;
    println!("Advertising as \"blew-integration\" ... (Ctrl-C to stop)");

    loop {
        tokio::select! {
            request = requests.next() => {
                let Some(request) = request else { break };
                match request {
                    PeripheralRequest::Read { char_uuid, responder, .. } => {
                        match char_uuid {
                            u if u == STATUS_CHAR_UUID => responder.respond(STATUS_VALUE.to_vec()),
                            u if u == PSM_CHAR_UUID => responder.respond(psm_bytes.clone()),
                            _ => responder.error(),
                        }
                    }
                    PeripheralRequest::Write {
                        client_id, char_uuid, value, responder, ..
                    } if char_uuid == ECHO_CHAR_UUID => {
                        if let Some(responder) = responder {
                            responder.success();
                        }
                        println!("  echo: {} bytes from {client_id}", value.len());
                        if let Err(e) = peripheral
                            .notify_characteristic(&client_id, ECHO_CHAR_UUID, value)
                            .await
                        {
                            eprintln!("  notify failed: {e}");
                        }
                    }
                    PeripheralRequest::Write { responder, .. } => {
                        if let Some(responder) = responder { responder.error(); }
                    }
                }
            }
            incoming = l2cap_channels.next() => {
                let Some(result) = incoming else { break };
                match result {
                    Ok((device_id, mut ch)) => {
                        println!("  L2CAP accept from {device_id}");
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 4096];
                            loop {
                                match ch.read(&mut buf).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        if ch.write_all(&buf[..n]).await.is_err() { break }
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => eprintln!("  L2CAP accept error: {e}"),
                }
            }
        }
    }

    peripheral.stop_advertising().await?;
    Ok(())
}
