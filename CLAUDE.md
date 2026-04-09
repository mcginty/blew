# blew — Project Guide for Claude

## What this is

A cross-platform BLE (Bluetooth Low Energy) library for Rust, providing both Central and Peripheral roles with platform backends for Apple (CoreBluetooth via objc2), Linux (BlueZ via bluer), and Android (JNI + Kotlin).

## Commands

Uses [mise](https://mise.jdx.dev) for task management. Run `mise tasks` for the full list.

```sh
mise run build                           # build all crates
mise run test                            # run all tests (nextest)
mise run lint                            # clippy
mise run fmt                             # format
mise run fmt:check                       # check formatting
mise run deny                            # license/vulnerability audit
cargo run --example scan -p blew         # scan for 10s
cargo run --example advertise -p blew    # advertise GATT service
```

## Style

- No comments unless the logic is non-obvious. Don't add doc comments to code you didn't change.
- Don't add features, refactor, or "improve" code beyond what was asked.
- Clippy pedantic is enabled (`pedantic = "warn"` in blew). Fix warnings, don't suppress them unless there's a good reason.
- Test with nextest: `cargo nextest run --workspace`.

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
├── gatt/
│   ├── props.rs                  # CharacteristicProperties, AttributePermissions (bitflags)
│   └── service.rs                # GattService, GattCharacteristic, GattDescriptor
├── central/
│   ├── mod.rs                    # Central<B>  (default B = PlatformCentral)
│   │                             #   + CentralEvents<S> wrapper type
│   ├── types.rs                  # CentralEvent, ScanFilter, WriteType
│   └── backend.rs                # CentralBackend sealed trait (RPITIT, no async_trait)
├── peripheral/
│   ├── mod.rs                    # Peripheral<B> (default B = PlatformPeripheral)
│   │                             #   + PeripheralEvents<S> wrapper type
│   ├── types.rs                  # PeripheralEvent (!Clone), ReadResponder, WriteResponder,
│   │                             #   AdvertisingConfig
│   └── backend.rs                # PeripheralBackend sealed trait
├── l2cap/
│   ├── mod.rs                    # L2capChannel stub (AsyncRead + AsyncWrite)
│   └── types.rs                  # Psm(u16) newtype
├── platform/
│   ├── mod.rs                    # #[cfg] type aliases: PlatformCentral, PlatformPeripheral
│   ├── apple/
│   │   ├── central.rs            # AppleCentral — full CoreBluetooth implementation
│   │   └── peripheral.rs         # ApplePeripheral — full CoreBluetooth implementation
│   ├── linux/
│   │   ├── central.rs            # LinuxCentral — full bluer/BlueZ implementation
│   │   └── peripheral.rs         # LinuxPeripheral — full bluer/BlueZ implementation
│   └── android/
│       ├── mod.rs                # Exports + init_jvm re-export
│       ├── jni_globals.rs        # OnceLock<JavaVM>, init_jvm(), jvm()
│       ├── central.rs            # AndroidCentral — JNI bridge to BleCentralManager.kt
│       ├── peripheral.rs         # AndroidPeripheral — JNI bridge to BlePeripheralManager.kt
│       └── jni_hooks.rs          # #[unsafe(no_mangle)] extern "C" JNI callbacks
├── testing.rs                    # In-memory mock backends (feature = "testing")
└── util/
    ├── event_fanout.rs           # EventFanout<E: Clone> — mpsc fan-out for CentralEvent
    └── request_map.rs            # RequestMap<V> — thread-safe pending request/response coupling
```

## Public API pattern

```rust
// Both roles are independent; use only what you need.
let central: Central = Central::new().await?;   // explicit type required — see below
let peripheral: Peripheral = Peripheral::new().await?;

let mut events = central.events();   // returns CentralEvents<_> (impl Stream)
use tokio_stream::StreamExt as _;
while let Some(ev) = events.next().await { ... }
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

**`PeripheralEvent` is `!Clone`** (it carries RAII `ReadResponder`/`WriteResponder` handles). Therefore `EventFanout<PeripheralEvent>` cannot be used. Instead, `PeripheralInner` holds `Mutex<Option<mpsc::UnboundedSender<PeripheralEvent>>>`. Each call to `Peripheral::events()` replaces the sender, so only the most-recent subscriber receives events.

**RAII responders:** `peripheralManager:didReceiveReadRequest:` and `didReceiveWriteRequests:` build a `ReadResponder`/`WriteResponder` (backed by an `oneshot::Sender`), emit a `PeripheralEvent`, then spawn a task (via `inner.runtime.spawn()`) that awaits the oneshot and calls `respondToRequest:withResult:`. The spawn uses the captured `Handle` because GCD callbacks run outside the Tokio runtime context — bare `tokio::spawn` would panic.

## CoreBluetooth rules that cause crashes

**Static value + Write property = `NSInvalidArgumentException` → SIGABRT.**
If `GattCharacteristic.value` is non-empty, CoreBluetooth treats the characteristic as static and throws if the characteristic also has the `Write` property. Use `value: vec![]` for any characteristic that needs to be writable; the app handles reads via `PeripheralEvent::ReadRequest`.

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

## Android backend design (`platform/android/`)

Uses `jni 0.22` and `ndk-context 0.1`. The Android BLE API is Java/Kotlin-only, so the backend bridges Rust ↔ Kotlin via JNI.

**Architecture:**
- **Kotlin singletons** (`BleCentralManager`, `BlePeripheralManager`) live in `crates/tauri-plugin-blew/android/`. They wrap `BluetoothLeScanner`, `BluetoothGatt`, `BluetoothGattServer`, and `BluetoothLeAdvertiser`.
- **Rust → Kotlin**: `jvm().attach_current_thread()` then `call_static_method` on Kotlin object singletons.
- **Kotlin → Rust**: `@JvmStatic external fun` declarations in Kotlin, implemented as `#[unsafe(no_mangle)] extern "C"` in `jni_hooks.rs`.

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

- `AndroidCentral`: uses `EventFanout<CentralEvent>` (central events are `Clone`) + `RequestMap<oneshot::Sender>` for async request/response coupling.
- `AndroidPeripheral`: uses `Mutex<mpsc::UnboundedSender<PeripheralEvent>>` (peripheral events are `!Clone` due to RAII responders). For read/write requests, a tokio task is spawned per request that awaits the responder's oneshot then calls Kotlin `respondToRead`/`respondToWrite` via JNI.

**JNI data marshalling:** Complex data (GATT services, UUID lists) passed as flat arrays or JSON strings to avoid complex JNI type construction. Service characteristics use parallel arrays (uuids, properties, permissions, values).

**L2CAP:** Implemented via `L2capSocketManager.kt` — JNI hooks bridge `BluetoothServerSocket`/`BluetoothSocket` to Rust `L2capChannel` (AsyncRead + AsyncWrite). Global state in `l2cap_state.rs`.

**Auto MTU negotiation:** `BleCentralManager.kt` calls `requestMtu(512)` automatically after connecting.

## `crates/tauri-plugin-blew` — Tauri plugin for Android BLE setup

A lightweight Tauri 2 plugin that ships Kotlin BLE classes and initializes the JNI bridge.

**Rust side** (`src/lib.rs`): On Android, stores the JVM reference in blew's `OnceLock` via `blew::platform::android::init_jvm()`, then registers the Android plugin.

**Kotlin side** (`android/src/main/java/org/jakebot/blew/`):
- `BlewPlugin.kt` — `@TauriPlugin`, initializes BLE managers, requests runtime permissions (BLUETOOTH_SCAN, BLUETOOTH_CONNECT, BLUETOOTH_ADVERTISE on Android 12+; ACCESS_FINE_LOCATION on older).
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
