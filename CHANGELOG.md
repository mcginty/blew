# Changelog

All notable changes to `blew` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.3.0] — 2026-04-22

### Added

- `CentralConfig::connect_timeout: Option<Duration>` — deadline applied to
  `Central::connect()`. `Default` sets it to 15s; `None` restores pre-0.3
  unbounded-wait behavior. Applied uniformly across Apple, Linux, and Android
  backends; emits `CentralEvent::DeviceDisconnected { cause: Timeout }` when
  it fires.
- `BlewError::ConnectTimedOut(DeviceId)` — returned from `Central::connect`
  when the new deadline elapses. Distinct from `BlewError::Timeout`, which
  remains reserved for adapter-readiness waits (`wait_ready` / `wait_powered`).
- `BlewError::ConnectInFlight(DeviceId)` — returned when `Central::connect`
  is called for a device that already has a connect in flight. Previously the
  second caller silently stole the first caller's completion.

### Changed

- **Android `Central::disconnect` now awaits the `onConnectionStateChange`
  callback** (with a 2-second fallback) instead of returning `Ok` immediately
  after dispatching the JNI call. This prevents zombie `BluetoothGatt`
  handles when the callback never arrives — the fallback path calls a new
  Kotlin `forceClose(addr)` that flushes the service cache (`refresh()`) and
  closes the GATT handle synchronously, freeing the client-IF slot.
- **Android post-133 cleanup.** `onConnectionStateChange(DISCONNECTED,
  status=133)` now calls the hidden `BluetoothGatt.refresh()` before
  `close()` to flush the client-side service cache. The `connect()`
  stale-cleanup path also calls `refresh()` on the old handle and waits
  ~300ms before issuing the fresh `connectGatt()`, matching the canonical
  Android BLE back-off recommendation.
- Linux `Central::connect` deadline is now config-driven (via
  `CentralConfig::connect_timeout`) and returns `BlewError::ConnectTimedOut`
  on elapse. Previously it was hardcoded to 30s and returned the generic
  `BlewError::Timeout`.

### Fixed

- `Central::connect()` could hang indefinitely on Android if
  `onConnectionStateChange` never fired (e.g. radio busy, adapter thrash,
  status-133 zombie). The new `connect_timeout` default now bounds this;
  callers that need the old behavior can opt in with
  `CentralConfig { connect_timeout: None, .. }`.
- Overlapping `Central::connect()` calls for the same device no longer
  silently orphan the first caller's completion channel. Android, Apple:
  the second caller receives `BlewError::ConnectInFlight` immediately.
- Apple `Central::connect` silently evicted any pending completion channel
  when called twice concurrently for the same device, leaving the first
  caller hanging. Now rejected with `ConnectInFlight`.

---

## [0.2.3] — 2026-04-20

### Fixed

- Pin `kotlinx-coroutines-android` to 1.7.3 in the Android module for Tauri
  compatibility. 1.10.x conflicts with the coroutines version Tauri's own
  Android runtime pulls in.

## [0.2.2] — 2026-04-20

### Fixed

- Android Gradle module now declares `kotlinx-coroutines-android` as an
  `implementation` dependency. The Kotlin sources (`BleCentralManager`,
  `GattOperationQueue`) import `kotlinx.coroutines.*` but the coroutine
  runtime was previously only pulled in transitively via the host app, which
  failed for consumers that didn't already depend on it.

## [0.2.1] — 2026-04-20

### Fixed

- `tauri-plugin-blew` no longer fails to locate the Android Kotlin sources when
  consumed from crates.io. Previously `build.rs` resolved `../blew/android`
  relative to its own manifest, which works in the workspace but points to a
  non-existent sibling in the published tarball (Gradle then reported
  "No variants exist"). `blew` now emits its Android directory via the
  `links = "blew"` metadata channel, and `tauri-plugin-blew` reads it as
  `DEP_BLEW_ANDROID_DIR` at build time.

## [0.2.0] — 2026-04-20

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
- Moved the long platform notes and bare Android setup guide out of the top-level
  README into `docs/platform-notes.md` and `docs/android-without-tauri.md`.
- `tauri-plugin-blew`: `BlewPluginConfig` + `init_with_config()` to opt out of
  auto-requesting Android BLE permissions at plugin load, and
  `request_ble_permissions()` to trigger the runtime dialog on demand (e.g.
  after an in-app explanation modal). Default behavior (`init()`) is unchanged.
- `blew::platform::android::request_ble_permissions()` — fire-and-forget helper
  that invokes the Tauri plugin's static method over JNI to show the Android
  runtime-permissions dialog. Requires the Tauri plugin to have loaded.
- `tauri-plugin-blew::permission_events()` + `BlePermissionStatus` — broadcast
  stream emitted whenever the aggregate Android BLE-permission state flips
  between granted and denied. Detected in `BlewPlugin.onResume`, so it covers
  both in-app dialog responses and out-of-app toggles (e.g. the user disabling
  a permission in system Settings while the app is backgrounded).

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

## Upgrade guide — 0.2.x → 0.3.0

**If you were constructing `CentralConfig` as a struct literal**, it gains a
new field:

```rust
// Before
let config = CentralConfig { restore_identifier: Some("...".into()) };

// After — either spread the default or set the new field explicitly
let config = CentralConfig {
    restore_identifier: Some("...".into()),
    ..CentralConfig::default()
};
```

**If you were matching on `BlewError::Timeout` for a connect failure on
Linux**, the variant narrowed to `BlewError::ConnectTimedOut(DeviceId)`:

```rust
// Before — Linux connect timeout returned the generic variant
match central.connect(&id).await {
    Err(BlewError::Timeout) => /* ... */,
    _ => /* ... */,
}

// After
match central.connect(&id).await {
    Err(BlewError::ConnectTimedOut(_)) => /* ... */,
    _ => /* ... */,
}
```

`BlewError::Timeout` is still used by `Central::wait_ready`,
`Peripheral::wait_ready`, and `wait_powered`.

**If you relied on unbounded connect waits**, set
`CentralConfig.connect_timeout = None` when constructing the central:

```rust
let central: Central = Central::with_config(CentralConfig {
    connect_timeout: None,
    ..CentralConfig::default()
}).await?;
```

**If overlapping `connect()` calls were previously "works by luck"**: the
second caller now receives `BlewError::ConnectInFlight(DeviceId)` immediately
on Apple and Android. If you need shared-completion fan-out semantics,
implement them at your layer (e.g. wrap the shared future in
`futures::future::Shared`). Linux still permits concurrent connects via
bluer's own state machine.

---

## [0.1.0]

Initial release.
