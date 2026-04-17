//! Shared L2CAP stream bridging for Apple platforms.
//!
//! Both `AppleCentral` and `ApplePeripheral` use this to wrap a
//! `CBL2CAPChannel` (NSInputStream + NSOutputStream) into an async
//! `L2capChannel`.
//!
//! ## Why we need a dedicated worker thread
//!
//! `CBL2CAPChannel` vends `NSInputStream`/`NSOutputStream` objects.  These are
//! backed by CoreBluetooth's internal socket layer, which delivers data via
//! Foundation run-loop callbacks. Without a run loop spinning, those callbacks
//! never fire and `read:maxLength:` returns 0 immediately even when data is in
//! flight.
//!
//! Each L2CAP channel gets a dedicated worker thread. That thread owns the
//! streams and performs all stream lifecycle and I/O operations on the same
//! thread, which avoids Apple thread-affinity issues during steady-state I/O
//! and shutdown.

#![allow(clippy::cast_possible_truncation)]

use std::ptr::NonNull;
use std::sync::{Arc, mpsc};

use objc2::rc::autoreleasepool;
use objc2_core_bluetooth::CBL2CAPChannel;
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSInputStream, NSOutputStream, NSRunLoop};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::trace;

use crate::l2cap::L2capChannel;
use crate::platform::apple::helpers::{ObjcSend, retain_send};

const L2CAP_DUPLEX_BUF_SIZE: usize = 65536;
const L2CAP_READ_BUF_SIZE: usize = 4096;
const RUN_LOOP_POLL_SECS: f64 = 0.05;

enum BridgeCmd {
    Write(Vec<u8>),
    Stop,
}

type InputStream = Arc<ObjcSend<NSInputStream>>;
type OutputStream = Arc<ObjcSend<NSOutputStream>>;

fn default_run_loop_mode() -> &'static objc2_foundation::NSRunLoopMode {
    unsafe { NSDefaultRunLoopMode }
}

fn schedule_streams(input: &InputStream, output: &OutputStream, run_loop: &NSRunLoop) {
    unsafe {
        input.scheduleInRunLoop_forMode(run_loop, default_run_loop_mode());
        output.scheduleInRunLoop_forMode(run_loop, default_run_loop_mode());
    }
}

struct Worker {
    _channel: Arc<ObjcSend<CBL2CAPChannel>>,
    input: InputStream,
    output: OutputStream,
    cmd_rx: mpsc::Receiver<BridgeCmd>,
    inbound_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
}

impl Worker {
    fn with_default_mode<T>(f: impl FnOnce(&objc2_foundation::NSRunLoopMode) -> T) -> T {
        f(default_run_loop_mode())
    }

    fn init(&self, run_loop: &NSRunLoop) {
        trace!("apple L2CAP worker init: schedule streams");
        schedule_streams(&self.input, &self.output, run_loop);

        trace!("apple L2CAP worker init: open input stream");
        self.input.open();
        trace!("apple L2CAP worker init: open output stream");
        self.output.open();
        trace!("apple L2CAP worker started");
    }

    fn handle_command(&self, cmd: BridgeCmd) -> bool {
        match cmd {
            BridgeCmd::Write(data) => self.write_chunk(&data),
            BridgeCmd::Stop => {
                trace!("apple L2CAP worker received stop");
                false
            }
        }
    }

    fn write_chunk(&self, data: &[u8]) -> bool {
        trace!(
            len = data.len(),
            "apple L2CAP worker writing outbound chunk"
        );
        let mut pos = 0;
        while pos < data.len() {
            let n = unsafe {
                self.output.write_maxLength(
                    NonNull::new(data[pos..].as_ptr().cast_mut()).expect("data ptr"),
                    data.len() - pos,
                )
            };
            if n <= 0 {
                trace!("apple L2CAP worker stopping: output write failed");
                return false;
            }
            pos += n.cast_unsigned();
        }
        true
    }

    fn forward_available_input(&self) -> bool {
        while self.input.hasBytesAvailable() {
            let mut buf = vec![0_u8; L2CAP_READ_BUF_SIZE];
            let n = unsafe {
                self.input
                    .read_maxLength(NonNull::new(buf.as_mut_ptr()).expect("buf ptr"), buf.len())
            };
            if n <= 0 {
                trace!("apple L2CAP worker stopping: input read returned EOF/closed");
                return false;
            }

            buf.truncate(n.cast_unsigned());
            trace!(
                len = buf.len(),
                "apple L2CAP worker forwarding inbound chunk"
            );
            if self.inbound_tx.send(buf).is_err() {
                trace!("apple L2CAP worker stopping: inbound channel closed");
                return false;
            }
        }

        true
    }

    fn poll_run_loop(run_loop: &NSRunLoop) {
        let deadline = NSDate::dateWithTimeIntervalSinceNow(RUN_LOOP_POLL_SECS);
        Self::with_default_mode(|mode| run_loop.acceptInputForMode_beforeDate(mode, &deadline));
    }

    fn pump(&self, run_loop: &NSRunLoop) {
        loop {
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                if !self.handle_command(cmd) {
                    return;
                }
            }

            if !self.forward_available_input() {
                return;
            }

            Self::poll_run_loop(run_loop);
        }
    }

    fn shutdown(&self, run_loop: &NSRunLoop) {
        trace!("apple L2CAP worker shutdown: unschedule streams");
        Self::with_default_mode(|mode| unsafe {
            self.input.removeFromRunLoop_forMode(run_loop, mode);
            self.output.removeFromRunLoop_forMode(run_loop, mode);
        });
        trace!("apple L2CAP worker shutdown: close input stream");
        self.input.close();
        trace!("apple L2CAP worker shutdown: close output stream");
        self.output.close();
        trace!("apple L2CAP worker exited");
    }

    fn run(self) {
        autoreleasepool(|_| {
            trace!("apple L2CAP worker init: get current run loop");
            let run_loop = NSRunLoop::currentRunLoop();
            self.init(&run_loop);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pump(&run_loop)));
            if result.is_err() {
                trace!("apple L2CAP worker panicked");
            }
            self.shutdown(&run_loop);
        });
    }
}

/// Wrap an Apple `CBL2CAPChannel` into a `L2capChannel` (AsyncRead + AsyncWrite).
///
/// # Ownership / lifetime
/// The `CBL2CAPChannel` is retained for the lifetime of the bridge: Apple's
/// docs say "you should retain the channel yourself" -- without this, CB may
/// release the channel after the delegate callback returns, closing the streams.
pub(crate) fn bridge_l2cap_channel(channel: &CBL2CAPChannel, runtime: &Handle) -> L2capChannel {
    let channel = Arc::new(unsafe { retain_send(channel) });

    let inp = unsafe { channel.inputStream() };
    let Some(inp) = inp else {
        let (app_side, _io_side) = tokio::io::duplex(64);
        return L2capChannel::from_duplex(app_side);
    };
    let out = unsafe { channel.outputStream() };
    let Some(out) = out else {
        let (app_side, _io_side) = tokio::io::duplex(64);
        return L2capChannel::from_duplex(app_side);
    };

    let input = Arc::new(unsafe { retain_send(&*inp) });
    let output = Arc::new(unsafe { retain_send(&*out) });
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (inbound_tx, mut inbound_rx) = tokio_mpsc::unbounded_channel();

    std::thread::Builder::new()
        .name("blew-l2cap".to_string())
        .spawn({
            let worker = Worker {
                _channel: Arc::clone(&channel),
                input: Arc::clone(&input),
                output: Arc::clone(&output),
                cmd_rx,
                inbound_tx,
            };
            move || worker.run()
        })
        .expect("spawn blew-l2cap worker");

    let (app_side, io_side) = tokio::io::duplex(L2CAP_DUPLEX_BUF_SIZE);
    let (mut io_reader, mut io_writer) = tokio::io::split(io_side);

    runtime.spawn(async move {
        trace!("apple L2CAP inbound async bridge started");
        while let Some(bytes) = inbound_rx.recv().await {
            if io_writer.write_all(&bytes).await.is_err() {
                trace!("apple L2CAP inbound async bridge stopping: app-side writer closed");
                break;
            }
        }
        trace!("apple L2CAP inbound async bridge exited");
    });

    let cmd_tx_for_io = cmd_tx.clone();
    runtime.spawn(async move {
        trace!("apple L2CAP outbound async bridge started");
        let mut buf = vec![0_u8; L2CAP_READ_BUF_SIZE];
        loop {
            let n = match io_reader.read(&mut buf).await {
                Ok(0) => {
                    trace!("apple L2CAP outbound async bridge stopping: app-side reader EOF");
                    break;
                }
                Err(e) => {
                    trace!("apple L2CAP outbound async bridge stopping: app-side read failed: {e}");
                    break;
                }
                Ok(n) => n,
            };
            if cmd_tx_for_io
                .send(BridgeCmd::Write(buf[..n].to_vec()))
                .is_err()
            {
                trace!("apple L2CAP outbound async bridge stopping: worker channel closed");
                break;
            }
        }
        let _ = cmd_tx_for_io.send(BridgeCmd::Stop);
        trace!("apple L2CAP outbound async bridge exited");
    });

    L2capChannel::from_duplex_with_close_hook(app_side, move || {
        let _ = cmd_tx.send(BridgeCmd::Stop);
    })
}
