# blew — Project Guide for Codex

## What this is

A cross-platform BLE (Bluetooth Low Energy) library for Rust, providing both Central and Peripheral roles with platform backends for Apple (CoreBluetooth via objc2), Linux (BlueZ via bluer), and Android (JNI + Kotlin).

## Why this exists

`blew` was extracted from [iroh-ble-transport](https://github.com/mcginty/iroh-ble-transport) (local clone: `~/git/iroh-ble-transport`) — that project is the primary driver for the API shape, L2CAP focus, and cross-platform requirements. When in doubt about a design decision, check what `iroh-ble-transport` needs. Many of the quirks handled here (e.g., post-connect MTU stability, L2CAP ergonomics) came directly from issues encountered there.

## Project goals

- L2CAP is a first-class transport, not an edge feature.
- Designed to run with as many concurrent L2CAP channels as the device and OS will allow.
- Backend-owned transports with explicit close/shutdown. Prefer shared event-loop / reactor threads over per-channel worker threads: Apple `L2capReactor`, Linux's native `bluer::l2cap::Stream`, and Android's per-device `GattOperationQueue` coroutines all follow this. When a short-term fix is tempting, bias toward the reactor-shaped architecture anyway.
  - **Exception, and it is not fixable:** Android L2CAP blocks one coroutine per channel. `BluetoothSocket` exposes only blocking `InputStream`/`OutputStream` — there is no async socket API at any API level — so a blocking read per channel is forced by the platform. They run on a shared `Dispatchers.IO` scope rather than raw threads, and the Rust side reaches Kotlin's `writeL2cap` through `spawn_blocking` so no Tokio worker ever blocks on a socket. Don't call `writeL2cap` straight from an async task; under the current-thread runtime the examples use, that stalls everything.
- Every L2CAP queue is bounded, and that is load-bearing rather than tidiness. L2CAP CoC is credit-based: a socket we stop reading stops returning credits, and the peer stops transmitting. An unbounded queue converts the peer's own flow control into unbounded local memory growth. Sizes come from `L2capConfig`. **Do not** reintroduce an unbounded channel on an L2CAP data path.

## Commands

Uses [mise](https://mise.jdx.dev) for task management. Run `mise tasks` for the full list.

```sh
mise run build                           # build all crates
mise run test                            # run all tests (nextest)
mise run lint                            # clippy
mise run fmt                             # format
mise run fmt:check                       # check formatting
mise run deny                            # license/vulnerability audit
mise run ci:compile-kotlin               # compile the Android Kotlin sources
mise run ci:test-kotlin                  # JVM tests: GATT ownership + Rust/Kotlin JNI contract
cargo run --example scan -p blew         # scan for 10s
cargo run --example advertise -p blew    # advertise GATT service
```

## Style

- No comments unless the logic is non-obvious. Don't add doc comments to code you didn't change.
- Don't add features, refactor, or "improve" code beyond what was asked.
- Clippy pedantic is enabled (`pedantic = "warn"` in blew). Fix warnings, don't suppress them unless there's a good reason.
- Test with nextest: `cargo nextest run --workspace`.
- **Update `CHANGELOG.md` alongside user-visible changes.** New entries go under
  `## [Unreleased]` (creating that heading if missing) in the `Added` / `Changed` /
  `Removed` / `Fixed` buckets. Breaking changes must also include a before/after
  snippet in the upgrade guide at the bottom of the file. Pure-internal refactors
  that don't alter public API or observable behavior can skip the changelog.

## Key dependencies

| Crate | Role |
|-------|------|
| `objc2` / `objc2-core-bluetooth` | Apple backend (CoreBluetooth) |
| `bluer 0.17` | Linux backend (BlueZ D-Bus bindings) |
| `jni 0.22` | Android backend (JNI bridge) |
| `tokio 1` | Async runtime |

## Module structure

```
crates/blew/src/
├── lib.rs                        # pub use re-exports; top-level doc example
├── error.rs                      # BlewError (typed enum), BlewResult<T>
├── types.rs                      # DeviceId (Display + as_str()), BleDevice
├── testing.rs                    # In-memory mock backends (feature = "testing")
├── gatt/
│   ├── props.rs                  # CharacteristicProperties, AttributePermissions (bitflags)
│   └── service.rs                # GattService, GattCharacteristic, GattDescriptor
├── central/
│   ├── mod.rs                    # Central<B>  (default B = PlatformCentral)
│   ├── types.rs                  # CentralEvent, ScanFilter, WriteType
│   └── backend.rs                # CentralBackend sealed trait (RPITIT, no async_trait)
├── peripheral/
│   ├── mod.rs                    # Peripheral<B> (default B = PlatformPeripheral)
│   │                             #   + state_events() / take_requests() accessors
│   ├── types.rs                  # PeripheralStateEvent (Clone), PeripheralRequest (!Clone),
│   │                             #   ReadResponder, WriteResponder, AdvertisingConfig
│   └── backend.rs                # PeripheralBackend sealed trait
├── l2cap/
│   ├── mod.rs                    # L2capChannel (AsyncRead + AsyncWrite), DuplexBridge,
│   │                             #   CloseReasonSlot, flush-on-close
│   └── types.rs                  # Psm(u16), L2capConfig, L2capEncryption, L2capCloseReason
├── platform/
│   ├── mod.rs                    # #[cfg] type aliases: PlatformCentral, PlatformPeripheral
│   ├── apple/
│   │   ├── central.rs            # AppleCentral — full CoreBluetooth implementation
│   │   ├── peripheral.rs         # ApplePeripheral — full CoreBluetooth implementation
│   │   └── l2cap.rs              # L2capReactor — single-thread NSRunLoop for all channels
│   ├── linux/
│   │   ├── central.rs            # LinuxCentral — full bluer/BlueZ implementation
│   │   ├── peripheral.rs         # LinuxPeripheral — full bluer/BlueZ implementation
│   │   └── l2cap.rs              # bluer::l2cap::Stream → L2capChannel bridge
│   └── android/
│       ├── mod.rs                # Exports + init_jvm re-export
│       ├── jni_globals.rs        # OnceLock<JavaVM>, init_jvm(), jvm()
│       ├── central.rs            # AndroidCentral — JNI bridge to BleCentralManager.kt
│       ├── peripheral.rs         # AndroidPeripheral — JNI bridge to BlePeripheralManager.kt
│       ├── l2cap_state.rs        # Global L2CAP channel map + JNI data-path bridges
│       └── jni_hooks.rs          # #[unsafe(no_mangle)] extern "C" JNI callbacks
└── util/
    ├── event_stream.rs           # EventStream<T,S> + BroadcastEventStream<T> (lag-swallowing)
    └── request_map.rs            # RequestMap<V> / KeyedRequestMap<K,V>

crates/blew/android/                  # Co-located Kotlin/Gradle module for the Android backend
├── src/main/java/org/jakebot/blew/   # BleCentralManager.kt, BlePeripheralManager.kt,
│                                     #   GattOperationQueue.kt, L2capSocketManager.kt,
│                                     #   AdapterRename.kt, BlewPlugin.kt (Tauri entry point)
└── AndroidManifest.xml               # Runtime permission declarations (merged into host app)
```

## Public API pattern

```rust
// Both roles are independent; use only what you need.
let central: Central = Central::new().await?;   // explicit type required — see below
let peripheral: Peripheral = Peripheral::new().await?;

let mut events = central.events();   // returns an impl Stream of CentralEvent
use tokio_stream::StreamExt as _;
while let Some(ev) = events.next().await { /* ... */ }

// Peripheral events are split by kind:
let mut state = peripheral.state_events();                  // Clone, broadcast fan-out
let mut requests = peripheral.take_requests()                // single-consumer; None on 2nd call
    .expect("requests already taken");
```

**`let central: Central` is required.** Rust's default type-parameter inference does not kick in for method calls; without the explicit annotation the compiler fails with E0283. Same for `Peripheral`.

## Apple backend design (`platform/apple/`)

**Threading model:**
- Each manager (`CBCentralManager`, `CBPeripheralManager`) is initialized with a dedicated GCD serial queue via `initWithDelegate_queue(Some(&queue))`.
- All CB delegate callbacks fire exclusively on that queue.
- Tokio tasks call CB methods directly from the thread pool; CoreBluetooth is documented thread-safe on macOS 10.15+ / iOS 13+.
- Results flow back to Tokio via `tokio::sync::oneshot` channels (set in the delegate callback, awaited in the async method).

**Key patterns:**

```rust
// ObjcSend<T> — asserts Send+Sync for Retained<T> when using a GCD queue.
struct ObjcSend<T: objc2::Message>(Retained<T>);
unsafe impl<T: objc2::Message> Send for ObjcSend<T> {}
unsafe impl<T: objc2::Message> Sync for ObjcSend<T> {}

// retain_send — retain a CB object and wrap it for cross-thread use.
unsafe fn retain_send<T: objc2::Message>(obj: &T) -> ObjcSend<T> {
    ObjcSend(Retained::retain(obj as *const T as *mut T).expect("retain"))
}

// Avoid holding Retained<T> (non-Send) across .await points.
// Pattern: do all ObjC work in a synchronous block, capture only the rx end.
let rx = {
    let peripheral = ...get ObjcSend<CBPeripheral>...;
    let (tx, rx) = oneshot::channel();
    // ... ObjC calls ...
    rx
}; // peripheral drops here — before .await
rx.await...
```

**objc2 0.6 traits:**
- `use objc2::AnyThread` — provides `alloc()` for both user-defined classes (via `define_class!`) and external CB classes when initialized with a custom queue. (`AllocAnyThread` is a deprecated alias for the same trait.)
- `use objc2::DefinedClass` — provides `ivars()` inside `define_class!` method bodies.
- Both imports are required; missing either gives "no method found" errors.

**RAII responders:** `peripheralManager:didReceiveReadRequest:` and `didReceiveWriteRequests:` build a `ReadResponder`/`WriteResponder` (backed by an `oneshot::Sender`), emit a `PeripheralRequest` on the `mpsc::UnboundedSender` handed out by `take_requests()`, then spawn a task (via `inner.runtime.spawn()`) that awaits the oneshot and calls `respondToRequest:withResult:`. The spawn uses the captured `Handle` because GCD callbacks run outside the Tokio runtime context — bare `tokio::spawn` would panic. All Rust-side synchronization uses `parking_lot::Mutex` (poison-free, faster than `std::sync::Mutex`).

**A power-down is cleaned up before it is reported, to the depth CoreBluetooth
documents.** `peripheralManagerDidUpdateState:` runs `PeripheralInner::power_down`
before `AdapterStateChanged { powered: false }` goes out: every waiter fails with
`NotPowered`, the `l2cap_listener` accept stream ends, and each subscribed
central is reported unsubscribed. `CBPeripheralManager.h` separates two depths
and the cleanup follows them exactly: any state below `PoweredOn` pauses
advertising and disconnects every central, and only a state below `PoweredOff`
also clears the local database. **Don't clear `chars` on a plain power-off** —
CoreBluetooth keeps those services, and notifications on them would fail after
power-on. `chars` also fills only from `didAddService:` on success, so a service
CoreBluetooth rejected leaves nothing behind. `didUnsubscribeFromCharacteristic:`
reports only a central it actually removed, since `power_down` has already
reported the rest.

**A waiter that gave up still owns its slot.** `add_service`, `start_advertising`
and `l2cap_listener` wait on callbacks that identify their request by the
service UUID at most. Each registers in a `util::callback_slots::CallbackSlots`,
and the slot stays held after its caller times out, until CoreBluetooth's
answer arrives or a power-down frees every slot at once; a request that finds
its key held is refused. **Don't free a slot on timeout**: the late answer would
then confirm the next request under the same key. Attribution by object identity
(`didAddService:` passes a `CBService`) was not used because nothing here has
verified that CoreBluetooth returns the instance it was given — and if it
doesn't, every add would time out.

**Submission and cleanup share the manager's queue.** Each of those calls takes
one turn on `PeripheralHandle::queue` — the serial queue the delegate, and so
`power_down`, runs on — through `callback_slots::submit`, which checks
`PoweredOn`, registers, and issues the command with nothing in between, and
refuses through the same oneshot the async side awaits. Done from a Tokio
thread, as it was first written, a request could pass the check, lose its slot
to `power_down` (its caller already told `NotPowered`), and issue its command
after power returned: that answer completed a newer request under the same key,
and the service the first caller was told failed was added anyway. **Don't fix
this with a mutex** that `power_down` also takes, held across `addService:` /
`startAdvertising:` / `publishL2CAPChannelWithEncryption:`: calling into
CoreBluetooth while holding a lock the delegate queue needs deadlocks the moment
CoreBluetooth waits on that queue internally, and the slots' mutex is released
before the command for that reason. The turn owns its ObjC objects through
`ObjcSend`, so nothing `Retained` crosses the await; the timeout starts before
the turn is queued, so it bounds queueing too; and a turn whose caller already
gave up issues nothing, since carrying it out would contradict what that caller
was told. `CallbackSlots::register` is private so a registration can't be made
outside a turn.

**Every command that must order against a queued turn goes through the queue —
`stopAdvertising` included.** A start queues its turn and yields, so a stop
issued straight from the calling thread overtook a start still waiting there,
and advertising began after `stop_advertising` returned `Ok`.
`stop_advertising_in_turn` queues the stop and returns only once it has been
issued, after any start queued before it. It deliberately leaves a pending
start's slot alone: the start's own `didStartAdvertising:` still answers it,
and freeing the slot early would let that late answer reach a newer start. Both
go through the private `Advertiser` trait so the order is tested against a
fake queue. Calls that stay on the calling thread don't order against a turn:
`updateValue:` targets characteristics and centrals the delegate queue already
recorded, and serializes against `power_down` through `pending_notifies`;
`respondToRequest:withResult:` answers the specific `CBATTRequest` it was
handed; `isAdvertising` and `state` are reads, and the slot inside the turn,
not the pre-check, decides whether a start goes ahead.

Residual, unverified either way: freeing slots on power-down still assumes
CoreBluetooth never *answers* a request from before the power-down once the
adapter is back on and a new request holds the same key — without an identity
there would be nothing to check such an answer against.

**L2CAP reactor** (`platform/apple/l2cap.rs`): one dedicated OS thread owns an `NSRunLoop` and all `NSInputStream`/`NSOutputStream` objects. Channels register via `ReactorCmd::Register`, close via `ReactorCmd::Close`; there is no write command — each channel carries a bounded `outbound_rx` the reactor drains itself, so backpressure lands on the caller's `write()` instead of in a queue. Bytes flow Reactor→App through a bounded `mpsc::Sender<Vec<u8>>`, App→Reactor through a `tokio::io::duplex` + outbound bridge task. No per-channel threads. The loop is event-driven: each channel's streams carry an `NSStreamDelegate` that marks the channel in a shared `ReadySet`, and `pump_channels` services only marked channels plus any that are lingering. The 1s `acceptInputForMode:beforeDate:` timeout is a backstop against a missed wakeup, not the service interval.

**L2CAP reactor wakeup rules.** Two things give a channel work and only one of them is visible to the run loop, so both bridge tasks must mark *and* `wake_reactor()`:
- Peer traffic arrives as a stream event, which the delegate handles.
- An application `write()` only lands in a Tokio channel, so the outbound bridge has to nudge the reactor itself.
- Draining the inbound queue frees the capacity a paused `pump_input` is waiting on, and produces no event either: `hasBytesAvailable` is a level, not an edge, so the unread bytes that caused the pause generate no new notification. The inbound bridge marks after each delivery. Removing that stalls reads until the backstop.

**L2CAP close invariant:** exactly one `ReactorCmd::Close` per channel. `DuplexTransport::trigger_close` uses `Option::take` on the close hook so `.close().await` and `Drop` both route to the same single-fire path. **Do not** add defensive `ReactorCmd::Close` sends from the bridge tasks — the hook is the only sender. Closing is a *lingering* close: the hook does not tear the channel down, it asks the backend to stop once drained. Apple sets `closing_since` and keeps pumping output until the queue empties or `L2capConfig::linger_timeout` passes (`linger_finished`); Android lets its outbound task close the socket when it drains, with a delayed force-close as the backstop. `close()` and `Drop` therefore behave identically, which is deliberate — a transport that only kept queued data when you remembered to call `close()` would be a trap. Note that a closing Apple channel stops pumping *input*: its inbound queue is usually already dropped, which would otherwise read as a teardown reason and defeat the linger.

**L2CAP encryption invariant.** `L2capConfig::encryption` (`L2capEncryption`) is
read from `CentralConfig::l2cap` on open and `PeripheralConfig::l2cap` on
publish. Platforms are coarser than the three-level enum, so a backend picks the
nearest level it can express that is **never weaker** than requested, and returns
`BlewError::L2capEncryptionUnsupported` when there is none. **Do not** map an
unsupported level onto a weaker one to avoid the error path — silently
under-delivering a security guarantee is the bug this rule exists to prevent.
Rounding *up* is fine (Android's single secure socket serves `RequireEncryption`).
The default is `Insecure`, matching what every backend hardcoded before 0.4.0;
don't raise it without a major bump.

**The Android level lives on the backend instance, not in `l2cap_state`.** Every
other L2CAP setting there is process-global and last-writer-wins, which is
survivable for buffer sizes and not for a security level: `AndroidCentral::new()`
routes through `with_config(default)`, so a bare second `Central::new()` would
otherwise reset a first one that asked for `RequireEncryption` and silently open
it an insecure socket. `AndroidCentral`/`AndroidPeripheral` each own an
`l2cap_encryption` field and pass it to `l2cap_state::secure_flag`. **Don't move
it back into `L2capState`** — there is deliberately no global accessor for it.

The setting is meaningful on *both* paths and the two enforce it differently:
the listener refuses a `LE_CREDIT_BASED_CONNECTION_REQ` whose link doesn't meet
its requirement (the request PDU carries no security field), while the opener
elevates the ACL link first — which on LE only the Central can actuate. **Don't
"simplify" this by dropping the central-side knob**: Linux (`BT_SECURITY_*` on
the connecting socket) and Android (`createL2capChannel`) both implement it
correctly, and it's the only protection available when you don't control the
peer's PSM. Apple's central is refused because CoreBluetooth has no
raise-security API, which is an API gap specific to that backend — not a
protocol rule.

**L2CAP accept channel policy.** This governs the *accept* path only — the
stream of newly-arrived channels. Every L2CAP **data** path is bounded; see the
flow-control rule under Project goals.
- Apple: `mpsc::unbounded_channel()`. Blocking the GCD delegate queue on `blocking_send` would stall every subsequent CB callback (disconnects, restore, etc.).
- Android: `mpsc::unbounded_channel()`. `try_send` on a bounded channel would silently drop incoming L2CAP connections.
- Linux: `mpsc::channel(16)` + `send().await`. Backpressure flows into BlueZ's kernel-side socket accept queue, which is the right place to cap.

**L2CAP data path invariants (Apple + Android):**
- Reserve inbound capacity *before* reading the socket. A byte read out is a byte whose L2CAP credit has already been returned to the peer, so reading with nowhere to put it is exactly the unbounded-buffering bug.
- Apple: never call `write:maxLength:` without `hasSpaceAvailable`. Without the check it either blocks the shared reactor thread — stalling every other channel — or returns a non-positive value that reads like a dead channel. Only a real stream error (`streamStatus == Error`, or a negative return) closes a channel; "no space" means wait.
- Android: `on_channel_data` uses `blocking_send`, which is safe *only* because it runs on Kotlin's per-socket read thread rather than a Tokio worker. Blocking that thread is the intended backpressure.

## Event fan-out convention

All Central event streams and Peripheral `state_events()` streams use
`tokio::sync::broadcast::channel(256)` wrapped in `util::BroadcastEventStream`.
The wrapper silently drops `Lagged(n)` errors so slow subscribers miss events
but stay connected. Don't introduce new custom fan-out utilities — use
`broadcast::Sender` directly (it's `Clone + Sync`; no external `Mutex` needed).

Buffer depth is 256 across every role/backend; keep it uniform unless there's
a specific reason to diverge. Backend-emitted `send()` calls should be
`let _ = tx.send(event);` because broadcast returns `Err(SendError)` when
there are zero subscribers (normal at startup and for apps that don't need events).

## iOS state restoration (Apple-only surface)

`Central::take_restored()` and `Peripheral::take_restored()` are **not** on the
sealed `CentralBackend` / `PeripheralBackend` traits. They live as inherent
methods on Apple-only `impl` blocks:

```rust
#[cfg(target_vendor = "apple")]
impl Peripheral { pub fn take_restored(&self) -> Option<Vec<Uuid>> { … } }
```

Mocks get their own `impl Peripheral<MockPeripheral>` block in `testing.rs`.
Non-Apple backends have no stub method. Any code that calls `take_restored()`
on a shared cross-platform path must `#[cfg(target_vendor = "apple")]`-gate the
call. `restore_identifier` on `PeripheralConfig` / `CentralConfig` remains
cross-platform (inert on non-Apple) so `with_config` can be called unconditionally.

Android has no `willRestoreState:` equivalent. If background scan survival
becomes a requirement, the natural surface is a separate `PendingIntent`-backed
API (see Android's BLE background guide), **not** an extension of `take_restored`.

## CoreBluetooth rules that cause crashes

**Static value + Write property = `NSInvalidArgumentException` → SIGABRT.**
If `GattCharacteristic.value` is non-empty, CoreBluetooth treats the characteristic as static and throws if the characteristic also has the `Write` property. Use `value: vec![]` for any characteristic that needs to be writable; the app handles reads via `PeripheralRequest::Read`.

```rust
GattCharacteristic {
    properties: CharacteristicProperties::READ | CharacteristicProperties::WRITE,
    permissions: AttributePermissions::READ | AttributePermissions::WRITE,
    value: vec![],   // MUST be empty for writable characteristics
    ..
}
```

## Linux backend design (`platform/linux/`)

Uses `bluer 0.17` (official BlueZ Rust bindings over D-Bus).

**Threading model:** bluer is async-native (tokio). All calls go through the tokio runtime; no GCD queues or spawn_blocking needed. `Session::new().await` + `session.default_adapter().await` in `new()`.

**Key patterns:**

- **Scan**: `adapter.discover_devices().await?` returns `impl Stream<Item = AdapterEvent>`. Must be `Box::pin`-ned before iterating since the concrete type may not be `Unpin`. `AdapterEvent` has only `DeviceAdded(Address)` and `DeviceRemoved(Address)` variants.
- **CharacteristicFlags**: `ch.flags().await?` returns a `CharacteristicFlags` **struct with bool fields** (`.read`, `.write`, `.notify`, etc.), not an enum or `HashSet`. Access fields directly.
- **Write Command vs Write Request**: Remote `Characteristic` has `write()` (D-Bus, Write Request/response) and `write_io()` (kernel socket fd, Write Command/no-response). Use `write_io()` for `WriteType::WithoutResponse` to avoid D-Bus latency; `write()` for `WriteType::WithResponse`.
- **Notifications**: `ch.notify_io().await?` returns `CharacteristicReader: AsyncRead`. Spawn a task reading chunks.
- **GATT server callbacks**: `CharacteristicRead { fun: Box<dyn Fn(CharacteristicReadRequest) -> Pin<Box<dyn Future<Output = ReqResult<Vec<u8>>> + Send>>> }`. The future IS the ATT response — the server waits for it to complete. Bridge to `ReadResponder`/`WriteResponder` via `oneshot::channel`.
- **`CharacteristicNotifier`**: Received via `CharacteristicNotifyMethod::Fun` callback when a client subscribes. Store as `Arc<tokio::sync::Mutex<CharacteristicNotifier>>` (not std Mutex) so `notifier.notify(value).await` doesn't hold a MutexGuard across an await point. `notifier.notify(value: Vec<u8>)` takes ownership.
- **L2CAP server**: `bluer::l2cap::StreamListener::bind(SocketAddr::any_le())` gets dynamic PSM. `listener.as_ref().local_addr()?.psm` reads it back.
- **L2CAP client**: `device.address_type().await?` for `bluer::AddressType`, then `bluer::l2cap::Stream::connect(SocketAddr::new(addr, addr_type, psm))`. Add ~200ms delay after ACL connect before L2CAP CoC setup.
- **L2CAP bridging**: `bluer::l2cap::Stream` implements `AsyncRead + AsyncWrite` directly — just two `tokio::io::copy` tasks into a `tokio::io::duplex`. Much simpler than Apple.

**bluer API gotchas:**
- `Service`, `Characteristic`, `Application` do **not** derive `Default` — construct them field-by-field. `ServiceControlHandle::default()` and `CharacteristicControlHandle::default()` exist and are used for the `control_handle` fields.
- `CharacteristicWrite.write` = Write Request (with response); `CharacteristicWrite.write_without_response` = Write Command (no response). The doc comment on `write` saying "Write Command" is incorrect — trust `set_characteristic_flags` which maps directly to `CharacteristicFlags.write` = BlueZ "write" property = Write Request.
- `WriteOp::Request` in `CharacteristicWriteRequest.op_type` indicates a Write Request needing a response.

**Linux never reports `Delivery::Confirmed`, and never fails a value it handed
to BlueZ.** On an indicate-only characteristic (`indicate && !notify`) bluer's
`CharacteristicNotifier::notify` waits for a D-Bus `Confirm`, but that carries
no delivery information. BlueZ calls `Confirm` when an indication fails too:
on ATT timeout or disconnect, `src/shared/att.c` hands the indication callback
an error opcode, `conf_cb` in `src/shared/gatt-server.c` ignores the opcode,
and `conf_cb` in `src/gatt-database.c` calls `Confirm`. Confirmations from
every subscribed central also land in the same channel. The wait exists only
for backpressure, pacing sends to BlueZ's one indication in flight per bearer.
**Don't turn a successful `notify()` into `Confirmed`, and don't turn the wait
expiring or a session ending mid-wait into an error**; all of them are `Sent`.

**The indication wait has two phases, and one fixed bound cannot replace
them.** `paced_indication_wait` waits `INDICATION_PROBE` (1 s) first, and only
if that expires asks BlueZ whether any central is connected; no central means
nothing can answer the indication, so it stops waiting and reports `Sent`. A
connected one — or a connectivity query that *failed*, which must never
shorten the wait — gets the full `INDICATION_WAIT_BOUND` (35 s), past BlueZ's
30 s ATT transaction timeout, after which BlueZ answers the indication itself.

**Phase two races the query against the confirmation and the deadline; it must
never `await` the query on its own.** The query is D-Bus traffic bounded only by
bluer's 120 s timeout, so awaiting it in sequence holds the notifier lock far
past the 35 s this function advertises and swallows a confirmation that already
arrived (a confirmation ready at 2 s returned at 121 s). In the race the
confirmation or a stopped session finishes the call, the deadline returns
`Sent`, and the query only decides whether to keep waiting. Only the query may
be dropped mid-flight — the notification future stays pinned across the race,
so it emits once and a losing branch abandons a poll rather than the future.

Both halves are load-bearing. Waiting out 35 s unconditionally is the throttle
#41 reported: a bonded central keeps its subscription when it disconnects
(`att_disconnected` returns early for a bonded device) and BlueZ drops its
values without a `Confirm` (`send_notification_to_device` →
`state_set_pending`), so every send paid the bound. Capping the wait at a few
seconds instead breaks pacing, because the connection interval says nothing
about confirmation latency: a central confirming at 6 s would see the notifier
released, the next value emitted, and its own late `Confirm` end that next
wait, while BlueZ's unbounded `ind_queue` (`src/shared/att.c`, one
`pending_ind` per bearer) grows — the unbounded-queue failure mode this project
treats as load-bearing. **Don't collapse the two phases into one timeout**, and
keep the connectivity query off the fast path: it costs D-Bus round trips.

Residual, and not fixable from here: an absent bonded subscriber while some
*other* central is connected still costs the full 35 s. BlueZ's notify session
carries no device identity, so blew cannot tell which devices subscribed to the
characteristic.

## Android backend design (`platform/android/`)

Uses `jni 0.22` and `ndk-context 0.1`. The Android BLE API is Java/Kotlin-only, so the backend bridges Rust ↔ Kotlin via JNI.

**Architecture:**
- **Kotlin singletons** (`BleCentralManager`, `BlePeripheralManager`) live in `crates/blew/android/` (co-located with the Rust JNI hooks so they link-time version together). They wrap `BluetoothLeScanner`, `BluetoothGatt`, `BluetoothGattServer`, and `BluetoothLeAdvertiser`.
- **Rust → Kotlin**: `jvm().attach_current_thread()` then `call_static_method` on Kotlin object singletons.
- **Kotlin → Rust**: `@JvmStatic external fun` declarations in Kotlin, implemented as `#[unsafe(no_mangle)] extern "C"` in `jni_hooks.rs`.

**JNI contract tests.** cargo and Gradle never read each other's method tables,
so a mismatch across the boundary compiles and passes CI on both sides —
0.4.0-beta.2 shipped a missing `@JvmStatic` and aborted on launch. Two tests in
`ci:test-kotlin` derive the contract from the code instead of restating it:

- `JniContractTest` (Rust → Kotlin) parses every `call_static_method` site in
  the workspace and asserts each resolves to a *static* Kotlin method with that
  exact descriptor. The class at a site must be a single expression the test can
  resolve (`central_class()`, `socket_class(is_server)`, …) — a local binding
  fails the test, which is why `socket_class` exists — and the site must keep the
  `call_static_method(class, jni_str!("…"), jni_sig!("…"), …)` shape.
- `JniNativeContractTest` (Kotlin → Rust) reflects over every `external fun` in
  `org.jakebot.blew` and parses every `Java_*` export in the workspace, and
  requires the two sets to match by JNI symbol *and* descriptor. This direction
  matters more than it looks: a name mismatch is an `UnsatisfiedLinkError` only
  when that callback first fires, and a type mismatch raises nothing — the
  export reads arguments the JVM never passed. Hook parameters must use a type
  with exactly one JVM meaning (`jint`, `JString`, `JByteArray`, …); a bare
  `JObject` fails the test rather than passing unchecked.

Both cross-check their match count against a plain text count, so a
declaration reshaped past the parser fails loudly instead of going unchecked.

**Classloader gotcha:** Rust background threads use the system classloader, which cannot find APK classes. `init_jvm()` caches `GlobalRef`s to both Kotlin classes on the main thread (which has the app classloader). All JNI calls use `central_class()` / `peripheral_class()` from `jni_globals.rs` instead of string class names.

**Threading model:** Android BLE callbacks arrive on Binder threads. JNI hooks push events into tokio `mpsc` channels and return immediately. Rust async code awaits these channels. JNI `AttachGuard` is NOT `Send` — always drop it in a block before any `.await` point:

```rust
// Correct pattern — env drops before await:
{
    let mut env = jvm().attach_current_thread()?;
    env.call_static_method(...)?;
} // env dropped here
rx.await?; // safe to await now
```

**Global state:** Module-level `OnceLock` statics store event channels and pending operation maps. Only one Bluetooth adapter exists on Android so singletons are correct.

- `AndroidCentral`: uses `tokio::sync::broadcast` for events and generation-qualified pending GATT operations. `ConnectAttempts` retains identity through connected/disconnecting states. Register waiters and apply callback effects under its mutex, but **never hold that mutex across JNI**: Kotlin delivers callbacks while holding its lifecycle monitor.
- `GattConnections` owns each attempt's callback, handle, queue, nonces and MTU. Its monitor orders callbacks, operation kicks, retirement and delivery to Rust; `GattFactory.open` runs outside the monitor so an unpublished handle can be retired. The callback captures its attempt before factory entry. All lifecycle/GATT JNI requests and results carry a generation (except read-only `refresh`/`getMtu`). Disconnect fallback and cancellation target exact generations; there is no wildcard close. `ci:test-kotlin` exercises the production controller with a fake factory and virtual coroutine time.
- `AndroidPeripheral`: state events fan out through `tokio::sync::broadcast` (`PeripheralStateEvent` is `Clone`). GATT reads/writes are delivered as `PeripheralRequest` over an `mpsc::UnboundedSender`, handed out once via `take_requests()`. For each request, a tokio task awaits the responder's oneshot then calls Kotlin `respondToRead`/`respondToWrite` via JNI. All Rust-side synchronization uses `parking_lot::Mutex`.

**Notification gate invariant.** Android's GATT server takes one notification
per device until `onNotificationSent`, and that callback carries no id: it
belongs to whichever send is registered for the device when it arrives.
`util::notify_gate` therefore admits one send per device and holds the gate
until the callback or a disconnect clears the registration — **nothing else
frees it**. A send whose caller times out or is dropped stays registered as
abandoned, still holding the gate; its late callback is consumed and discarded.
**Don't "fix" a stuck device by releasing the gate on timeout**: the next send
would then register, and the stale callback would complete it with a false
`Sent`/`Confirmed`. **Don't add a generation through Kotlin either**: with no id
on `onNotificationSent`, Kotlin could only echo whichever send is current, which
is the same flaw. A device whose callback never comes fails later sends with a
timeout (the deadline covers waiting for the gate), not hangs and not overlaps.

**Local-name invariant.** `AdvertisingConfig::local_name` (`LocalName`) is a
permission, not just a value. Android has no per-advertisement name, so
`AllowPermanent` is the only variant that reaches `BluetoothAdapter.setName`,
and that rename is device-global and persistent. **Do not** make `Temporary`
fall back to renaming the adapter on Android to avoid
`BlewError::LocalNameUnsupported` — the point is that the device-wide mutation
is unreachable without asking for it.

blew deliberately does **not** restore the previous adapter name. A restore
can't be made correct from inside one app: an uninstall mid-advertisement, a
process that never relaunches, or two apps each saving the other's borrowed
name all defeat it. Restoring is left to the application; don't add a
best-effort restore back. What blew provides instead is the read and the write:
`Peripheral::adapter_name` / `set_adapter_name`, Android-only inherent methods
in the same style as `take_restored` (not on `PeripheralBackend`, mirrored on
the mock).

`AdapterRename.kt` does handle the race: a rename lands asynchronously and the
scan response snapshots whatever name is in place at start, so a named start
waits for `ACTION_LOCAL_NAME_CHANGED`. After one second it reads the name back
and **fails** the start (`ADVERTISE_FAILED_RENAME_UNCONFIRMED`) if the rename
hasn't landed — never advertise anyway, that puts the previous name on air.
One rename waits at a time. A second request while one waits — a named start
and `set_adapter_name`, or two sets — is **refused** (`Outcome.Busy`, surfacing
as `ADVERTISE_RENAME_BUSY` / `RENAME_BUSY`), not queued and not allowed to
replace the first: overwriting a waiter strands its caller until the Rust-side
timeout, and resolving overlaps in order is machinery for a case the app
creates itself. Only the last name asked for is tracked; a broadcast for any
other name is ignored, which can't put a wrong name on air because an
advertisement takes the name in place when it starts.
`onReady` runs under the class's monitor so `cancel` can't slip between
deciding to advertise and advertising; `onFailed` runs after the monitor is
released, because it takes `advertiseLock`, which callers hold while calling
in. Stop cancels the ticket **before** touching the advertiser; reversing the
order lets a waiting start begin an advertisement nothing can stop.
`ci:test-kotlin` exercises it with a fake adapter and virtual time.

**JNI data marshalling:** Complex data (GATT services, UUID lists) passed as flat arrays or JSON strings to avoid complex JNI type construction. Service characteristics use parallel arrays (uuids, properties, permissions, values).

**L2CAP:** Implemented via `L2capSocketManager.kt` — JNI hooks bridge `BluetoothServerSocket`/`BluetoothSocket` to Rust `L2capChannel` (AsyncRead + AsyncWrite). Global state in `l2cap_state.rs`.

**Auto MTU negotiation:** `BleCentralManager.kt` calls `requestMtu(512)` automatically after connecting.

**Connect reliability invariants (all three backends, Android load-bearing):**

- `CentralConfig::connect_timeout: Option<Duration>` bounds `connect()`. The
  `Default` is 15s. On elapse every backend returns `BlewError::ConnectTimedOut`
  and emits `CentralEvent::DeviceDisconnected { cause: Timeout }`. `None`
  restores pre-0.3 unbounded behavior. `BlewError::Timeout` is now reserved
  for adapter-readiness waits (`wait_ready` / `wait_powered`) and is **not**
  used by the connect path. Keep it that way — splitting the two lets callers
  match precisely.
- Overlapping `connect()` on the same device is rejected with
  `BlewError::ConnectInFlight(DeviceId)` on Apple and Android (built on
  `KeyedRequestMap::try_insert` on Apple and `ConnectAttempts` on Android) and on Linux (`CentralInner::pending_connects`).
  Do not reintroduce "latest wins" eviction — it silently orphans the first
  caller's oneshot.
- Android `disconnect()` **awaits** `onConnectionStateChange(DISCONNECTED)`
  with a 2s fallback. On fallback it calls Kotlin `forceClose(addr)` which
  synchronously removes the handle from `gattConnections`, invokes the hidden
  `refresh()`, and calls `gatt.close()`. This prevents leaking client-IF
  slots (Android caps at ~7). Do not revert to fire-and-forget disconnect.
- Status-133 zombie handling lives in Kotlin: on
  `onConnectionStateChange(DISCONNECTED, status=133)` the backend calls
  `refresh()` **before** `close()` to flush the client-side service cache.
  `BleCentralManager.connect()`'s stale-cleanup path does the same and then
  `delay(300)` before the next `connectGatt()` — back-to-back `connectGatt`
  attempts to the same address can be silently dropped by some vendor stacks.

## `crates/tauri-plugin-blew` — Tauri plugin for Android BLE setup

A thin Tauri 2 integration wrapper. The Kotlin BLE classes now live in `crates/blew/android/`; this crate just points Tauri's Gradle build at that path and initializes the JNI bridge.

**Rust side** (`src/lib.rs`): On Android, stores the JVM reference in blew's `OnceLock` via `blew::platform::android::init_jvm()`, then registers the Android plugin. `build.rs` computes the path to `../blew/android` and passes it to `tauri_plugin::Builder::android_path()`.

**Kotlin side** (lives at `crates/blew/android/src/main/java/org/jakebot/blew/`):
- `BlewPlugin.kt` — `@TauriPlugin`, initializes BLE managers, requests runtime permissions (BLUETOOTH_SCAN/CONNECT/ADVERTISE on Android 12+; ACCESS_FINE_LOCATION on pre-12 only). `BLUETOOTH_SCAN` is declared with `neverForLocation` so scan results are delivered regardless of the OS-level Location Services toggle.
- `BleCentralManager.kt` — Singleton wrapping scanner + GATT client. `ScanCallback` → `nativeOnDeviceDiscovered`. `BluetoothGattCallback` → `nativeOnConnectionStateChanged`, `nativeOnServicesDiscovered`, `nativeOnCharacteristicRead/Write/Changed`, `nativeOnMtuChanged`.
- `BlePeripheralManager.kt` — Singleton wrapping GATT server + advertiser. `BluetoothGattServerCallback` → `nativeOnReadRequest`, `nativeOnWriteRequest`, `nativeOnSubscriptionChanged`, `nativeOnConnectionStateChanged`, `nativeOnAdapterStateChanged`.

**AndroidManifest.xml:** Declares BLE permissions with `maxSdkVersion` guards. These merge into the app manifest automatically via Gradle.

**Usage in a Tauri app:**
```rust
// Cargo.toml: [target.'cfg(target_os = "android")'.dependencies]
// tauri-plugin-blew = { git = "https://github.com/mcginty/blew.git" }

#[allow(unused_mut)]
let mut builder = tauri::Builder::default();
#[cfg(target_os = "android")]
{ builder = builder.plugin(tauri_plugin_blew::init()); }
```

## Examples

```sh
cargo run --example scan -p blew          # scan for 10 s, print discoveries
cargo run --example advertise -p blew     # advertise a GATT service, handle reads/writes
cargo run --example l2cap_server -p blew  # peripheral: publish L2CAP CoC, echo data
cargo run --example l2cap_client -p blew  # central: scan, connect, open L2CAP, send data
```

All use `#[tokio::main(flavor = "current_thread")]` since the blew tokio dependency only enables the `rt` (not `rt-multi-thread`) feature.
