//! Global state for Android L2CAP channels.
//!
//! Each channel uses a `tokio::io::DuplexStream` pair -- one half becomes the
//! `L2capChannel`, the other is bridged via JNI to Kotlin's BluetoothSocket.
//!
//! **Kotlin->Rust data path:** Kotlin read thread -> JNI `nativeOnL2capChannelData`
//! -> `mpsc::UnboundedSender` -> tokio drain task -> DuplexStream writer -> L2capChannel read.
//!
//! **Rust->Kotlin data path:** L2capChannel write -> DuplexStream -> tokio read task
//! -> JNI `writeL2cap` -> Kotlin OutputStream.

use std::collections::HashMap;
use std::sync::OnceLock;

use jni::objects::JClass;
use jni::refs::Global;
use jni::{jni_sig, jni_str};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{BlewError, BlewResult};
use crate::l2cap::types::{L2capCloseReason, L2capConfig, L2capEncryption, Psm};
use crate::l2cap::{CloseReasonSlot, DuplexBridge, L2capChannel};
use crate::types::DeviceId;

use super::jni_globals::{central_class, jvm, peripheral_class};

/// Smallest number of queued chunks, so a tiny `buffer_size` can never produce
/// a zero-capacity channel (which would deadlock).
const MIN_QUEUE_CHUNKS: usize = 2;

type AcceptSender = mpsc::UnboundedSender<BlewResult<(DeviceId, L2capChannel)>>;

struct L2capState {
    pending_server: Mutex<Option<oneshot::Sender<BlewResult<Psm>>>>,
    pending_open: Mutex<HashMap<String, oneshot::Sender<BlewResult<L2capChannel>>>>,
    accept_tx: Mutex<Option<AcceptSender>>,
    /// Bounded per-socket inbound queues. The Kotlin read thread blocks on a
    /// full queue, which stops it draining the socket, which stops L2CAP
    /// credits being returned, which stops the peer sending. That chain is the
    /// whole backpressure mechanism.
    data_tx: Mutex<HashMap<i32, mpsc::Sender<Vec<u8>>>>,
    close_reasons: Mutex<HashMap<i32, CloseReasonSlot>>,
    /// Central and peripheral can be configured independently; `from_server`
    /// on the open callback says which side a socket belongs to.
    ///
    /// **Never read `encryption` from these.** They are process-global and the
    /// last role constructed wins, so a second default-configured `Central`
    /// would silently relax a first one that asked for `RequireEncryption`.
    /// The buffer sizes tolerate that (wrong size is a tuning bug); a security
    /// level does not. Each backend instance owns its own level — see
    /// `AndroidCentral::l2cap_encryption` — and passes it to [`secure_flag`].
    client_config: Mutex<L2capConfig>,
    server_config: Mutex<L2capConfig>,
}

fn queue_capacity(config: &L2capConfig) -> usize {
    config
        .effective_buffer_size()
        .div_ceil(config.effective_read_chunk_size())
        .max(MIN_QUEUE_CHUNKS)
}

static STATE: OnceLock<L2capState> = OnceLock::new();
static TOKIO_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

fn state() -> &'static L2capState {
    STATE.get().expect("L2CAP state not initialized")
}

pub(crate) fn tokio_handle() -> &'static tokio::runtime::Handle {
    TOKIO_HANDLE.get().expect("tokio handle not initialized")
}

pub(crate) fn init_statics() {
    let _ = STATE.set(L2capState {
        pending_server: Mutex::new(None),
        pending_open: Mutex::new(HashMap::new()),
        accept_tx: Mutex::new(None),
        data_tx: Mutex::new(HashMap::new()),
        close_reasons: Mutex::new(HashMap::new()),
        client_config: Mutex::new(L2capConfig::default()),
        server_config: Mutex::new(L2capConfig::default()),
    });
    let _ = TOKIO_HANDLE.set(tokio::runtime::Handle::current());
}

/// The manager class that owns the socket, which is whichever role opened it.
///
/// Both classes declare the whole L2CAP data-path surface, so the choice is
/// only about which half's socket table to look the id up in. Kept as one
/// function rather than three inline `if`s so the class at each
/// `call_static_method` site below is a single expression: `JniContractTest`
/// on the Kotlin side reads these sites to derive the JNI contract and cannot
/// resolve a local binding.
fn socket_class(is_server: bool) -> &'static Global<JClass<'static>> {
    if is_server {
        peripheral_class()
    } else {
        central_class()
    }
}

/// Tell Kotlin how large its socket reads should be.
///
/// The read loop lives on the Kotlin side, so `read_chunk_size` has to cross
/// the boundary or the configured value would size only the Rust-side bridge
/// while the socket kept reading its own fixed amount -- which would also make
/// [`queue_capacity`] describe a bound that isn't the real one.
fn push_read_buffer_size(config: &L2capConfig, is_server: bool) {
    let bytes = i32::try_from(config.effective_read_chunk_size()).unwrap_or(i32::MAX);
    let result = jvm().attach_current_thread(|env| {
        env.call_static_method(
            socket_class(is_server),
            jni_str!("setL2capReadBufferSize"),
            jni_sig!("(I)V"),
            &[bytes.into()],
        )?;
        Ok::<_, jni::errors::Error>(())
    });
    if let Err(e) = result {
        tracing::warn!("failed to set L2CAP read buffer size: {e}");
    }
}

pub(crate) fn set_client_config(config: L2capConfig) {
    push_read_buffer_size(&config, false);
    if let Some(s) = STATE.get() {
        *s.client_config.lock() = config;
    }
}

pub(crate) fn set_server_config(config: L2capConfig) {
    push_read_buffer_size(&config, true);
    if let Some(s) = STATE.get() {
        *s.server_config.lock() = config;
    }
}

/// Whether Kotlin should take the `secure` branch of the L2CAP socket APIs.
///
/// `BluetoothDevice::createL2capChannel` / `BluetoothAdapter::listenUsingL2capChannel`
/// require an authenticated, encrypted link; the `Insecure` variants require
/// neither. There is no middle setting, so `RequireEncryption` gets the secure
/// socket too — stronger than asked for, which is the safe direction to round.
///
/// Takes the level by value rather than reading it out of [`L2capState`]: see
/// the warning on `client_config`.
pub(crate) fn secure_flag(encryption: L2capEncryption) -> bool {
    match encryption {
        L2capEncryption::Insecure => false,
        L2capEncryption::RequireEncryption | L2capEncryption::RequireAuthentication => true,
    }
}

pub(crate) fn set_pending_server(tx: oneshot::Sender<BlewResult<Psm>>) {
    *state().pending_server.lock() = Some(tx);
}

pub(crate) fn complete_server_open(result: BlewResult<Psm>) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_server.lock().take()
    {
        let _ = tx.send(result);
    }
}

pub(crate) fn set_accept_tx(tx: AcceptSender) {
    *state().accept_tx.lock() = Some(tx);
}

pub(crate) fn set_pending_open(addr: String, tx: oneshot::Sender<BlewResult<L2capChannel>>) {
    state().pending_open.lock().insert(addr, tx);
}

fn close_socket(socket_id: i32, is_server: bool) {
    let _ = jvm().attach_current_thread(|env| {
        let _ = env.call_static_method(
            socket_class(is_server),
            jni_str!("closeL2cap"),
            jni_sig!("(I)V"),
            &[socket_id.into()],
        );
        Ok::<_, jni::errors::Error>(())
    });
}

pub(crate) fn on_channel_opened(device_addr: &str, socket_id: i32, from_server: bool) {
    let config = STATE.get().map_or_else(L2capConfig::default, |s| {
        if from_server {
            s.server_config.lock().clone()
        } else {
            s.client_config.lock().clone()
        }
    });
    let read_chunk = config.effective_read_chunk_size();

    let (app_half, bridge_half) = tokio::io::duplex(config.effective_buffer_size());
    let (mut bridge_reader, mut bridge_writer) = tokio::io::split(bridge_half);

    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(queue_capacity(&config));
    let close_reason = CloseReasonSlot::default();
    if let Some(s) = STATE.get() {
        s.data_tx.lock().insert(socket_id, data_tx);
        s.close_reasons
            .lock()
            .insert(socket_id, close_reason.clone());
    }

    let handle = TOKIO_HANDLE.get().expect("tokio handle not initialized");

    handle.spawn(async move {
        while let Some(data) = data_rx.recv().await {
            if bridge_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    let is_server = from_server;
    handle.spawn(async move {
        let mut buf = vec![0_u8; read_chunk];
        loop {
            match bridge_reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    // Kotlin's writeL2cap goes straight to a blocking
                    // BluetoothSocket OutputStream -- Android exposes no async
                    // socket API at any level -- so it must not run on a Tokio
                    // worker. Under the current-thread runtime the examples use
                    // it would stall the whole runtime; under a multi-threaded
                    // one it burns a worker per writing channel.
                    //
                    // Awaiting the blocking task rather than firing and
                    // forgetting is load-bearing twice over: it preserves
                    // backpressure into the caller's `write()`, and it keeps
                    // "this task has exited" equivalent to "the bytes are on
                    // the socket", which the lingering close below relies on.
                    let result = tokio::task::spawn_blocking(move || {
                        jvm().attach_current_thread(|env| {
                            let j_data = env.byte_array_from_slice(&data)?;
                            env.call_static_method(
                                socket_class(is_server),
                                jni_str!("writeL2cap"),
                                jni_sig!("(I[B)V"),
                                &[socket_id.into(), (&j_data).into()],
                            )?;
                            Ok::<_, jni::errors::Error>(())
                        })
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        break;
                    }
                }
            }
        }
        // Every write above completed before its `await` returned, so reaching
        // here means the outbound side is drained. Closing from *here* rather
        // than from the close hook is what makes a dropped channel finish
        // writing instead of discarding, matching the Apple reactor's
        // lingering close.
        close_socket(socket_id, is_server);
    });

    let linger = config.linger_timeout;
    let channel = L2capChannel::from_bridge(DuplexBridge {
        inner: app_half,
        close_hook: Some(Box::new(move || {
            // The outbound task above closes the socket as soon as it drains.
            // This only forces the issue if it never does -- a peer that has
            // stopped accepting data must not pin the socket open forever.
            let Some(limit) = linger else { return };
            if let Some(handle) = TOKIO_HANDLE.get() {
                handle.spawn(async move {
                    tokio::time::sleep(limit).await;
                    close_socket(socket_id, from_server);
                });
            }
        })),
        close_reason,
    });

    if from_server {
        if let Some(s) = STATE.get()
            && let Some(tx) = s.accept_tx.lock().as_ref()
        {
            let device_id = DeviceId(device_addr.to_string());
            if tx.send(Ok((device_id, channel))).is_err() {
                tracing::warn!(
                    socket_id,
                    "L2CAP accept receiver dropped, discarding channel"
                );
            }
        }
    } else if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_open.lock().remove(device_addr)
    {
        let _ = tx.send(Ok(channel));
    }
}

pub(crate) fn on_channel_data(socket_id: i32, data: &[u8]) {
    let Some(s) = STATE.get() else { return };
    // Clone the sender out rather than holding the map lock across the blocking
    // send below, which would stall every other socket's callbacks.
    let Some(tx) = s.data_tx.lock().get(&socket_id).cloned() else {
        return;
    };
    // Called on Kotlin's per-socket read thread, never a Tokio worker, so
    // blocking here is safe -- and is precisely the backpressure we want: the
    // thread stops draining the socket and the peer runs out of L2CAP credits.
    let _ = tx.blocking_send(data.to_vec());
}

pub(crate) fn on_channel_closed(socket_id: i32, error: Option<String>) {
    if let Some(s) = STATE.get() {
        // Record the reason *before* dropping the sender. Dropping it ends the
        // inbound task, which drops its half of the duplex, which is what the
        // application sees as EOF -- and `poll_read` consults the slot at that
        // moment. Setting it afterwards races that wake-up and can report a
        // transport failure as a clean end-of-stream.
        //
        // Kotlin funnels deliberate closes, read failures, link loss and write
        // failures through one callback; only the message distinguishes them.
        if let Some(slot) = s.close_reasons.lock().remove(&socket_id) {
            slot.set(match error {
                Some(message) => L2capCloseReason::TransportError(message),
                None => L2capCloseReason::Closed,
            });
        }
        s.data_tx.lock().remove(&socket_id);
    }
}

pub(crate) fn on_channel_error(device_addr: &str, error: String) {
    if let Some(s) = STATE.get()
        && let Some(tx) = s.pending_open.lock().remove(device_addr)
    {
        let _ = tx.send(Err(BlewError::L2cap {
            source: error.into(),
        }));
    }
}
