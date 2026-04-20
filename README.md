[![blew Crates.io](https://img.shields.io/crates/v/blew?label=blew)](https://crates.io/crates/blew)
[![tauri-plugin-blew Crates.io](https://img.shields.io/crates/v/tauri-plugin-blew?label=tauri-plugin-blew)](https://crates.io/crates/tauri-plugin-blew)
[![blew Docs.rs](https://img.shields.io/docsrs/blew?label=blew%20docs)](https://docs.rs/blew)
[![tauri-plugin-blew Docs.rs](https://img.shields.io/docsrs/tauri-plugin-blew?label=tauri-plugin-blew%20docs)](https://docs.rs/tauri-plugin-blew)

# `blew` (and `tauri-plugin-blew`)

🔥 **Warning:** 🔥 This library is experimental. A best effort will be made to resolve
bugs and follow semantic versioning, but there are no guarantees. Do not rely on it
until it has been sufficiently field-tested.

`blew` is a BLE (Bluetooth Low Energy) Rust library focused on enabling
peer-to-peer applications, and was built as the backend for
[`iroh-ble-transport`](https://github.com/mcginty/iroh-ble-transport).

It differs from other libraries in that it implements both Central *and*
Peripheral modes, and aims to provide support for macOS, iOS, Android, and
Linux. It is also `async`-only and requires a Tokio runtime.

It also supports opportunistic L2CAP which provides a lower-level socket-type
interface that tends to be significantly faster than using GATT for data
transfer.

`blew` is intended to support as many concurrent L2CAP channels as the device
and platform can sustain. Backend implementations should therefore optimize for
high L2CAP concurrency, not just single-channel correctness.

There is also an included `tauri-plugin-blew` that vastly simplifies the
necessary Kotlin/JNI glue to enable Android functionality.

## Supported Platforms

| Platform | Backend | Central | Peripheral | L2CAP |
|----------|---------|---------|------------|-------|
| macOS / iOS | CoreBluetooth (via `objc2`) | Yes | Yes | Yes |
| Linux | BlueZ (via `bluer`) | Yes | Yes | Yes |
| Android | JNI + Kotlin (via `jni` and `ndk-context`) | Yes | Yes | Yes |

## Documentation

- [Platform notes](docs/platform-notes.md)
- [Android setup (without Tauri)](docs/android-without-tauri.md)

## Examples

```sh
cargo run --example scan -p blew          # scan for 10s, print discoveries
cargo run --example advertise -p blew     # advertise a GATT service, handle reads/writes
cargo run --example l2cap_server -p blew  # peripheral: publish L2CAP CoC, echo data
cargo run --example l2cap_client -p blew  # central: scan, connect, open L2CAP, send data
cargo run --example restore -p blew       # iOS state-restoration launch sequence
```

## Testing on real hardware

Unit tests run against an in-memory mock backend and do not exercise a real
radio. For end-to-end coverage there's a two-process integration harness --
`integration_peripheral` advertises a known service + L2CAP listener, and
`integration_central` runs a scripted protocol against it and exits 0 on
success. Run the two binaries on two separate hosts (CoreBluetooth blocks
same-process loopback on Apple, and a single Linux adapter cannot scan for its
own advertisements either):

```sh
# host A
cargo run --example integration_peripheral -p blew

# host B
cargo run --example integration_central -p blew
```

The protocol covers scan + connect + service discovery, read of a fixed
status characteristic, a write-then-notify round-trip, and an L2CAP CoC
echo. On success the central prints `integration-central: PASS` and exits 0.

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
