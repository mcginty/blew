[![blew Crates.io](https://img.shields.io/crates/v/blew?label=blew)](https://crates.io/crates/blew)
[![tauri-plugin-blew Crates.io](https://img.shields.io/crates/v/tauri-plugin-blew?label=tauri-plugin-blew)](https://crates.io/crates/tauri-plugin-blew)
[![blew Docs.rs](https://img.shields.io/docsrs/blew?label=blew%20docs)](https://docs.rs/blew)
[![tauri-plugin-blew Docs.rs](https://img.shields.io/docsrs/tauri-plugin-blew?label=tauri-plugin-blew%20docs)](https://docs.rs/tauri-plugin-blew)

# `blew` (and `tauri-plugin-blew`)

🔥 Warning 🔥 This library is in alpha state and is subject to change without
backwards-compatibility until otherwise noted.

`blew` is a BLE (Bluetooth Low Energy) Rust library focused on enabling
peer-to-peer applications, and was built as the backend for
[`iroh-ble-transport`](https://github.com/mcginty/iroh-ble-transport).

It differs from other libraries in that it implements both Central *and*
Peripheral modes, and aims to provide support for macOS, iOS, Android, and
Linux. It is also `async`-only and requires a Tokio runtime.

It also supports opportunistic L2CAP which provides a lower-level socket-type
interface that tends to be significantly faster than using GATT for data
transfer.

There is also an included `tauri-plugin-blew` that vastly simplifies the
necessary Kotlin/JNI glue to enable Android functionality.

## Supported Platforms

| Platform | Backend | Central | Peripheral | L2CAP |
|----------|---------|---------|------------|-------|
| macOS / iOS | CoreBluetooth (via `objc2`) | Yes | Yes | Yes |
| Linux | BlueZ (via `bluer`) | Yes | Yes | Yes |
| Android | JNI + Kotlin (via `jni` and `ndk-context`) | Yes | Yes | Yes |

## Platform notes

- **macOS/iOS:** writable characteristics must be constructed with `value: vec![]` — otherwise
  CoreBluetooth raises `NSInvalidArgumentException` → `SIGABRT`. See `GattCharacteristic` docs.
- **Linux/BlueZ:** negotiated ATT MTU is not plumbed through the API yet; `Central::mtu` returns
  a conservative default of 247. Writes larger than 244 bytes may be rejected by peers with
  smaller negotiated MTUs.
- **Android:** BLE permissions (`BLUETOOTH_SCAN`/`CONNECT`/`ADVERTISE` on API 31+, plus
  `ACCESS_FINE_LOCATION` on older versions) must be granted at runtime — `Central::new` /
  `Peripheral::new` return `BlewError::PermissionDenied` if not.
- **Testing:** the `testing` feature exposes in-memory mock backends that enforce the
  connection/service/MTU invariants the real backends do. Real hardware behavior is not
  covered by CI; plan to smoke-test on device before shipping.

## Examples

```sh
cargo run --example scan -p blew          # scan for 10s, print discoveries
cargo run --example advertise -p blew     # advertise a GATT service, handle reads/writes
cargo run --example l2cap_server -p blew  # peripheral: publish L2CAP CoC, echo data
cargo run --example l2cap_client -p blew  # central: scan, connect, open L2CAP, send data
```

## Alternative Libraries

This library was customized primarily to be used for Iroh, and there are plenty
of great alternatives out there! I'm sure I'm missing some, but to name a few
that are actively maintained:

- [`btleplug`](https://github.com/deviceplug/btleplug)
- [`bluest`](https://github.com/alexmoon/bluest)
- [`ble-peripheral-rust`](https://github.com/rohitsangwan01/ble-peripheral-rust)

## License

This project is licensed under the [GNU Affero General Public License v3.0 or later](LICENSE).

Commercial licenses are available for use cases where the AGPL is not suitable.
Contact [me@jakebot.org](mailto:me@jakebot.org) for details.
