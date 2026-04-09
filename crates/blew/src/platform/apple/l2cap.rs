//! Shared L2CAP stream bridging for Apple platforms.
//!
//! Both `AppleCentral` and `ApplePeripheral` use this to wrap a
//! `CBL2CAPChannel` (NSInputStream + NSOutputStream) into an async
//! `L2capChannel`.
//!
//! ## Why we need a dedicated run loop thread
//!
//! `CBL2CAPChannel` vends `NSInputStream`/`NSOutputStream` objects.  These are
//! backed by CoreBluetooth's internal socket layer, which delivers data via
//! CoreFoundation run-loop callbacks (`kCFStreamEventHasBytesAvailable` etc.).
//! Without a run loop spinning, those callbacks never fire and
//! `read:maxLength:` returns 0 immediately even when data is in flight.
//!
//! We create a single global background thread that runs a `CFRunLoop` forever
//! and schedule every L2CAP stream pair on it.  `read:maxLength:` /
//! `write:maxLength:` are then called from Tokio `spawn_blocking` tasks; they
//! will block until data is available (or the stream closes).

#![allow(clippy::cast_possible_truncation)]

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use objc2::msg_send;
use objc2_core_bluetooth::CBL2CAPChannel;
use tokio::runtime::Handle;

use crate::l2cap::L2capChannel;
use crate::platform::apple::helpers::retain_send;

const L2CAP_DUPLEX_BUF_SIZE: usize = 65536;
const L2CAP_READ_BUF_SIZE: usize = 4096;

// We use CF directly (toll-free bridged with NS) because it gives us the
// CFRunLoop reference we need to schedule streams from another thread.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRun();
    fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: *const c_void);
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopTimerCreate(
        allocator: *const c_void,
        fire_date: f64,
        interval: f64,
        flags: u32,
        order: isize,
        callback: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
        context: *mut c_void, // CFRunLoopTimerContext* -- NULL means no context
    ) -> *mut c_void;
    fn CFReadStreamScheduleWithRunLoop(
        stream: *const c_void,
        run_loop: *mut c_void,
        mode: *const c_void,
    );
    fn CFWriteStreamScheduleWithRunLoop(
        stream: *const c_void,
        run_loop: *mut c_void,
        mode: *const c_void,
    );
    static kCFRunLoopDefaultMode: *const c_void;
}

unsafe extern "C" fn noop_timer_cb(_timer: *mut c_void, _info: *mut c_void) {}

/// A `CFRunLoopRef` pointer that is `Send + Sync` (safe because we only use it
/// to schedule streams from the creating thread and from the bridge function).
#[derive(Copy, Clone)]
struct RunLoopRef(*mut c_void);
unsafe impl Send for RunLoopRef {}
unsafe impl Sync for RunLoopRef {}

/// Returns the `CFRunLoopRef` of the dedicated background stream I/O thread,
/// creating it on first call.
fn stream_run_loop() -> RunLoopRef {
    static RUN_LOOP: OnceLock<RunLoopRef> = OnceLock::new();
    *RUN_LOOP.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("blew-l2cap".to_string())
            .spawn(move || unsafe {
                let rl = CFRunLoopGetCurrent();
                // A repeating dummy timer keeps the run loop alive even before
                // the first stream is scheduled (CFRunLoopRun exits immediately
                // if there are no sources or timers).
                let timer = CFRunLoopTimerCreate(
                    std::ptr::null(),                 // default allocator
                    CFAbsoluteTimeGetCurrent() + 1e9, // first fire: far future
                    1e9,                              // repeat interval: ~31 years
                    0,                                // no flags
                    0,                                // order
                    Some(noop_timer_cb),
                    std::ptr::null_mut(), // no context
                );
                CFRunLoopAddTimer(rl, timer, kCFRunLoopDefaultMode);
                tx.send(RunLoopRef(rl)).unwrap();
                CFRunLoopRun(); // spins until the process exits
            })
            .expect("spawn blew-l2cap run loop thread");
        rx.recv().expect("receive run loop ref")
    })
}

/// Wrap an Apple `CBL2CAPChannel` into a `L2capChannel` (AsyncRead + AsyncWrite).
///
/// # Ownership / lifetime
/// The `CBL2CAPChannel` is retained for the lifetime of the bridge: Apple's
/// docs say "you should retain the channel yourself" -- without this, CB may
/// release the channel after the delegate callback returns, closing the streams.
pub(crate) fn bridge_l2cap_channel(channel: &CBL2CAPChannel, runtime: &Handle) -> L2capChannel {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    // Schedule both streams on the background run loop BEFORE opening them so
    // that the CF/NS stream machinery has a run loop to deliver I/O events to.
    // Without this, read:maxLength: returns 0 immediately even when data is in
    // flight because the kernel I/O callbacks never fire.
    let rl = stream_run_loop();
    unsafe {
        CFReadStreamScheduleWithRunLoop(
            (&raw const **input).cast::<c_void>(),
            rl.0,
            kCFRunLoopDefaultMode,
        );
        CFWriteStreamScheduleWithRunLoop(
            (&raw const **output).cast::<c_void>(),
            rl.0,
            kCFRunLoopDefaultMode,
        );
        let _: () = msg_send![&**input, open];
        let _: () = msg_send![&**output, open];
    }

    let (app_side, io_side) = tokio::io::duplex(L2CAP_DUPLEX_BUF_SIZE);
    let (mut io_reader, mut io_writer) = tokio::io::split(io_side);

    let inp = Arc::clone(&input);
    let ch_in = Arc::clone(&channel);
    runtime.spawn(async move {
        let _ch = ch_in; // keep channel alive for the duration of this task
        loop {
            let inp2 = Arc::clone(&inp);
            let data = tokio::task::spawn_blocking(move || -> Option<Vec<u8>> {
                let mut buf = vec![0_u8; L2CAP_READ_BUF_SIZE];
                let n: isize = unsafe {
                    msg_send![
                        &**inp2,
                        read: buf.as_mut_ptr(),
                        maxLength: buf.len()
                    ]
                };
                if n <= 0 {
                    return None;
                }
                buf.truncate(n.cast_unsigned());
                Some(buf)
            })
            .await;
            match data {
                Ok(Some(bytes)) => {
                    if io_writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    let out = Arc::clone(&output);
    let ch_out = Arc::clone(&channel);
    runtime.spawn(async move {
        let _ch = ch_out; // keep channel alive for the duration of this task
        let mut buf = vec![0_u8; L2CAP_READ_BUF_SIZE];
        loop {
            let n = match io_reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let data = buf[..n].to_vec();
            let out2 = Arc::clone(&out);
            let ok = tokio::task::spawn_blocking(move || -> bool {
                let mut pos = 0;
                while pos < data.len() {
                    let n: isize = unsafe {
                        msg_send![
                            &**out2,
                            write: data[pos..].as_ptr(),
                            maxLength: data.len() - pos
                        ]
                    };
                    if n <= 0 {
                        return false;
                    }
                    pos += n.cast_unsigned();
                }
                true
            })
            .await;
            if !matches!(ok, Ok(true)) {
                break;
            }
        }
    });

    L2capChannel::from_duplex(app_side)
}
