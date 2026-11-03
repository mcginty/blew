# Android setup (without Tauri)

If you're using Tauri, install [`tauri-plugin-blew`](https://crates.io/crates/tauri-plugin-blew)
and skip this section — it wires everything below automatically. For a bare
Android + Rust app, the Kotlin side and JNI bootstrap are your responsibility.
This is the minimal path.

## 1. Vendor the Kotlin runtime

The crate ships its Kotlin at
`crates/blew/android/src/main/java/org/jakebot/blew/`. Copy every file there
except `BlewPlugin.kt` (it's Tauri-specific) into your Android app's source
tree under the **same package** (`org.jakebot.blew`) — the JNI hooks look up
classes by that fully-qualified name:

- `AdapterRename.kt`
- `BleCentralManager.kt`
- `BlePeripheralManager.kt`
- `GattConnections.kt`
- `GattOperationQueue.kt`
- `GattServerHost.kt`
- `L2capSocketManager.kt`
- `Subscription.kt`

These classes are pure Android + `kotlinx.coroutines`, no Tauri dependency. Add
`implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:<current>")`
to your app's `build.gradle` if it isn't already there.

## 2. Merge the BLE permissions

Copy the `<uses-permission>` and `<uses-feature>` entries from
`crates/blew/android/src/main/AndroidManifest.xml` into your app's
`AndroidManifest.xml` (the manifest-merger will combine them if you make the
vendored Kotlin its own Gradle library module).

## 3. Hand blew the JVM and a Context

`init_jvm` needs an Android `Context`, which `JNI_OnLoad` doesn't have, so call
it from a Kotlin `external fun` instead. Declare one anywhere in your app:

```kotlin
package com.example.app

import android.content.Context

object BlewBootstrap {
    @JvmStatic external fun init(context: Context)
}
```

and implement it in your crate's `src/lib.rs`:

```rust
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_app_BlewBootstrap_init<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: jni::objects::JClass<'local>,
    context: blew::platform::android::JContext<'local>,
) {
    env.with_env(|env| blew::platform::android::init_jvm(env, &context))
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}
```

`init_jvm` keeps the application context, caches global refs to the two
manager classes through its classloader (the system classloader on Rust
threads can't see APK classes), and hands the Kotlin managers their Context.
A failure is thrown back to Kotlin as a `RuntimeException`. Calling it again is
a no-op.

## 4. Initialize and request permissions

In your `Application.onCreate` (or `Activity.onCreate`):

```kotlin
System.loadLibrary("your_app")
BlewBootstrap.init(applicationContext)

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
then loads it.
