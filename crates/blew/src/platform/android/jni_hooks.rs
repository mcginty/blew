//! JNI callback entry points called from Kotlin when Android BLE events occur.
//!
//! Each `#[unsafe(no_mangle)] extern "C"` function matches a Kotlin `external fun` declaration.
//! These run on Android Binder threads and must not block -- they push events into
//! tokio channels and return immediately.

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{JNI_TRUE, jboolean, jint};
use jni::{EnvUnowned, jni_sig, jni_str};
use tokio::sync::oneshot;
use tracing::trace;
use uuid::Uuid;

use crate::central::types::CentralEvent;
use crate::l2cap::types::Psm;
use crate::peripheral::types::{PeripheralEvent, ReadResponder, WriteResponder};
use crate::types::{BleDevice, DeviceId};

use super::jni_globals::{jvm, peripheral_class};

fn jstring_to_string(env: &mut jni::Env, s: &JString) -> Option<String> {
    s.try_to_string(env).ok()
}

fn jbytes_to_vec(env: &mut jni::Env, arr: &JByteArray) -> Vec<u8> {
    env.convert_byte_array(arr).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnReadRequest(
    mut env: EnvUnowned,
    _class: JClass,
    request_id: jint,
    device_addr: JString,
    service_uuid: JString,
    char_uuid: JString,
    offset: jint,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(svc) = jstring_to_string(env, &service_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(svc_uuid) = svc.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(chr_uuid) = chr.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };

        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);

        let event = PeripheralEvent::ReadRequest {
            client_id: DeviceId::from(addr.as_str()),
            service_uuid: svc_uuid,
            char_uuid: chr_uuid,
            offset: offset as u16,
            responder,
        };

        super::peripheral::send_event(event);

        let req_id = request_id;
        let addr_clone = addr.clone();
        super::l2cap_state::tokio_handle().spawn(async move {
            match rx.await {
                Ok(Ok(value)) => {
                    respond_to_read_jni(&addr_clone, req_id, Some(&value));
                }
                _ => {
                    respond_to_read_jni(&addr_clone, req_id, None);
                }
            }
        });

        Ok(())
    })
    .into_outcome();
}

fn respond_to_read_jni(device_addr: &str, request_id: jint, value: Option<&[u8]>) {
    let _ = jvm().attach_current_thread(|env| {
        let j_addr = env.new_string(device_addr)?;

        if let Some(val) = value {
            let j_value = env.byte_array_from_slice(val)?;
            let _ = env.call_static_method(
                peripheral_class(),
                jni_str!("respondToRead"),
                jni_sig!("(Ljava/lang/String;I[B)V"),
                &[(&j_addr).into(), request_id.into(), (&j_value).into()],
            );
        } else {
            let _ = env.call_static_method(
                peripheral_class(),
                jni_str!("respondToReadError"),
                jni_sig!("(Ljava/lang/String;I)V"),
                &[(&j_addr).into(), request_id.into()],
            );
        }

        Ok::<_, jni::errors::Error>(())
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnWriteRequest(
    mut env: EnvUnowned,
    _class: JClass,
    request_id: jint,
    device_addr: JString,
    service_uuid: JString,
    char_uuid: JString,
    value: JByteArray,
    response_needed: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(svc) = jstring_to_string(env, &service_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(svc_uuid) = svc.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(chr_uuid) = chr.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };
        let data = jbytes_to_vec(env, &value);
        let needs_response = response_needed == JNI_TRUE;

        let responder = if needs_response {
            let (tx, rx) = oneshot::channel();
            let req_id = request_id;
            let addr_clone = addr.clone();
            super::l2cap_state::tokio_handle().spawn(async move {
                let success = rx.await.unwrap_or(false);
                respond_to_write_jni(&addr_clone, req_id, success);
            });
            Some(WriteResponder::new(tx))
        } else {
            None
        };

        let event = PeripheralEvent::WriteRequest {
            client_id: DeviceId::from(addr.as_str()),
            service_uuid: svc_uuid,
            char_uuid: chr_uuid,
            value: data,
            responder,
        };

        super::peripheral::send_event(event);

        Ok(())
    })
    .into_outcome();
}

fn respond_to_write_jni(device_addr: &str, request_id: jint, success: bool) {
    let _ = jvm().attach_current_thread(|env| {
        let j_addr = env.new_string(device_addr)?;

        let _ = env.call_static_method(
            peripheral_class(),
            jni_str!("respondToWrite"),
            jni_sig!("(Ljava/lang/String;IZ)V"),
            &[
                (&j_addr).into(),
                request_id.into(),
                (success as jboolean).into(),
            ],
        );

        Ok::<_, jni::errors::Error>(())
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnSubscriptionChanged(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    char_uuid: JString,
    subscribed: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(chr_uuid) = chr.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };

        trace!(addr, %chr_uuid, subscribed = subscribed == JNI_TRUE, "subscription changed");

        super::peripheral::send_event(PeripheralEvent::SubscriptionChanged {
            client_id: DeviceId::from(addr.as_str()),
            char_uuid: chr_uuid,
            subscribed: subscribed == JNI_TRUE,
        });

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnConnectionStateChanged(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    connected: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        trace!(
            addr,
            connected = connected == JNI_TRUE,
            "peripheral connection state changed"
        );
        // Not surfaced as a PeripheralEvent -- the transport discovers
        // connections via SubscriptionChanged events instead.
        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnAdapterStateChanged(
    _env: EnvUnowned,
    _class: JClass,
    powered: jboolean,
) {
    trace!(powered = powered == JNI_TRUE, "adapter state changed");
    super::peripheral::send_event(PeripheralEvent::AdapterStateChanged {
        powered: powered == JNI_TRUE,
    });
}

// Central hooks (called from BleCentralManager.kt)

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnAdapterStateChanged(
    _env: EnvUnowned,
    _class: JClass,
    powered: jboolean,
) {
    trace!(
        powered = powered == JNI_TRUE,
        "central adapter state changed"
    );
    super::central::send_event(CentralEvent::AdapterStateChanged {
        powered: powered == JNI_TRUE,
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnDeviceDiscovered(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    device_name: JString,
    rssi: jint,
    service_uuids_str: JString,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let name = jstring_to_string(env, &device_name);
        let uuids_str = jstring_to_string(env, &service_uuids_str).unwrap_or_default();

        let services: Vec<Uuid> = uuids_str
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();

        let device = BleDevice {
            id: DeviceId::from(addr.as_str()),
            name,
            rssi: Some(rssi as i16),
            services,
        };

        trace!(addr, "device discovered");
        super::central::send_event(CentralEvent::DeviceDiscovered(device.clone()));
        super::central::update_discovered(device);

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnConnectionStateChanged(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    connected: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let device_id = DeviceId::from(addr.as_str());

        if connected == JNI_TRUE {
            trace!(%device_id, "central: device connected");
            super::central::send_event(CentralEvent::DeviceConnected {
                device_id: device_id.clone(),
            });
            super::central::complete_connect(&addr, Ok(()));
        } else {
            trace!(%device_id, "central: device disconnected");
            // Fail any pending connect() call -- the connection dropped before
            // MTU negotiation completed (nativeOnConnectionStateChanged(true)
            // is deferred until onMtuChanged).
            super::central::complete_connect(
                &addr,
                Err(crate::error::BlewError::NotConnected(device_id.clone())),
            );
            super::central::send_event(CentralEvent::DeviceDisconnected {
                device_id: device_id.clone(),
            });
        }

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnServicesDiscovered(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    services_json: JString,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(json) = jstring_to_string(env, &services_json) else {
            return Ok::<_, jni::errors::Error>(());
        };

        let services = super::central::parse_services_json(&json);
        super::central::complete_discover_services(&addr, Ok(services));

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnCharacteristicRead(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    char_uuid: JString,
    value: JByteArray,
    status: jint,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let data = jbytes_to_vec(env, &value);

        let result = if status == 0 {
            Ok(data)
        } else {
            Err(crate::error::BlewError::Gatt {
                device_id: DeviceId::from(addr.as_str()),
                source: format!("GATT read failed: status {status}").into(),
            })
        };

        let key = format!("{addr}:read:{chr}");
        super::central::complete_pending(&key, result);

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnCharacteristicWrite(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    char_uuid: JString,
    status: jint,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };

        let result = if status == 0 {
            Ok(vec![])
        } else {
            Err(crate::error::BlewError::Gatt {
                device_id: DeviceId::from(addr.as_str()),
                source: format!("GATT write failed: status {status}").into(),
            })
        };

        let key = format!("{addr}:write:{chr}");
        super::central::complete_pending(&key, result);

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnCharacteristicChanged(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    char_uuid: JString,
    value: JByteArray,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Some(chr) = jstring_to_string(env, &char_uuid) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let Ok(chr_uuid) = chr.parse::<Uuid>() else {
            return Ok::<_, jni::errors::Error>(());
        };
        let data = jbytes_to_vec(env, &value);

        super::central::send_event(CentralEvent::CharacteristicNotification {
            device_id: DeviceId::from(addr.as_str()),
            char_uuid: chr_uuid,
            value: data.into(),
        });

        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnMtuChanged(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    mtu: jint,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        trace!(addr, mtu, "MTU changed");
        super::central::set_mtu(&addr, mtu as u16);
        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnL2capServerOpened(
    _env: EnvUnowned,
    _class: JClass,
    psm: jint,
) {
    trace!(psm, "L2CAP server opened");
    super::l2cap_state::complete_server_open(Ok(Psm::from(psm as u16)));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnL2capServerError(
    mut env: EnvUnowned,
    _class: JClass,
    error_message: JString,
) {
    env.with_env(|env| {
        let msg = jstring_to_string(env, &error_message).unwrap_or_default();
        tracing::warn!("L2CAP server error: {msg}");
        if msg.contains("API 29") {
            super::l2cap_state::complete_server_open(Err(crate::error::BlewError::NotSupported));
        } else {
            super::l2cap_state::complete_server_open(Err(crate::error::BlewError::L2cap {
                source: msg.into(),
            }));
        }
        Ok::<_, jni::errors::Error>(())
    })
    .into_outcome();
}

// Channel opened (one per Kotlin class)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnL2capChannelOpened(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    socket_id: jint,
    from_server: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        trace!(addr, socket_id, "L2CAP channel opened");
        super::l2cap_state::on_channel_opened(&addr, socket_id, from_server == JNI_TRUE);
        Ok(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnL2capChannelOpened(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    socket_id: jint,
    from_server: jboolean,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        trace!(addr, socket_id, "L2CAP channel opened (server)");
        super::l2cap_state::on_channel_opened(&addr, socket_id, from_server == JNI_TRUE);
        Ok(())
    })
    .into_outcome();
}

// Channel data (one per Kotlin class)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnL2capChannelData(
    mut env: EnvUnowned,
    _class: JClass,
    socket_id: jint,
    data: JByteArray,
) {
    env.with_env(|env| {
        let bytes = jbytes_to_vec(env, &data);
        super::l2cap_state::on_channel_data(socket_id, &bytes);
        Ok::<_, jni::errors::Error>(())
    })
    .into_outcome();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnL2capChannelData(
    mut env: EnvUnowned,
    _class: JClass,
    socket_id: jint,
    data: JByteArray,
) {
    env.with_env(|env| {
        let bytes = jbytes_to_vec(env, &data);
        super::l2cap_state::on_channel_data(socket_id, &bytes);
        Ok::<_, jni::errors::Error>(())
    })
    .into_outcome();
}

// Channel closed (one per Kotlin class)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnL2capChannelClosed(
    _env: EnvUnowned,
    _class: JClass,
    socket_id: jint,
) {
    trace!(socket_id, "L2CAP channel closed");
    super::l2cap_state::on_channel_closed(socket_id);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlePeripheralManager_nativeOnL2capChannelClosed(
    _env: EnvUnowned,
    _class: JClass,
    socket_id: jint,
) {
    trace!(socket_id, "L2CAP channel closed (server)");
    super::l2cap_state::on_channel_closed(socket_id);
}

// Channel error (central only -- peripheral uses server error)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BleCentralManager_nativeOnL2capChannelError(
    mut env: EnvUnowned,
    _class: JClass,
    device_addr: JString,
    error_message: JString,
) {
    env.with_env(|env| {
        let Some(addr) = jstring_to_string(env, &device_addr) else {
            return Ok::<_, jni::errors::Error>(());
        };
        let msg = jstring_to_string(env, &error_message).unwrap_or_default();
        tracing::warn!(addr, "L2CAP channel error: {msg}");
        super::l2cap_state::on_channel_error(&addr, msg);
        Ok(())
    })
    .into_outcome();
}
