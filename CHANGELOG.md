# Changelog

All notable changes to `blew` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **L2CAP channel encryption is configurable via `L2capConfig::encryption`.**
  ([#19](https://github.com/mcginty/blew/issues/19)) Every backend used to
  hardcode the weakest setting — `publishL2CAPChannelWithEncryption(false)` on
  Apple, `listenUsingInsecureL2capChannel()` / `createInsecureL2capChannel()` on
  Android, `BT_SECURITY_LOW` on Linux — with no way to ask for anything else.
  The new `L2capEncryption` enum (`Insecure`, `RequireEncryption`,
  `RequireAuthentication`) is read from `CentralConfig::l2cap` when opening a
  channel and from `PeripheralConfig::l2cap` when publishing a listener. The
  default stays `Insecure`, so nothing changes unless you set it.

  Platforms are far coarser than the enum, so a backend picks the nearest level
  it can express that is never *weaker* than what was requested, and returns the
  new `BlewError::L2capEncryptionUnsupported` when no such level exists rather
  than quietly handing back a less protected channel. Concretely: Linux maps the
  three levels onto `BT_SECURITY_LOW` / `MEDIUM` / `HIGH` exactly; Android has
  only insecure-vs-secure, so `RequireEncryption` gets the secure socket too
  (stronger than asked); Apple's peripheral role takes a single boolean and so
  refuses `RequireAuthentication`; and CoreBluetooth's `openL2CAPChannel:` has
  no security argument at all, so the Apple central role refuses anything but
  `Insecure`.

- **Android: `Peripheral::adapter_name` and `Peripheral::set_adapter_name`.**
  blew leaves the rename `LocalName::AllowPermanent` makes in place, and
  restoring the previous name is the application's call — but from Rust there
  was no way to read the adapter's name before advertising, or to write it
  back afterwards. These Android-only methods do both. `set_adapter_name`
  returns once the new name has taken effect, and fails if the stack refuses
  it or it hasn't landed within a second. One rename waits at a time: while
  one is waiting, a second `set_adapter_name` or a named `start_advertising`
  fails immediately instead of queueing. The `testing` mock mirrors both, and advertising under
  `AllowPermanent` renames its adapter as Android does.

### Changed

- **`AdvertisingConfig::local_name` is now a `LocalName`, defaulting to no
  name.** ([#29](https://github.com/mcginty/blew/issues/29),
  [#21](https://github.com/mcginty/blew/pull/21)) Every advertisement used to
  carry a name. On Android that meant renaming the device's Bluetooth adapter —
  `AdvertiseData` can only include the adapter's own name — which is
  device-global and persistent: it is the name the car, the headphones and every
  pairing dialog show. On every backend the name also competed with a 128-bit
  service UUID for the 31-byte legacy advertisement.

  `LocalName` makes the choice explicit, by how long the name may last:
  `None` (the default) advertises no name; `Temporary(name)` advertises one
  only inside the advertisement; `AllowPermanent(name)` also permits renaming
  the device where that is the only mechanism. Apple and Linux treat the two
  naming variants identically. Android refuses `Temporary` with the new
  `BlewError::LocalNameUnsupported` rather than quietly advertising no name,
  so the device-wide rename can't be reached without asking for it by name.
  The rename is left in place: blew doesn't restore the previous name, which
  can't be done reliably from inside one app, so that is up to the
  application. 0.4.0-beta.5 and beta.6 had an interim `Option<String>` here,
  where `Some` renamed the Android adapter.
- **`Peripheral::notify_characteristic` returns a `Delivery` instead of `()`.**
  ([#9](https://github.com/mcginty/blew/issues/9)) `Ok(())` meant something
  different on every backend — queued on Apple, handed to BlueZ on Linux, and on
  Android accepted by the stack without waiting for `onNotificationSent` — and
  there was no way to learn that a central had confirmed an indication.
  `Delivery::Confirmed` means the central's ATT layer acknowledged the value,
  `Delivery::Sent` means the platform took it with no acknowledgement, and
  `Delivery::NoSubscriber` means nothing was sent because the central isn't
  subscribed, which stays a success. Android now resolves only once
  `onNotificationSent` reports, so an indication to a central that subscribed
  for indications returns `Confirmed` once the central confirms it, and fails
  if the central disconnects or doesn't confirm within the ATT transaction
  timeout, rather than reporting success. Linux and Apple can only ever report
  `Sent`: CoreBluetooth has no indication confirmation, and BlueZ's D-Bus API
  reports a confirmed indication and a timed-out one the same way.

### Fixed

- **Apple: a Bluetooth power cycle no longer strands the peripheral's pending
  operations.** ([#45](https://github.com/mcginty/blew/issues/45))
  `peripheralManagerDidUpdateState:` only emitted `AdapterStateChanged`, and
  `add_service`, `start_advertising` and `l2cap_listener` each waited on a
  delegate callback with no timeout. Powering off while one was in flight left
  its caller waiting forever, and a queued `notify_characteristic` could do the
  same. Leaving `PoweredOn` now fails every pending call with
  `BlewError::NotPowered`, ends the `l2cap_listener` accept stream (with a
  `NotPowered` item) so its consumer knows to publish again, and reports each
  subscribed central as unsubscribed — CoreBluetooth disconnects them all —
  before `AdapterStateChanged { powered: false }` goes out. A central is
  reported once, whether its loss comes from the power cycle or from a later
  `didUnsubscribeFromCharacteristic:`.

  What a power cycle takes follows CoreBluetooth's own two cases. A plain
  power-off keeps the local database, so services stay registered and
  notifications keep working after power-on without re-adding anything. A
  state below `PoweredOff` — `Resetting`, `Unauthorized`, `Unsupported` —
  clears it, and blew then forgets the characteristics too; those services
  have to be added again.

  The three calls now also give up after five seconds
  (`BlewError::Peripheral`, or `BlewError::L2cap` for `l2cap_listener`)
  rather than waiting indefinitely, and refuse up front with `NotPowered`
  when the adapter is off — CoreBluetooth ignores a command issued then and
  never answers it. A call that gave up still holds its place until
  CoreBluetooth does answer, because that late answer would otherwise
  confirm the next request: meanwhile another `add_service` for the same
  service is refused with `BlewError::Peripheral`, and another
  `start_advertising` with `AlreadyAdvertising`. That also replaces the old
  behaviour for two overlapping calls, where the second silently displaced
  the first and woke it with `Internal("… channel dropped")`. Finally, a
  service's characteristics are only used for notifications once
  CoreBluetooth confirms the service, so one it rejected no longer leaves
  them behind.
- **Linux: advertising again after a Bluetooth power cycle works, and re-adding
  a service no longer serves it twice.**
  ([#46](https://github.com/mcginty/blew/issues/46)) Powering the adapter off
  takes the advertisement and GATT application down with it, but the
  peripheral kept its handles to both, and `start_advertising` refuses while
  it holds one: an application answering `AdapterStateChanged { powered: true }`
  by advertising again got `AlreadyAdvertising` for the life of the process,
  with nothing on air. The peripheral now drops both handles and its notify
  sessions when the adapter powers off, before it reports the event, so a
  handler reacting to it finds the peripheral ready to advertise. A
  `start_advertising` still waiting on BlueZ when the power goes fails with
  `BlewError::NotPowered` instead of keeping what it published.

  `add_service` now replaces a queued service with the same UUID, in place,
  instead of queueing a second copy. Linux's `add_service` never reaches
  BlueZ — the queue is served as one application on each `start_advertising`
  — and nothing removes from it, so an application that re-added its services
  after a power cycle, which Android requires, served every one of them twice,
  and once more per cycle. This holds whether or not the adapter cycled. The
  queue itself survives a power-off, so an application that doesn't re-add
  gets the same services back on its next `start_advertising`.
- **Linux: an indication to a central that walked away no longer stalls sends
  for 35 s.** ([#41](https://github.com/mcginty/blew/issues/41)) A bonded
  central keeps its subscription when it disconnects, and BlueZ drops values
  for it without ever reporting a confirmation, so `notify_characteristic` on
  an indicate-only characteristic waited out its full 35 s bound on every send
  and held the characteristic's other senders behind it. The wait now has two
  phases: after one second without a confirmation it asks BlueZ whether any
  central is connected, and returns as soon as the answer is no. A connected
  central still gets the full wait, so a subscriber that confirms slowly is
  awaited as before and only one indication is ever in flight. An absent
  subscriber alongside another connected central still costs the full wait:
  BlueZ doesn't say which centrals subscribed to a characteristic. Nothing else
  changes: the call still returns `Sent`, and the wait still says nothing about
  delivery.
- **Android: a central that subscribes for indications now gets indications.**
  ([#18](https://github.com/mcginty/blew/issues/18)) The peripheral reduced
  the central's CCCD write to "subscribed or not" and then sent every
  `notify_characteristic` value as a notification, so a central that enabled
  only indications received a notification it never asked for, and never got
  to confirm it. The peripheral now remembers which bit each central set per
  characteristic and sends an indication when only the indicate bit is set; a
  central that enables both gets notifications. A central that rewrites its
  CCCD while a send is waiting its turn gets the kind it asked for last. There
  is no API change.
- **Linux: a characteristic that declares `INDICATE` can be subscribed to.**
  ([#18](https://github.com/mcginty/blew/issues/18)) The peripheral registered
  a characteristic with BlueZ as notify-only whenever it declared `NOTIFY`, and
  not at all when it declared only `INDICATE`. BlueZ builds the CCCD from those
  flags and refuses a subscription for a kind that isn't registered, so a
  central could never enable indications. The characteristic now registers
  exactly the kinds it declares, and BlueZ sends each central the kind it
  subscribed for. On Linux `notify_characteristic` returns `Delivery::Sent`
  once the value is handed to BlueZ, never `Confirmed`, and never fails a
  value that went out. On an indicate-only characteristic it first waits for
  BlueZ to finish the indication, to pace sends to what BlueZ
  can deliver; BlueZ reports a confirmation and a timed-out indication the
  same way, so the wait says nothing about delivery. A failed D-Bus emit is an
  error. A concurrent send on the same characteristic also no longer forgets a
  subscriber whose session another send is holding, which a slow indication
  made easy to hit.
- **Android: notifying a busy device no longer fails after a few seconds, and
  a refused notification is no longer retried as busy.** Kotlin serialized sends
  per device with a semaphore released by `onNotificationSent`, and Rust polled
  it with fifty 5 ms retries. A device whose previous value was still in the
  stack for longer than that, or several tasks notifying one device at once,
  failed with "notification busy after retries". A value the stack refused
  outright was also reported as busy and retried to the same end. Rust now holds
  a per-device gate across the send and waits for the stack's callback, so
  concurrent sends queue instead of failing and a refusal fails at once. The
  gate is released only by the stack's report or a disconnect. A caller that
  times out (35 s, counting the wait for the gate) or is cancelled gets its
  error, but the gate stays held, so a send can never overlap one still in the
  stack or be completed by an earlier send's late callback. A device whose
  callback never arrives fails later sends with a timeout until it disconnects.
- **Android: a named advertisement no longer goes out under the previous
  name.** ([#21](https://github.com/mcginty/blew/pull/21), reported by
  @Resilum-owner) The adapter applies a rename asynchronously, but the scan
  response carries whichever name is in place when advertising starts.
  Renaming and advertising back to back therefore advertised the *old* name,
  and an old name too long for the 31-byte scan response — thirty Cyrillic
  characters is about fifty UTF-8 bytes — failed the advertisement with
  `ADVERTISE_FAILED_DATA_TOO_LARGE`. That struck whenever the adapter's name
  changed: the first session after install, or after the app picked a new name.
  Advertising now waits for `ACTION_LOCAL_NAME_CHANGED`; if the name hasn't
  taken effect within a second, `start_advertising` fails instead of
  advertising the previous name. A stop that lands during the wait cancels
  the start, so it can't begin an advertisement nothing is left to stop.
- **Linux: a discovered device's name, UUIDs, manufacturer data and service
  data are no longer read too early.** BlueZ announces a device the moment its
  first advertisement lands and fills the rest of the properties as later
  packets arrive, so a single read at `DeviceAdded` returned a snapshot with
  none of them: a peer advertising service data was reported with an empty
  `service_data` map, permanently, for as long as it kept advertising. The
  Linux central now watches each discovered device's properties and re-emits
  `DeviceDiscovered` when the advertised payload changes. RSSI updates the
  snapshot without an event, since it moves with every packet and says nothing
  new about the peer.
- **Linux: restarting a scan works, and no longer inherits the previous
  scan's service filter.** Two problems on the same path. The discovery filter
  was only sent when `ScanFilter` named at least one service, but bluer caches
  it per adapter and re-sends it on every `StartDiscovery`, so an unfiltered
  `start_scan` after a filtered one kept filtering on the old UUID list and
  silently reported nothing else; the filter is now always set, with an empty
  UUID list matching any device. Setting it also requires that no discovery
  session is live — bluer returns `DiscoveryActive` otherwise — so `start_scan`
  now tears down a running scan and waits for it to be dropped before
  reconfiguring, which previously made a filtered `start_scan` → `start_scan`
  fail outright. `stop_scan` waits for the same teardown, so a `stop_scan` →
  `start_scan` sequence cannot race it.
- **Android: connection ownership now spans the entire GATT lifecycle.**
  Each attempt owns its callback, client, operation queue, pending nonces and
  MTU. Callback effects and retirement are serialized, and generations qualify
  GATT requests/results and remain live in Rust through disconnect. Late callbacks
  and queued completions cannot affect a replacement connection. Dropping an
  unfinished connect or disconnect closes its exact client; pending GATT waiters
  are released on retirement. Early callbacks and cancellation before client
  publication are covered by deterministic fake-factory tests run in CI.

- **Android: a connect timeout leaked the GATT client it gave up on.**
  ([#24](https://github.com/mcginty/blew/issues/24)) `openGatt()` discarded the
  `BluetoothGatt` that `connectGatt()` returned, and the address-keyed handle
  map was populated only by the `STATE_CONNECTED` callback. A timeout before
  the connection completed therefore found nothing to close: it reported the
  disconnect and left the native client outstanding, where the stale-client
  cleanup on the next `connect()` could not reach it either. Repeated timeouts
  accumulated clients against Android's cap of roughly seven, after which
  connecting failed until the process restarted. Each attempt now owns its
  client from the moment `connectGatt()` returns, and a timeout closes that
  exact one.

- **Android: a retired connection attempt's late callback could tear down the
  attempt that replaced it.**
  ([#25](https://github.com/mcginty/blew/issues/25)) Every GATT callback
  identified its connection by device address alone. A superseded attempt's
  delayed `STATE_DISCONNECTED` removed the live attempt's handle, closed its
  operation queue, dropped its pending nonces and completed its `connect()` as
  disconnected; a delayed `STATE_CONNECTED`, or an MTU exchange finishing after
  the attempt it belonged to was retired, could satisfy a waiter that referred
  to a different GATT client. Attempts now carry a generation that travels to
  the platform with the connect request and returns on every callback.
  Each attempt gets its own `BluetoothGattCallback`, so identity is exact from
  the moment `connectGatt()` is called rather than from whenever its handle is
  published; every callback checks ownership under the same lock retirement
  takes before touching the per-device tables; state is cleared by
  compare-and-remove rather than by address; and a stale callback may release
  only its own client. A connection-state change is reported to Rust only by
  the attempt that owns the address when it is reported, so a superseded
  attempt can no longer emit a disconnect against its replacement.

- **Android: a `stop_advertising()` could leave the radio advertising with the
  Rust state machine saying `Idle`.**
  ([#26](https://github.com/mcginty/blew/issues/26)) A stop that took the
  advertising slot from a start still between its Rust registration and its JNI call
  reached Kotlin first and found nothing to stop, and the start then began
  advertising anyway. Its cleanup asked whether it still owned the slot, was
  told no — the stop had freed it — and skipped the teardown, leaving an
  advertisement running with nothing holding the request id needed to stop it.
  Cleanup is now keyed on the request id rather than on slot ownership. The
  matching hazard in the other direction is closed too: `stopAdvertising` takes
  the request id it means to stop, so an older stop can no longer tear down a
  newer start that claimed the advertiser after the slot was freed.

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

`L2capConfig` also carries `encryption`, which defaults to
`L2capEncryption::Insecure` — the level every backend hardcoded before 0.4.0.
Raising it makes the platform demand a secured link, and a backend that cannot
express the level you asked for fails the call with
`BlewError::L2capEncryptionUnsupported` rather than substituting a weaker one:

```rust
use blew::{L2capConfig, L2capEncryption, PeripheralConfig};

let config = PeripheralConfig {
    l2cap: L2capConfig {
        encryption: L2capEncryption::RequireEncryption,
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

**If you were setting `AdvertisingConfig::local_name`**, it is now a
`LocalName`:

```rust
use blew::peripheral::LocalName;

// Before (0.3.x)
let config = AdvertisingConfig {
    local_name: "my-device".into(),
    service_uuids: vec![SVC_UUID],
};

// Before (0.4.0-beta.5 and beta.6)
let config = AdvertisingConfig {
    local_name: Some("my-device".into()),
    service_uuids: vec![SVC_UUID],
};

// After — advertise no name, and identify the peripheral by its service UUID
let config = AdvertisingConfig {
    service_uuids: vec![SVC_UUID],
    ..Default::default()
};

// After — a name that lasts only as long as the advertisement
// (returns BlewError::LocalNameUnsupported on Android)
let config = AdvertisingConfig {
    local_name: LocalName::Temporary("my-device".into()),
    service_uuids: vec![SVC_UUID],
};

// After — the old behaviour, including renaming the Android adapter
let config = AdvertisingConfig {
    local_name: LocalName::AllowPermanent("my-device".into()),
    service_uuids: vec![SVC_UUID],
};
```

`AllowPermanent` is what a bare name always meant on Android: the adapter is
renamed and stays renamed, and blew doesn't restore the previous name. Prefer
`None` unless peers genuinely need the name. To put the name back yourself,
read it with `Peripheral::adapter_name` before advertising and write it with
`Peripheral::set_adapter_name` afterwards — checking first that the adapter
still carries your name, so one the user chose in the meantime survives.

**If you were using the result of `Peripheral::notify_characteristic`**, it is
now a `Delivery` rather than `()`. Code that only propagates or inspects the
error (`?`, `if let Err(e) = …`) compiles unchanged. Code that binds the value
needs to accept it:

```rust
use blew::peripheral::Delivery;

// Before
let () = peripheral.notify_characteristic(&client, CHAR_UUID, value).await?;

// After
match peripheral.notify_characteristic(&client, CHAR_UUID, value).await? {
    Delivery::Confirmed => { /* the central acknowledged the indication */ }
    Delivery::Sent => { /* handed to the stack, no acknowledgement */ }
    Delivery::NoSubscriber => { /* the central isn't subscribed; nothing sent */ }
}
```

A future you were spawning and awaiting as `JoinHandle<BlewResult<()>>` now
yields `BlewResult<Delivery>`.

---

## [0.1.0]

Initial release.
