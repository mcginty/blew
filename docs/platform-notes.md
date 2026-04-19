# Platform notes

Platform-specific behavior that is easy to miss when integrating `blew`.

- **macOS/iOS:** writable characteristics must be constructed with `value: vec![]` — otherwise
  CoreBluetooth raises `NSInvalidArgumentException` → `SIGABRT`. See `GattCharacteristic` docs.
- **Linux/BlueZ:** negotiated ATT MTU is not plumbed through the API yet; `Central::mtu` returns
  a conservative default of 247. Writes larger than 244 bytes may be rejected by peers with
  smaller negotiated MTUs.
- **Linux central → macOS/iOS peripheral pairing:** `blew` tries to avoid Apple pairing popups by
  marking the Linux adapter non-pairable, opening L2CAP CoC sockets with low security, and warning
  when `/etc/bluetooth/main.conf` still enables the BlueZ behaviors that commonly touch encrypted
  Apple services during discovery. That is still not enough to guarantee a zero-dialog flow:
  CoreBluetooth owns pairing UX on Apple platforms, and any encrypted GATT access can cause the OS
  to prompt on one or both sides. In practice, the best out-of-box setup today is:

  Why this happens:

  - Apple platforms may require bonding before allowing access to encrypted characteristics.
  - BlueZ can touch those characteristics during discovery even if your application never reads
    them directly. The usual culprits are the `battery` / `deviceinfo` plugins and GATT caching.
  - Once an encrypted read/write triggers security negotiation, CoreBluetooth decides whether to
    show a system pairing dialog. `blew` cannot suppress that UI from user space.

  What `blew` already does:

  - Linux central sets the adapter non-pairable to avoid unnecessary pairing attempts during
    normal GATT discovery.
  - Linux L2CAP CoC sockets are opened with low security so an app-level L2CAP transport does not
    request pairing by itself.
  - On startup, the Linux backend warns if the local BlueZ config is known to trigger Apple
    pairing popups during discovery.

  ```ini
  [General]
  DisablePlugins=battery,deviceinfo
  # Useful when re-pairing a device that one side still thinks is bonded:
  JustWorksRepairing=always

  [GATT]
  Cache=no
  ```

  Then restart BlueZ with `sudo systemctl restart bluetooth`.

  Additional notes:

  - If pairing state gets wedged, remove the bond on **both** systems before retrying.
  - If your protocol can tolerate it, keep the initial discovery/bootstrap path unencrypted and
    move authentication into your application protocol over GATT or L2CAP. That is currently the
    most reliable way to avoid OS-managed pairing UI across Linux and Apple devices.
  - If you require encrypted characteristics on the Apple side, assume a system pairing dialog may
    still appear. `blew` can reduce the frequency of those prompts, but it cannot suppress them.
  - Android can also bond for encrypted BLE access, but Android exposes more explicit bond
    management APIs than Apple does. The awkward "pairing dialog appears on both sides" behavior is
    mainly a Linux ↔ Apple concern.
- **Android:** BLE permissions (`BLUETOOTH_SCAN`/`CONNECT`/`ADVERTISE` on API 31+, plus
  `ACCESS_FINE_LOCATION` on older versions) must be granted at runtime — `Central::new` /
  `Peripheral::new` return `BlewError::PermissionDenied` if not. For non-Tauri apps,
  see [Android setup (without Tauri)](android-without-tauri.md) for the Kotlin + JNI bootstrap
  steps.
- **iOS state restoration:** opt in via `Central::with_config` / `Peripheral::with_config`
  with a stable `restore_identifier`. This *must* run synchronously from
  `application:didFinishLaunchingWithOptions:`, and `take_restored()` must be called
  immediately afterwards — before any new scans, connects, or `add_service` calls — to
  drain the payload the OS delivered during construction. L2CAP channels are never
  restored. The app's `Info.plist` must also declare the matching `UIBackgroundModes`:

  ```xml
  <key>UIBackgroundModes</key>
  <array>
      <string>bluetooth-central</string>
      <string>bluetooth-peripheral</string>
  </array>
  ```

  Include only the modes you actually use. See the crate-level `State restoration` rustdoc
  for the full contract — misusing it causes silent connection drops.
