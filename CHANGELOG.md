# Changelog

All notable changes to `blew` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **`NotifyKind::{Notify, Indicate}` — per-call selection of the ATT write
  kind in `Peripheral::notify_characteristic`** (issue #18). `NotifyKind::Indicate`
  sends an ATT Handle Value Indication; `NotifyKind::Notify` (default) a
  Handle Value Notification. Android honours the selection exactly. Apple and
  Linux derive the wire format from the property the central subscribed
  through (the CCCD) and reject an unsupported kind, and the mock backend
  mirrors them, with **`BlewError::NotifyKindMismatch { char_uuid,
  requested }`** as the typed error. An unknown characteristic id is still a
  `LocalCharacteristicNotFound`, matching Apple and Android.

- **Android `notify_characteristic` now resolves `NotifyKind::Indicate` only
  once the stack reports the send** via `onNotificationSent` (issue #9). For
  an indication that is after the peer's ATT confirmation — a true
  acknowledgement. A five-second backstop degrades a stack that never reports
  to "accepted". A stack that rejects the send outright fails fast with an
  error instead of looking busy. Apple and Linux already resolve
  appropriately: CoreBluetooth on queue acceptance and BlueZ after the peer
  confirms. `nativeOnNotificationSent`
  was added to the Android JNI bridge to carry the callback into Rust; the
  call is tagged with a monotonic seq so a busy-retry can never resolve a newer
  call with an older callback.

- **Linux indication support.** Characteristics declaring
  `CharacteristicProperties::INDICATE` now register an indication CCCD, and
  BlueZ resolves the notifier only after the peer confirms a `NotifyKind::Indicate`.

### Changed

- **Breaking: `Peripheral::notify_characteristic` gained a `kind:
  NotifyKind` argument** before `value` (issue #18). It is a single type
  parameter — not an options struct — so the compiler points you at every call
  site. See the upgrade guide below.

### Fixed

- **Android: `BleCentralManager.init` / `BlePeripheralManager.init` crashed on
  startup with `MethodNotFound`.** Both `init(Context)` methods were missing
  `@JvmStatic`, so on a Kotlin `object` they only compiled as instance methods
  reachable through the synthesized `INSTANCE` field. `init_jvm()` calls them
  with `call_static_method`, which found no matching static method and
  panicked, aborting the process. Every other Rust-called method in these
  files already carried `@JvmStatic`; `init` was the one omission.

## [0.4.0-beta.1] — 2026-09-01

### Added

- **CI now compiles the Android Kotlin.** It previously ran only ktlint, which
  checks style and not whether the code builds, so every Kotlin change shipped
  uncompiled. A `compile-kotlin` job builds `tauri-plugin-blew` for Android
  (which generates the Tauri annotations the plugin sources need) and then
  runs `compileDebugKotlin`. Locally: `mise run ci:compile-kotlin`.

- The JNI parity test now covers `BlewPluginNative` and compares **return
  types** as well as parameters. Those two hooks had their Kotlin in `blew`
  and their Rust in `tauri-plugin-blew`, so they fell between the crates and
  nothing checked them at all.

- The JNI parity test now compares **signatures**, not just names. It reports
  arity and per-parameter type drift between each Kotlin `external fun` and
  its Rust `extern "C"` hook — a mismatch the JVM otherwise only surfaces as a
  crash at call time, on a device. Verified by mutation: both a changed
  parameter type and a dropped parameter fail the test.

- **Breaking: `L2capConfig`**, on `CentralConfig::l2cap` and
  `PeripheralConfig::l2cap`: `buffer_size`, `read_chunk_size` and
  `linger_timeout`. Neither config struct is `#[non_exhaustive]`, so an
  external struct literal that names every field no longer compiles; see the
  upgrade guide below. Values below the documented floors are raised rather
  than honoured — a zero-size buffer would deadlock rather than throttle.
  Linux observes none of them — `bluer::l2cap::Stream` is already an async byte stream handed straight
  to the caller, so there is no in-process bridge to size and nothing queued
  locally to flush. Apple and Android observe all three.
- **`L2capChannel::close_reason()` and `L2capCloseReason`.** A dropped ACL link
  and a polite hangup previously both surfaced as a clean end-of-stream.
  `LinkLost` and `TransportError` now also surface from `AsyncRead` as an
  `io::Error` (`ConnectionReset` and `Other` respectively), so a failure is no
  longer indistinguishable from the peer closing normally.
- `Psm` now implements `Display`.

### Changed

- **Breaking: `Peripheral::start_advertising` now waits for the stack to
  confirm.** On Android it returned `Ok(())` as soon as the request reached
  `BluetoothLeAdvertiser`, so a peripheral that failed to start — too many
  advertisers, an unsupported payload, Bluetooth off — reported success and
  was silently invisible. It now awaits `AdvertiseCallback` and surfaces the
  failure, matching what the Apple backend already did. Callers that ignored
  the result see no change; callers that checked it may now see errors they
  previously did not.

- **Closing an L2CAP channel now lingers instead of discarding.** Both
  `close()` and dropping the channel hand it to the backend, which keeps
  writing whatever is still queued until the queue empties or
  `L2capConfig::linger_timeout` (default 1s) passes — `SO_LINGER` semantics.
  Previously the queued bytes were dropped on the floor.

  Neither path blocks the caller, and neither has a delivery advantage over
  the other. That symmetry is the point: an `AsyncWrite` that only kept your
  data when you remembered to close it explicitly would be a trap, and
  dropping is by far the more common way one of these goes away.

  What this does *not* promise is peer acknowledgement — lingering covers
  delivery to the platform socket, nothing beyond it.
- **Breaking: `Peripheral::l2cap_listener` and `Central::open_l2cap_channel`
  now report an unusable channel as an error.** Apple previously logged a
  warning and returned a channel whose peer half had already been dropped, so
  the caller got `Ok(channel)` and an immediate EOF.

- **`BleDevice::manufacturer_data` and `BleDevice::service_data`.**
  Manufacturer-specific data is how most peer-to-peer BLE apps establish
  identity at discovery time, and every backend was already receiving it from
  the OS and discarding it. `manufacturer_data` is keyed by the 16-bit
  Bluetooth SIG company identifier, `service_data` by service UUID; both are
  empty when the peer advertised none.

  There is deliberately no matching field on `AdvertisingConfig`.
  CoreBluetooth's `startAdvertising:` accepts only
  `CBAdvertisementDataLocalNameKey` and `CBAdvertisementDataServiceUUIDsKey`,
  so an Apple peripheral cannot advertise manufacturer data at all — exposing
  it would make the field a silent no-op on one of three backends.

- `Default` for `GattService`, `GattCharacteristic`, `GattDescriptor`,
  `CharacteristicProperties` and `AttributePermissions`, plus
  `AdvertisingConfig`, so all of them can be built with the
  `..Default::default()` spread that `CentralConfig` already documented.
  `GattService::default()` sets `primary: true` rather than the `false` a
  derive would give — secondary services exist only to be included by
  another service and are vanishingly rare.

- **Breaking: `BleDevice` is now `#[non_exhaustive]`.** It is produced by the
  library and never constructed by callers, so this costs nothing today and
  means future advertisement fields stop being breaking changes. Exhaustive
  destructuring (`let BleDevice { id, name, rssi, services } = dev;`) needs a
  trailing `..`.

  Config and GATT structs were deliberately left constructible.
  `#[non_exhaustive]` forbids *every* struct expression from another crate —
  functional-update syntax included — so marking `CentralConfig` or
  `AdvertisingConfig` would have broken the `..Default::default()` idiom
  rather than protecting it, and left callers with no construction path
  short of builders. Adding a field to those remains a breaking change,
  which is the right trade while the crate is pre-1.0.

- **Breaking: `PeripheralRequest::Write` gains an `offset: u16` field**,
  matching the one `PeripheralRequest::Read` already carried. Without it an
  application had no way to reassemble a long write — every backend received
  the offset from the OS and discarded it at the Rust boundary. See the
  upgrade guide below.
- `Central::refresh` on non-Android targets is now declared as
  `fn refresh(..) -> impl Future<Output = BlewResult<()>> + Send` instead of
  `async fn`. Callers that `.await` it are unaffected; the change silences a
  new `clippy::unused_async_trait_impl` error that broke the CI lint gate.

### Fixed

- **Android: `start_advertising` now rejects an overlapping call with
  `AlreadyAdvertising`,** which is what the `PeripheralBackend` contract has
  always documented and what Apple, Linux and the mock backend already did.
  Android was the only backend that accepted a second concurrent start — and
  because the platform can only stop an advertisement by handing back the
  exact `AdvertiseCallback` it was started with, the first advertisement was
  left running with nothing able to reach it.

- **Android: advertising state is a single value, not a flag beside a slot.**
  The `Starting` → `Active` transition now happens under the same lock that
  takes the waiter, before the waiting task is woken. Previously a `stop`
  landing between the wake-up and the flag being set was undone by the
  resuming task, which left the slot claimed while the radio was idle and
  wedged every later start on `AlreadyAdvertising`. A `stop` during startup
  also takes the slot and wakes the in-flight start rather than letting it sit
  out its deadline, and `start_advertising` cleans up through a guard so a
  dropped or cancelled future cannot leave the radio advertising behind a
  callback nothing can reach. The state machine lives in
  `util::advertise_state` so it is unit-tested on every host rather than only
  on a device.

- **Android: the Kotlin advertising state is synchronized.** `startAdvertising`,
  `stopAdvertising`, `cancelAdvertising` and the `AdvertiseCallback` mutate the
  same fields from JNI and stack threads; a stop could previously pass through
  the gap between a start deciding to advertise and recording its callback, so
  advertising began after the stop had returned.

- **Android: a refusal from the Kotlin side maps to `AlreadyAdvertising`**
  rather than being collapsed into "advertiser unavailable".

- **Android: an abandoned advertising request can no longer complete a later
  one.** Requests carry an id, so a callback arriving after a timeout is
  dropped rather than applied to whoever is waiting by then, and the failure
  paths — timeout, JNI error, refusal — release the slot and tear down the
  stack-side request instead of leaving it running. An advertising timeout
  also reports `BlewError::Peripheral` rather than `BlewError::Timeout`, which
  stays reserved for adapter-readiness waits.

- **Android: the advertiser is resolved per call instead of cached at init.**
  `getBluetoothLeAdvertiser` returns null while Bluetooth is off, and nothing
  refreshed the cached null when it came back on — so a peripheral initialised
  with Bluetooth off could never advertise for the life of the process.

- **tauri-plugin-blew: the startup `Activity` is no longer retained for the
  life of the process.** `ndk_context` was given a global reference to the
  `Activity`, which keeps its whole view hierarchy — `WebView` included —
  alive past every recreation. It now receives the application context, which
  is all either consumer needs: both want a classloader, and the
  `Application`'s is the same one. (The `WeakReference` change below removed a
  second, smaller leak; this is the one that dominated.)

- **Android: the public permission helpers no longer panic before
  initialisation.** `are_ble_permissions_granted`, `request_ble_permissions`
  and `is_emulator` all reached a panicking JVM accessor if the plugin had not
  been registered — including through `tauri-plugin-blew`'s wrappers, and
  despite `request_ble_permissions` documenting a no-op for exactly that case.
  They now report `false`/no-op with a warning, and the new `is_initialized()`
  (on both crates) distinguishes that from a genuine denial.

- **Android: constructing a role before the Tauri plugin finished loading no
  longer reports `PermissionDenied`.** The Kotlin singletons only received a
  `Context` from `BlewPlugin.load()`, which runs after plugin setup returns —
  and a missing `Context` answers the permission check exactly as a denial
  does. `init_jvm` now hands them the application context itself, closing the
  window. An application that never registered the plugin at all gets the new
  `BlewError::NotInitialized` instead of a panic or a misleading denial.

- **Android: adapter state events are no longer duplicated after the host
  activity is recreated.** `BleCentralManager.init` / `BlePeripheralManager.init`
  ended in an unguarded `registerReceiver`, and `load()` runs again on every
  recreation — a rotation is enough — so registrations accumulated with
  nothing ever unregistering.

- **`init_jvm` no longer panics when called twice.** Aborting the process
  because a plugin setup ran twice was a sharp edge; the second call has
  nothing new to do.

- **tauri-plugin-blew: plugin setup can no longer hang forever at startup.**
  It blocked on an unbounded wait for wry to run a closure on the Android main
  thread. That is now bounded, so a main thread that never runs it fails setup
  with a message rather than hanging silently on a device.

- **tauri-plugin-blew: the host `Activity` is no longer leaked.** `BlewPlugin`
  held it in a static, keeping a destroyed instance alive across every
  recreation; it is a `WeakReference` now.

- **Apple: a peer closing an L2CAP channel cleanly is now observed.** The read
  loop only reported EOF via a zero-length read, which a clean close never
  produces — it leaves no readable bytes, so the loop never ran. The channel
  stayed registered and application reads hung indefinitely, with the idle
  backstop re-observing the same state forever. `NSStreamStatus::AtEnd` is now
  treated as end-of-stream.

- **Apple: a failed output stream no longer stalls writes permanently.** The
  write path stopped when `hasSpaceAvailable` was false without checking
  whether the stream had errored — and an errored stream never reports space
  again. Once the bounded buffers filled, every subsequent write waited
  forever. The stream status is now checked before treating "no space" as
  backpressure.

- **Apple: short `linger_timeout` values are honoured.** The reactor always
  slept against its one-second idle backstop, so a closing channel with a
  shorter deadline and a silent peer was torn down up to a second late. The
  sleep now shortens to the nearest active linger deadline.

- **Android: transport failures are no longer reported as clean closes.** The
  Kotlin side funnels deliberate closes, read failures, link loss and write
  failures through one callback, and the Rust side recorded `Closed` for all
  of them — contradicting the `L2capCloseReason` contract this release
  introduces. The callback now carries the failure message, and a close the
  read loop observes *after* the socket was deregistered is correctly reported
  as deliberate rather than as an error.

- **Android: a peripheral-only application can now use L2CAP.** The shared
  L2CAP state was initialised only from the central path, so
  `PeripheralConfig::l2cap` was silently ignored and `l2cap_listener()`
  panicked on uninitialised state. The peripheral initialises it too.

- **Android: `read_chunk_size` now sizes the actual socket reads.** It sized
  only the Rust-side bridge while Kotlin read a fixed 4 KiB, which also made
  the bounded-queue capacity describe a bound that wasn't the real one.

- **Android: L2CAP writes no longer block a Tokio worker.** The outbound task
  called Kotlin's `writeL2cap` — which lands on a blocking `BluetoothSocket`
  `OutputStream` — directly from an async task. Under the current-thread
  runtime the examples use, a slow peer stalled the entire runtime; under a
  multi-threaded one it burned a worker per writing channel. The call now goes
  through `spawn_blocking`. It is still awaited, which preserves both
  backpressure into the caller's `write()` and the lingering close's
  assumption that a finished outbound task means the bytes are on the socket.

- **Android: concurrent writes to one L2CAP socket can no longer interleave.**
  `L2capSocketManager.write` took no lock, so two writers could splice partial
  payloads into the same stream. Each socket now has its own monitor.

- **Android: blocking L2CAP loops moved off raw threads.** Channel connect,
  server accept, and per-socket reads each spawned an unmanaged `Thread`. They
  now run on a `Dispatchers.IO` scope. `BluetoothSocket` exposes no async API,
  so a blocking read per channel remains unavoidable — but it no longer costs
  an unmanaged thread apiece.

- **L2CAP no longer buffers without limit in either direction.** Every queue
  between the application and the platform socket on Apple and Android was
  unbounded: the Apple reactor's command channel and inbound channel, and
  Android's inbound channel. A peer faster than the application (or an
  application faster than the peer) grew memory without limit, per channel.
  All are now bounded from `L2capConfig`, and — the point of the exercise —
  the backends stop *reading* the socket when the queue is full. L2CAP CoC is
  credit-based, so an unread socket stops returning credits and the peer stops
  transmitting. The unbounded queues were converting the protocol's own flow
  control into local memory growth.

- **Apple: the L2CAP reactor is event-driven rather than a 20 Hz poll.** It
  woke every 50 ms and re-checked every channel, whether or not anything had
  happened, which cost idle CPU proportional to open channels and added up to
  50 ms of latency. Each channel's streams now carry an `NSStreamDelegate`, and
  the reactor is pulled out of its wait by `CFRunLoopWakeUp` when an
  application write queues bytes — an app `write()` produces no stream event,
  so it needs the explicit nudge. Only channels that signalled are serviced.
  The remaining timeout is a 1s backstop against a missed wakeup, not the
  service interval.

- **Apple: a busy L2CAP channel is no longer torn down instead of throttled.**
  The reactor called `write:maxLength:` without first checking
  `hasSpaceAvailable`, and treated the resulting non-positive return as a dead
  channel — so a channel that merely filled its transmit buffer was destroyed.
  The same call could instead block, stalling the single reactor thread and
  with it every other channel. Writes now happen only against reported space,
  partial writes are resumed, and only a real stream error closes the channel.

- **Linux: spontaneous disconnects are now reported.** `DeviceDisconnected`
  was only emitted on an explicit `disconnect()`, a connect timeout, or a
  BlueZ `DeviceRemoved` — and `DeviceRemoved` only arrives while a discovery
  session is running. Connecting, calling `stop_scan`, and then walking out
  of range produced no event at all, so applications driving reconnection
  from the event stream would wait forever. Each connection now gets a
  `Connected` property watcher, which bluer surfaces over D-Bus independently
  of discovery. Disconnect reporting is deduplicated across all three
  observers, so an explicit `disconnect()` on a device that was never
  connected through `Central::connect` no longer emits a spurious event.

- **Linux: starting a scan no longer deletes the user's bonded devices.**
  `start_scan` cleared BlueZ's stale device cache by calling
  `Adapter::remove_device` on every cached address that was not currently
  connected. That cache is shared host-wide and `remove_device` deletes the
  record outright — so a scan could drop the pairing keys and trust flag of
  the user's keyboard, headphones, or any other bonded peripheral that
  happened to be idle. Devices that are connected, paired, or trusted are
  now left alone, as is any device whose properties cannot be read. Stale
  advertisement data may therefore persist for bonded peers; unbonded peers,
  which is what peer-to-peer discovery actually cares about, still get a
  fresh cache.

- **Apple: notifications are no longer silently dropped under load.**
  `notify_characteristic` discarded the `BOOL` returned by
  `updateValue:forCharacteristic:onSubscribedCentrals:` and reported
  success unconditionally. A `NO` there means the transmit queue was full
  and the value was *not* sent, and the crate implemented no
  `peripheralManagerIsReadyToUpdateSubscribers:` delegate, so a refused
  notification was simply lost. Refused notifications are now queued and
  retried in FIFO order when CoreBluetooth signals readiness, and
  `notify_characteristic` resolves only once the value has actually been
  accepted — giving callers real backpressure instead of false success.

- **Apple: GATT operations no longer hang forever when the peer disconnects.**
  `centralManager:didDisconnectPeripheral:error:` now fails every request
  still pending on that device with
  `BlewError::DisconnectedDuringOperation`. CoreBluetooth delivers no
  completion callback for in-flight requests when a peer drops, and none of
  `discover_services` / `read_characteristic` / `write_characteristic` /
  `subscribe_characteristic` / `open_l2cap_channel` carries its own deadline
  (only `connect` does) — so a peer vanishing mid-operation previously left
  the caller awaiting a `oneshot` that nothing would ever send, and leaked
  the pending entry for the life of the process. Android already handled
  this via its per-operation queue timeout; Linux surfaces bluer's D-Bus
  errors.
- **Apple: long and batched ATT writes no longer lose all but their first
  fragment.** `peripheralManager:didReceiveWriteRequests:` took
  `requests[0]` and dropped the rest of the array. CoreBluetooth batches
  queued and prepared (long) writes into that array: exactly one ATT
  response is owed and it must name the first request, but *every* request
  carries its own slice of the payload. All fragments are now emitted as
  separate `PeripheralRequest::Write` events and the batch is acknowledged
  only once all of them have been answered.
- Refreshed `Cargo.lock`, pulling in `plist 1.10.0` / `quick-xml 0.41.0` and
  clearing RUSTSEC-2026-0194 and RUSTSEC-2026-0195, which were failing
  `cargo deny`. Both advisories reached the tree through `tauri`'s build-time
  `Info.plist` parsing and did not affect `blew` itself. `tauri` also moves
  2.11.0 → 2.11.5 and `tauri-build` / `tauri-plugin` 2.6.0 → 2.6.3.

---

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

## Upgrade guide — 0.3.x → 0.4.0

**If you were constructing `CentralConfig` or `PeripheralConfig` as a struct
literal naming every field**, both gain an `l2cap` field:

```rust
// Before
let config = CentralConfig {
    restore_identifier: None,
    connect_timeout: Some(Duration::from_secs(15)),
};

// After — spread the default, which also survives the next field we add
let config = CentralConfig {
    connect_timeout: Some(Duration::from_secs(15)),
    ..Default::default()
};
```

Tuning it is optional; the default matches the previous hardcoded behaviour:

```rust
use blew::{CentralConfig, L2capConfig};

let config = CentralConfig {
    l2cap: L2capConfig {
        buffer_size: 128 * 1024,
        ..Default::default()
    },
    ..Default::default()
};
```

**If you were matching on `PeripheralRequest::Write`**, it gains an `offset`
field. Bindings that already end in `..` need no change; exhaustive patterns
do:

```rust
// Before
PeripheralRequest::Write { client_id, char_uuid, value, responder, .. } => {
    store.insert(char_uuid, value);
    if let Some(r) = responder { r.success(); }
}

// After — splice at the offset instead of replacing the whole value
PeripheralRequest::Write { client_id, char_uuid, offset, value, responder, .. } => {
    let buf = store.entry(char_uuid).or_default();
    let start = offset as usize;
    if buf.len() < start + value.len() {
        buf.resize(start + value.len(), 0);
    }
    buf[start..start + value.len()].copy_from_slice(&value);
    if let Some(r) = responder { r.success(); }
}
```

`offset` is `0` for ordinary writes, so applications that never receive a
payload larger than `MTU - 3` can keep treating `value` as the whole value.

---

## Upgrade guide — Unreleased → next

**If you called `Peripheral::notify_characteristic`, pass a `NotifyKind`**:

```rust
// Before
peripheral.notify_characteristic(&device_id, char_uuid, value).await?;

// After — a notification (unchanged wire format)
peripheral
    .notify_characteristic(&device_id, char_uuid, NotifyKind::Notify, value)
    .await?;

// After — an indication; on Android the call resolves only once the peer
// has confirmed at the ATT layer
peripheral
    .notify_characteristic(&device_id, char_uuid, NotifyKind::Indicate, value)
    .await?;
```

`use blew::peripheral::NotifyKind;` (also re-exported from `blew`).

---

## [0.1.0]

Initial release.
