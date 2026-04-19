# Android setup (without Tauri)

If you're using Tauri, install [`tauri-plugin-blew`](https://crates.io/crates/tauri-plugin-blew)
and skip this section — it wires everything below automatically. For a bare
Android + Rust app, the Kotlin side and JNI bootstrap are your responsibility.
This is the minimal path.

## 1. Vendor the Kotlin runtime

The crate ships four Kotlin files at
`crates/blew/android/src/main/java/org/jakebot/blew/`. Copy them into your
Android app's source tree under the **same package** (`org.jakebot.blew`) —
the JNI hooks look up classes by that fully-qualified name:

- `BleCentralManager.kt`
- `BlePeripheralManager.kt`
- `GattOperationQueue.kt`
- `L2capSocketManager.kt`

(Skip `BlewPlugin.kt` — it's Tauri-specific.) These classes are pure
Android + `kotlinx.coroutines`, no Tauri dependency. Add
`implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:<current>")`
to your app's `build.gradle` if it isn't already there.

## 2. Merge the BLE permissions

Copy the `<uses-permission>` and `<uses-feature>` entries from
`crates/blew/android/src/main/AndroidManifest.xml` into your app's
`AndroidManifest.xml` (the manifest-merger will combine them if you make the
vendored Kotlin its own Gradle library module).

## 3. Initialize the JVM reference from Rust

Add a `JNI_OnLoad` entry point to your crate's `src/lib.rs`:

```rust
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(
    vm: jni::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    blew::platform::android::init_jvm(vm);
    jni::sys::JNI_VERSION_1_6
}
```

`init_jvm` caches global refs to the two manager classes using the activity's
classloader (system classloaders on Rust threads can't see APK classes).
Call it exactly once.

## 4. Initialize the Kotlin managers and request permissions

In your `Activity.onCreate` (or `Application.onCreate`):

```kotlin
System.loadLibrary("your_app")         // triggers Rust JNI_OnLoad
BleCentralManager.init(applicationContext)
BlePeripheralManager.init(applicationContext)

// Then trigger runtime permission requests via ActivityCompat.requestPermissions
// for BLUETOOTH_SCAN / CONNECT / ADVERTISE (API 31+) and ACCESS_FINE_LOCATION
// on older versions. blew won't start scanning/connecting until these are
// granted — Central::new / Peripheral::new return BlewError::PermissionDenied
// if they're missing.
```

From Rust you can check whether the app is ready:

```rust
use blew::platform::android::are_ble_permissions_granted;
if !are_ble_permissions_granted() { /* ask the user */ }
```

## 5. Cross-compile

Use [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (or equivalent) to build
the `.so` for each ABI you ship and drop the outputs in
`app/src/main/jniLibs/<abi>/`. `System.loadLibrary("your_app")` from step 4
then loads the shared library and fires `JNI_OnLoad`.
