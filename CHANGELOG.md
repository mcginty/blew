# Changelog

All notable changes to `blew` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.0] — unreleased

### Added

- iOS state restoration for both roles. `Central::with_config` and
  `Peripheral::with_config` accept a `restore_identifier`; after construction,
  `take_restored()` drains the peripherals/services preserved by `willRestoreState:`.
  Available only on Apple targets (`target_vendor = "apple"`).
- Single-consumer GATT request stream on `Peripheral::take_requests()`, yielding
  `PeripheralRequest` values with RAII `ReadResponder`/`WriteResponder` handles —
  the type system enforces that exactly one consumer owns incoming requests.
- In-memory mock backends (`testing::MockLink`, `MockCentral`, `MockPeripheral`)
  behind the `testing` feature, with fault injection for BLE-semantic edge cases
  (post-disconnect ops, adapter off, duplicate subscribes, dropped notifications).
- Two-host integration examples: `integration_central`, `integration_peripheral`
  (GATT + L2CAP speedtest with live progress), and `restore.rs` (iOS launch sequence).
- Typed error variants: `StreamClosed`, `DisconnectedDuringOperation`, `DiscoveryFailed`.
- Expanded README guidance for Linux central to Apple peripherals, explaining why
  BlueZ/CoreBluetooth pairing prompts happen and documenting the recommended
  `main.conf` workarounds (`DisablePlugins=battery,deviceinfo`, `[GATT] Cache=no`,
  `JustWorksRepairing=always` for re-pairing).

### Changed

- **Apple L2CAP transport rewritten.** Replaced per-channel worker threads with a
  single `NSRunLoop` reactor; all channels now share one run-loop with explicit
  register/write/close commands over an `mpsc`. Fixes prior close/shutdown bugs.
- **Peripheral events split.** The old unified `PeripheralEvent` stream is gone.
  State-like events (adapter power, subscription changes) fan out via
  `Peripheral::state_events()`; inbound GATT read/write requests go to
  `Peripheral::take_requests()`.
- **Event fan-out unified on `tokio::sync::broadcast`.** The bespoke `EventFanout`
  utility was removed. All three Central backends and Peripheral state streams
  use `broadcast::channel(256)` wrapped in `BroadcastEventStream` (silently drops
  `Lagged` errors). Slow subscribers now miss events but stay connected; the old
  behavior was to disconnect slow subscribers entirely.
- **L2CAP accept channels unbounded on Apple and Android.** The previous
  bounded(16) could block the CoreBluetooth GCD delegate queue on Apple and
  silently drop incoming channels on Android. Linux keeps its bounded +
  `await` design so pressure flows through BlueZ's kernel socket queue.
- Android auto-requests MTU 512 on connect; per-device Kotlin coroutine queue
  serializes GATT ops so one slow peer can't block others.
- Linux `Central::connect` now times out after 30s rather than blocking
  indefinitely against BlueZ.

### Removed

- `PeripheralEvent` enum. Use `state_events()` + `take_requests()`.
- `Central::take_restored()` / `Peripheral::take_restored()` from the
  sealed-trait surface on Android and Linux. The method exists only on Apple.
- `util::event_fanout` module (`EventFanout`, `EventFanoutTx`).
- The `Restored` variant previously emitted on the event stream (iOS state
  restoration now flows through `take_restored()` instead of an event).

### Fixed

- Apple L2CAP close hook no longer fires twice when the outbound bridge task
  observes EOF — the `DuplexTransport` drop impl is now the single sender.
- Multiple backend races and GATT-queue bugs shaken out by the new test
  harness (see `test(mock)` and `fix:` commits in the history).
- Linux clippy pedantic cleanup across the BlueZ backend.

---

## Upgrade guide — 0.1.x → 0.2.0

If you were previously handling `PeripheralEvent`:

```rust
// Before (0.1.x)
let mut events = peripheral.events();
while let Some(ev) = events.next().await {
    match ev {
        PeripheralEvent::AdapterStateChanged { .. } => { /* ... */ }
        PeripheralEvent::ReadRequest { .. }        => { /* ... */ }
        PeripheralEvent::WriteRequest { .. }       => { /* ... */ }
        PeripheralEvent::SubscriptionChanged { .. } => { /* ... */ }
    }
}
```

```rust
// After (0.2.0) — state + requests are separate streams
let mut state = peripheral.state_events();
let mut requests = peripheral.take_requests()
    .expect("requests stream already taken");

tokio::spawn(async move {
    while let Some(ev) = state.next().await {
        match ev {
            PeripheralStateEvent::AdapterStateChanged { .. } => { /* ... */ }
            PeripheralStateEvent::SubscriptionChanged { .. } => { /* ... */ }
        }
    }
});

while let Some(req) = requests.next().await {
    match req {
        PeripheralRequest::Read { responder, .. }  => responder.respond(Ok(b"...".into())),
        PeripheralRequest::Write { responder, .. } => responder.respond(Ok(())),
    }
}
```

Key points:
- `take_requests()` returns `Option` — the first caller gets `Some`, the rest get `None`.
  Requests are single-consumer by design (the `ReadResponder`/`WriteResponder` handles
  move into the consumer and respond via RAII).
- `state_events()` can be called multiple times; each caller gets an independent
  broadcast receiver.

**If you were calling `take_restored()` on non-Apple platforms:**

```rust
// Before — compiled everywhere, returned None on non-Apple
if let Some(devices) = central.take_restored() { /* ... */ }

// After — cfg-gate the call, or just delete it
#[cfg(target_vendor = "apple")]
if let Some(devices) = central.take_restored() { /* ... */ }
```

**If you were matching on `BlewError::Internal(_)`:**

Some call sites now return typed variants. Match them explicitly, or keep a
catch-all:

```rust
match err {
    BlewError::StreamClosed                   => /* previously Internal("...") */,
    BlewError::DisconnectedDuringOperation(_) => /* previously Internal("...") */,
    BlewError::DiscoveryFailed(_)             => /* previously Internal("...") */,
    BlewError::Internal(msg)                  => /* remaining JNI/NSError fallbacks */,
    _ => { /* ... */ }
}
```

**If you were using `EventFanout` or `EventFanoutTx` directly:**

These were `pub use`'d from `util` but are removed. If you reached into them
(unlikely — they were primarily a backend implementation detail), switch to
`tokio::sync::broadcast` + `BroadcastEventStream` from `util::event_stream`.

---

## [0.1.0]

Initial release.
