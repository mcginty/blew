# blew Architectural Overhaul — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address the critical findings from the 2026-04-16 deep review — co-locate Android sources, replace fragile concurrency primitives, harden the FFI and GATT layers, introduce a split peripheral-event API, and migrate load-bearing docs out of CLAUDE.md.

**Architecture:** Ten phases, each independently mergeable and shippable. Phase 0 is a mechanical warm-up. Phases 1–7 address specific platform/backend bugs. Phase 8 is the one breaking public-API change (`PeripheralEvent` split). Phases 9–10 tighten tests and docs. Stop at any phase boundary and the tree should compile + `mise run ci` should pass.

**Tech Stack:** Rust 2024 edition, tokio 1, parking_lot 0.12, objc2 0.6, bluer 0.17, jni 0.22, Kotlin coroutines, Tauri 2.

**User preference:** I (the user) am the only consumer of this crate right now, so breaking API changes are fine. Prefer the best-possible end-state design over backwards-compat shims.

---

## Phase 0 — Foundations

Mechanical warm-up: doc note, parking_lot swap. Low risk.

### Task 0.1: Document the iroh-ble-transport origin in CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add origin note near the top of CLAUDE.md**

Insert after the existing "## What this is" section:

```markdown
## Why this exists

`blew` was extracted from [iroh-ble-transport](https://github.com/mcginty/iroh-ble-transport) (local clone: `~/git/iroh-ble-transport`) — that project is the primary driver for the API shape, L2CAP focus, and cross-platform requirements. When in doubt about a design decision, check what `iroh-ble-transport` needs. Many of the quirks handled here (e.g., post-connect MTU stability, L2CAP ergonomics) came directly from issues encountered there.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude): note iroh-ble-transport as the driving consumer"
```

### Task 0.2: Add parking_lot as a direct workspace dependency

**Files:**
- Modify: `crates/blew/Cargo.toml`

Currently `parking_lot` is pulled in transitively. Make it direct so `Mutex` imports are unambiguous.

- [ ] **Step 1: Add to `[dependencies]`**

Add to `crates/blew/Cargo.toml` after the existing deps:

```toml
parking_lot = "0.12"
```

- [ ] **Step 2: Verify it resolves**

Run: `cargo check -p blew`
Expected: builds without error.

- [ ] **Step 3: Commit**

```bash
git add crates/blew/Cargo.toml Cargo.lock
git commit -m "deps(blew): add direct parking_lot dependency"
```

### Task 0.3: Swap `std::sync::Mutex` for `parking_lot::Mutex` in util

**Files:**
- Modify: `crates/blew/src/util/event_fanout.rs`

`util/request_map.rs` already uses parking_lot; `util/event_fanout.rs` does not. Unify.

- [ ] **Step 1: Update `event_fanout.rs` imports and types**

Change line 1:
```rust
use std::sync::{Arc, Mutex};
```
to:
```rust
use parking_lot::Mutex;
use std::sync::Arc;
```

Remove the `.unwrap()` calls on `.lock()` (parking_lot doesn't poison):
- Line 27: `self.subscribers.lock().unwrap().push(tx);` → `self.subscribers.lock().push(tx);`
- Line 41–42: `let mut subs = self.subscribers.lock().unwrap();` → `let mut subs = self.subscribers.lock();`

- [ ] **Step 2: Run tests**

Run: `cargo nextest run -p blew util::event_fanout`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/blew/src/util/event_fanout.rs
git commit -m "refactor(util): use parking_lot::Mutex in event_fanout"
```

### Task 0.4: Sweep `std::sync::Mutex` → `parking_lot::Mutex` across all platform code

Scope (from `grep -l 'std::sync::{.*Mutex'` and equivalent): `testing.rs`, `platform/linux/central.rs`, `platform/linux/peripheral.rs`, `platform/apple/central.rs`, `platform/apple/peripheral.rs`, `platform/apple/l2cap.rs`, `platform/android/central.rs`, `platform/android/peripheral.rs`, `platform/android/l2cap_state.rs`, `platform/android/jni_globals.rs`.

Leave `tokio::sync::Mutex` alone — those are intentional for holding locks across `.await`.

- [ ] **Step 1: Mechanical replacement, one file at a time**

For each file:
1. Replace `use std::sync::{… Mutex …}` with `use parking_lot::Mutex;` plus `use std::sync::{the rest};`.
2. Replace every `.lock().unwrap()` with `.lock()`.
3. Replace every `.lock().expect(...)` with `.lock()`.
4. If any call site needs the lock as `LockResult`, that means the surrounding code was defensively coping with poisoning — remove the defensive branches.

Do **not** touch `tokio::sync::Mutex` (`*.lock().await` pattern). Do **not** touch `std::sync::RwLock` if present (separate change).

- [ ] **Step 2: Confirm no `.lock().unwrap()` left on parking_lot types**

Run: `rg '\.lock\(\)\.unwrap\(\)' crates/blew/src/`
Expected: only matches inside commented-out code or on `std::sync::Mutex` holdouts (there shouldn't be any by the end).

- [ ] **Step 3: Run workspace build + tests**

Run: `mise run test`
Expected: pass.

- [ ] **Step 4: Run clippy**

Run: `mise run lint`
Expected: pass. Pay attention to any new `needless_pass_by_value` or `significant_drop_tightening` warnings — parking_lot guards have stricter Drop semantics in clippy's eyes.

- [ ] **Step 5: Commit**

```bash
git add crates/blew/src/
git commit -m "refactor: migrate std::sync::Mutex to parking_lot across platform code"
```

---

## Phase 1 — Co-locate Android Kotlin into `blew/android/`

The Kotlin glue and the Rust `#[no_mangle]` JNI hooks are link-time coupled. They belong in the same crate so they version together. `tauri-plugin-blew` shrinks to a thin Tauri integration layer.

### Task 1.1: Move the Kotlin module into `crates/blew/android/`

**Files:**
- Move (git mv): `crates/tauri-plugin-blew/android/` → `crates/blew/android/`

- [ ] **Step 1: Move the tree**

```bash
git mv crates/tauri-plugin-blew/android crates/blew/android
```

- [ ] **Step 2: Update blew's `Cargo.toml` to include the android directory on publish**

Add to `crates/blew/Cargo.toml` under `[package]`:

```toml
include = [
    "src/**/*.rs",
    "Cargo.toml",
    "LICENSE",
    "README.md",
    "android/**/*",
]
```

(If there's already an `include` or `exclude` field, reconcile. Without `include`, cargo ships everything; with it, we need to be explicit.)

- [ ] **Step 3: Commit the move**

```bash
git add crates/blew/Cargo.toml crates/blew/android crates/tauri-plugin-blew/
git commit -m "refactor: move Android Kotlin source from tauri-plugin-blew to blew"
```

### Task 1.2: Rewire `tauri-plugin-blew/build.rs` and Gradle to reference blew's android directory

The `android_path("android")` call currently points to the local `android/` that just moved. We need the plugin to either (a) reference `../blew/android`, or (b) generate/symlink at build time.

Simplest reliable approach: have `tauri-plugin-blew/build.rs` compute the blew android path and pass it.

**Files:**
- Modify: `crates/tauri-plugin-blew/build.rs`

- [ ] **Step 1: Update build.rs to point at blew's android dir**

```rust
fn main() {
    let blew_android = {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        std::path::PathBuf::from(manifest_dir)
            .join("..")
            .join("blew")
            .join("android")
    };
    let blew_android = blew_android.canonicalize().unwrap_or(blew_android);

    tauri_plugin::Builder::new(&[])
        .android_path(blew_android.to_string_lossy().as_ref())
        .build();

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let mobile = target_os == "ios" || target_os == "android";
    println!("cargo:rustc-check-cfg=cfg(desktop)");
    println!("cargo:rustc-check-cfg=cfg(mobile)");
    if mobile {
        println!("cargo:rustc-cfg=mobile");
    } else {
        println!("cargo:rustc-cfg=desktop");
    }
}
```

Caveat: `tauri_plugin::Builder::android_path` may reject absolute paths or paths outside `CARGO_MANIFEST_DIR`. If it does, fall back to approach (b): a `build.rs` step that copies `../blew/android` to `target/tauri-plugin-blew-android/` and points at that. Verify with an actual Android build before committing.

- [ ] **Step 2: Try an Android check build**

Run: `mise run ci:check-android`
Expected: succeeds; Tauri's build system picks up Kotlin from the blew crate's android/ directory.

If it fails with "android_path must be inside CARGO_MANIFEST_DIR" or similar, switch to the copy-at-build-time approach:

```rust
let out_dir = std::env::var("OUT_DIR").unwrap();
let staged = std::path::PathBuf::from(&out_dir).join("android");
std::fs::create_dir_all(&staged).unwrap();
// recursive copy from blew_android to staged …
tauri_plugin::Builder::new(&[]).android_path(staged.to_string_lossy().as_ref()).build();
```

Either way, end state: Kotlin lives in `crates/blew/android/`, tauri-plugin-blew surfaces it to Tauri.

- [ ] **Step 3: Fix the tauri-plugin-blew version pin on blew**

`crates/tauri-plugin-blew/Cargo.toml` line 21 currently pins `blew = "0.1.0-alpha.2"`. Change to follow the local workspace:

```toml
blew = { path = "../blew", version = "0.1.0" }
```

- [ ] **Step 4: Commit**

```bash
git add crates/tauri-plugin-blew/
git commit -m "build(tauri-plugin-blew): reference blew's android tree, fix version pin"
```

### Task 1.3: Update CLAUDE.md and READMEs to reflect new layout

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Modify: `crates/blew/README.md`

- [ ] **Step 1: Update CLAUDE.md layout diagram**

In CLAUDE.md, replace the `crates/tauri-plugin-blew` section's description of where Kotlin lives. The `android/` tree now lives under `crates/blew/android/`. `tauri-plugin-blew` is now a thin Tauri-integration wrapper only.

- [ ] **Step 2: Cross-check that docs reference correct paths**

Run: `rg 'tauri-plugin-blew/android' .`
Expected: no matches outside of git history / Cargo.lock.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md crates/blew/README.md
git commit -m "docs: update layout references for co-located Android source"
```

---

## Phase 2 — Android per-device Operation Queue (Kotlin side)

Replace `Semaphore(1)` with a per-device operation queue using Kotlin coroutines. Gives us per-op timeout and cancel-on-disconnect.

### Task 2.1: Design the `GattOperationQueue` abstraction

**Files:**
- Create: `crates/blew/android/src/main/java/org/jakebot/blew/GattOperationQueue.kt`

- [ ] **Step 1: Write the queue**

```kotlin
package org.jakebot.blew

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Per-device GATT operation queue.
 *
 * Android's framework already serializes GATT operations per BluetoothGatt instance
 * (mDeviceBusy). This queue adds:
 *  - per-operation timeout (guards against OEM firmware bugs where callbacks never fire),
 *  - cancel-on-disconnect (on close() all pending ops fail with a single stable error),
 *  - FIFO guarantee.
 *
 * Each device gets its own instance; a single worker coroutine drains the queue serially.
 */
class GattOperationQueue(tag: String) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val channel = Channel<Task<*>>(capacity = Channel.UNLIMITED)
    private val worker: Job

    init {
        worker = scope.launch {
            for (task in channel) {
                task.run()
            }
        }
    }

    /**
     * Enqueue a GATT operation.
     *
     * `kick` should start the operation (e.g., gatt.writeCharacteristic(...)) and
     * return true on success. If `kick` returns false, the task fails with
     * IllegalStateException("kick returned false").
     *
     * `timeoutMs` applies from the moment `kick` runs to the moment `complete()`
     * is called via completeCurrent().
     */
    suspend fun <T> enqueue(
        name: String,
        timeoutMs: Long,
        kick: () -> Boolean,
        onComplete: (Task<T>) -> Unit = {},
    ): Result<T> {
        val task = Task<T>(name, timeoutMs, kick, onComplete)
        channel.send(task)
        return task.await()
    }

    /**
     * Called from the GATT callback to hand a result to whichever task is currently running.
     *
     * If no task is running (spurious callback), this is a no-op.
     */
    @Suppress("UNCHECKED_CAST")
    fun <T> completeCurrent(value: T) {
        val task = current ?: return
        (task as Task<T>).complete(value)
    }

    @Volatile private var current: Task<*>? = null

    fun close(reason: Throwable = CancellationException("queue closed")) {
        channel.close()
        scope.launch {
            current?.fail(reason)
            while (true) {
                val next = channel.tryReceive().getOrNull() ?: break
                next.fail(reason)
            }
            scope.cancel()
        }
    }

    inner class Task<T>(
        val name: String,
        private val timeoutMs: Long,
        private val kick: () -> Boolean,
        private val onComplete: (Task<T>) -> Unit,
    ) {
        private val deferred = CompletableDeferred<Result<T>>()

        suspend fun run() {
            current = this
            try {
                if (!kick()) {
                    deferred.complete(Result.failure(IllegalStateException("kick returned false for $name")))
                    return
                }
                val result = withTimeoutOrNull(timeoutMs) { deferred.await() }
                if (result == null) {
                    deferred.complete(Result.failure(RuntimeException("$name timed out after ${timeoutMs}ms")))
                }
            } finally {
                current = null
                onComplete(this)
            }
        }

        fun complete(value: T) { deferred.complete(Result.success(value)) }
        fun fail(t: Throwable) { deferred.complete(Result.failure(t)) }
        suspend fun await(): Result<T> = deferred.await()
    }
}
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/android/src/main/java/org/jakebot/blew/GattOperationQueue.kt
git commit -m "feat(android): add per-device GATT operation queue"
```

### Task 2.2: Replace the global Semaphore with per-device queues in BleCentralManager

**Files:**
- Modify: `crates/blew/android/src/main/java/org/jakebot/blew/BleCentralManager.kt`

- [ ] **Step 1: Replace the Semaphore fields with a per-device queue map**

Find the block around line 63-72 currently defining:
```kotlin
private val gattSemaphore = Semaphore(1)
private val mtuHoldDevices: MutableSet<String> = ConcurrentHashMap.newKeySet()
```

Replace with:
```kotlin
private val gattQueues: MutableMap<String, GattOperationQueue> = ConcurrentHashMap()

private fun queueFor(address: String): GattOperationQueue =
    gattQueues.getOrPut(address) { GattOperationQueue("gatt-$address") }
```

Remove the `acquireGatt`/`releaseGatt` helper functions (~lines 191-195 region).

- [ ] **Step 2: Rewrite each GATT-operation call site**

For every operation currently pattern `if (!acquireGatt()) { fail }; ...; releaseGatt()`, rewrite as:

```kotlin
// read
val result = queueFor(addr).enqueue<ByteArray>(
    name = "read-$charUuid",
    timeoutMs = 5000L,
    kick = { gatt.readCharacteristic(characteristic) },
)
```

and in `onCharacteristicRead`:
```kotlin
queueFor(gatt.device.address).completeCurrent<ByteArray>(characteristic.value)
```

Apply the same pattern to: writeCharacteristic, setCharacteristicNotification (which also writes CCCD), discoverServices, requestMtu.

For write-without-response (op_type doesn't solicit a callback), complete the task immediately after kick succeeds — or bypass the queue entirely for that type; see the existing logic that handles this.

- [ ] **Step 3: MTU-hold handling on connect**

The current logic drains permits, calls `requestMtu(512)`, waits for `onMtuChanged`. With per-device queues, this becomes:

```kotlin
// in onConnectionStateChange CONNECTED branch:
scope.launch {
    val mtuResult = queueFor(addr).enqueue<Int>(
        name = "request-mtu",
        timeoutMs = 5000L,
        kick = { gatt.requestMtu(512) },
    )
    // regardless of result (failure just means we stick with 23), fire connected event
    nativeOnConnectionStateChanged(addr, true, 0)
}
// in onMtuChanged:
queueFor(gatt.device.address).completeCurrent<Int>(mtu)
mtuMap[gatt.device.address] = mtu
```

No more global drain. If MTU negotiation hangs, only that device's queue waits; other devices are unaffected. After timeout, the queue continues with subsequent operations — they'll just operate at the default MTU of 23.

- [ ] **Step 4: Cleanup on disconnect**

In `onConnectionStateChange` DISCONNECTED branch:

```kotlin
gattQueues.remove(addr)?.close(
    CancellationException("device $addr disconnected")
)
```

This fails any pending or in-flight operations with a stable error and releases the queue's resources.

- [ ] **Step 5: Build the Android library**

Run: `mise run ci:check-android`
Expected: compiles.

- [ ] **Step 6: Update jni_parity test if new/renamed external funs appear**

Run: `cargo nextest run -p blew --test jni_parity`
Expected: pass. If fail, an `external fun` signature changed — update the Rust side to match.

- [ ] **Step 7: Commit**

```bash
git add crates/blew/android/src/main/java/org/jakebot/blew/BleCentralManager.kt
git commit -m "feat(android): replace global GATT semaphore with per-device operation queues"
```

### Task 2.3: Apply the same queue to BlePeripheralManager where GATT-server ops serialize

**Files:**
- Modify: `crates/blew/android/src/main/java/org/jakebot/blew/BlePeripheralManager.kt`

The peripheral side already uses per-device notification semaphores. Audit for any remaining adapter-wide serialization (advertiser start/stop is fine being serial since there's only one advertiser).

- [ ] **Step 1: Audit for adapter-wide locks**

Run: `rg '\bSemaphore\b' crates/blew/android/src/main/java/org/jakebot/blew/BlePeripheralManager.kt`

- [ ] **Step 2: Replace any that serialize *across* connected centrals with per-central queues; leave advertiser-start/stop as-is**

No code sample — depends on what audit finds.

- [ ] **Step 3: Commit**

```bash
git add crates/blew/android/src/main/java/org/jakebot/blew/BlePeripheralManager.kt
git commit -m "feat(android): per-central serialization in peripheral manager"
```

---

## Phase 3 — Android JNI safety

### Task 3.1: Wrap every `#[no_mangle] extern "C"` in `catch_unwind`

**Files:**
- Modify: `crates/blew/src/platform/android/jni_hooks.rs`

Panics across FFI are UB. Every hook body must be `catch_unwind(AssertUnwindSafe(…))`.

- [ ] **Step 1: Add a helper macro at the top of jni_hooks.rs**

```rust
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Run `f` and swallow any panic with a `tracing::error!`, so panics don't unwind across FFI.
fn guard<F: FnOnce()>(label: &'static str, f: F) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("<non-string panic>");
        tracing::error!(hook = label, "panic in JNI hook: {msg}");
    }
}
```

- [ ] **Step 2: Wrap each hook body**

For each `pub unsafe extern "C" fn Java_…`, convert:

```rust
pub unsafe extern "C" fn Java_... (mut env: EnvUnowned, ...) {
    env.with_env(|env| { ... }).into_outcome();
}
```

into:

```rust
pub unsafe extern "C" fn Java_... (mut env: EnvUnowned, ...) {
    guard("nativeOnXYZ", || {
        env.with_env(|env| { ... }).into_outcome();
    });
}
```

Keep the `label` matching the Kotlin-visible name for useful logs.

- [ ] **Step 3: Commit**

```bash
git add crates/blew/src/platform/android/jni_hooks.rs
git commit -m "fix(android/jni): wrap every JNI hook in catch_unwind to prevent FFI panics"
```

### Task 3.2: Promote Android permission denial to a typed error

**Files:**
- Modify: `crates/blew/src/error.rs`
- Modify: `crates/blew/android/src/main/java/org/jakebot/blew/BlewPlugin.kt`
- Modify: `crates/blew/src/platform/android/mod.rs` (or wherever `are_ble_permissions_granted` lives)
- Modify: `crates/blew/src/platform/android/central.rs`
- Modify: `crates/blew/src/platform/android/peripheral.rs`

- [ ] **Step 1: Add variant**

In `error.rs`, inside the `BlewError` enum:
```rust
#[error("required Android BLE permissions are not granted")]
PermissionDenied,
```

- [ ] **Step 2: Gate construction on the permission check**

In both `AndroidCentral::new` and `AndroidPeripheral::new`, early-return `BlewError::PermissionDenied` if `are_ble_permissions_granted()` returns false.

- [ ] **Step 3: Wire `onRequestPermissionsResult` to push a result back to Rust**

In `BlewPlugin.kt`, override `onRequestPermissionsResult(requestCode, permissions, grantResults)`. On request-code match, call a new JNI hook `nativeOnPermissionsResult(granted: Boolean)`. The Rust side maintains a `tokio::sync::OnceCell<bool>` (or a broadcast) that `Central::new()`/`Peripheral::new()` can read.

For the minimum viable change: have `BlewPlugin.kt` cache the last-seen grant state in a static boolean read by `are_ble_permissions_granted()`. Surface-complete behavior can wait; the current failure mode (silent success) must be replaced with an `Err(PermissionDenied)`.

- [ ] **Step 4: Build + test**

Run: `mise run ci:check-android && cargo nextest run -p blew`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/blew/src/error.rs crates/blew/android/src/main/java/org/jakebot/blew/BlewPlugin.kt crates/blew/src/platform/android/
git commit -m "feat(android): surface permission denial as BlewError::PermissionDenied"
```

---

## Phase 4 — Apple write-length guards

### Task 4.1: Add `BlewError::ValueTooLarge`

**Files:**
- Modify: `crates/blew/src/error.rs`

- [ ] **Step 1: Add variant**

```rust
#[error("value too large for current MTU: got {got} bytes, max {max}")]
ValueTooLarge { got: usize, max: usize },
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/error.rs
git commit -m "feat(error): add ValueTooLarge variant for MTU/ATT-size guards"
```

### Task 4.2: Guard CoreBluetooth writes against NSInvalidArgumentException

**Files:**
- Modify: `crates/blew/src/platform/apple/central.rs`

Oversize `writeValue:forCharacteristic:type:` raises an NSException → SIGABRT. We check `maximumWriteValueLengthForType(_:)` (which CoreBluetooth reports correctly post-connect — verified 2026-04-16) and return `ValueTooLarge` before the FFI call.

- [ ] **Step 1: Find the write site**

Around `central.rs:~790` inside `write_characteristic`, locate the `unsafe { peripheral.writeValue_forCharacteristic_type(…) }` call.

- [ ] **Step 2: Add the guard**

Before the write call:

```rust
// SAFETY: maximumWriteValueLengthForType is thread-safe per Apple docs; the Retained<CBPeripheral>
// is held via ObjcSend above.
let max = unsafe {
    peripheral.0.maximumWriteValueLengthForType(cb_type) as usize
};
if value.len() > max {
    return Err(BlewError::ValueTooLarge { got: value.len(), max });
}
```

Do this for *both* `.withResponse` and `.withoutResponse` paths — `.withResponse` has its own long-write bugs (FB13596337) so clamping conservatively is safer.

- [ ] **Step 3: Add a unit/integration test covering the guard via the mock**

Not applicable directly since the mock backend has its own MTU. Add a test at `tests/apple_write_guard.rs` gated `#[cfg(all(test, target_vendor = "apple"))]` that requires a hardware device — mark `#[ignore]` for CI.

For mock coverage, raise `testing.rs` MTU clamping in Phase 9.

- [ ] **Step 4: Commit**

```bash
git add crates/blew/src/platform/apple/central.rs
git commit -m "fix(apple): guard writeValue against oversize payloads (NSInvalidArgumentException)"
```

---

## Phase 5 — Apple request-map consolidation

Apple uses six bespoke `Mutex<HashMap<K, oneshot::Sender>>` maps where a shared `RequestMap` would do.

### Task 5.1: Generalize `util::request_map` to accept arbitrary key types

**Files:**
- Modify: `crates/blew/src/util/request_map.rs`

Currently keyed on `u64`. Apple keys on `DeviceId`, `(DeviceId, Uuid)`, etc.

- [ ] **Step 1: Parameterize over `K`**

```rust
use std::collections::HashMap;
use std::hash::Hash;
use parking_lot::Mutex;

pub struct KeyedRequestMap<K: Eq + Hash, V> {
    inner: Mutex<HashMap<K, V>>,
}

impl<K: Eq + Hash, V> KeyedRequestMap<K, V> {
    pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } }
    pub fn insert(&self, key: K, value: V) -> Option<V> { self.inner.lock().insert(key, value) }
    pub fn take(&self, key: &K) -> Option<V> { self.inner.lock().remove(key) }
    pub fn drain(&self) -> Vec<(K, V)> { self.inner.lock().drain().collect() }
}
```

Keep the existing `RequestMap` (u64-keyed, auto-assigning) as a thin wrapper around `KeyedRequestMap<u64, V>` so Android isn't disturbed.

- [ ] **Step 2: Add tests**

```rust
#[test]
fn keyed_insert_and_take() {
    let map = KeyedRequestMap::<String, u32>::new();
    assert_eq!(map.insert("a".into(), 1), None);
    assert_eq!(map.take(&"a".into()), Some(1));
    assert_eq!(map.take(&"a".into()), None);
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/blew/src/util/request_map.rs
git commit -m "feat(util): add KeyedRequestMap for non-u64 keyed pending ops"
```

### Task 5.2: Refactor Apple central to use KeyedRequestMap

**Files:**
- Modify: `crates/blew/src/platform/apple/central.rs`

- [ ] **Step 1: Replace `Mutex<HashMap<…, oneshot::Sender<…>>>` fields with `KeyedRequestMap<…, oneshot::Sender<…>>`**

Scope: `connects`, `reads`, `writes`, `notify_states`, `l2cap_pendings`, `discoveries`. Keep `peripherals` and `chars` as plain maps (they're state, not pending-request slots).

- [ ] **Step 2: Replace every lock-insert/lookup-remove pair with `.insert(key, tx)` / `.take(&key)`**

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p blew` (all tests that use `testing::MockCentral` — Apple's refactor shouldn't break them, and test failures will localize any regression)
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add crates/blew/src/platform/apple/central.rs
git commit -m "refactor(apple): consolidate pending-op tables into KeyedRequestMap"
```

---

## Phase 6 — Linux fixes

### Task 6.1: Fix Linux `mtu()` to return something useful

**Files:**
- Modify: `crates/blew/src/platform/linux/central.rs:536`

Currently hardcoded to `23`. bluer doesn't expose the negotiated ATT MTU directly via `Device` properties, but the `write_io`/`notify_io` socket readers expose an `mtu()` method on their `CharacteristicWriter`/`CharacteristicReader`. Simplest correct interim: return 247 (conservative LE Data Length Extension minimum that nearly all modern devices support).

- [ ] **Step 1: Update the method**

Replace lines 536-538:

```rust
async fn mtu(&self, _device_id: &DeviceId) -> u16 {
    // bluer does not expose negotiated ATT MTU on `Device`. Real value is available per
    // CharacteristicWriter/Reader after opening a socket. Until we plumb that through, return
    // a conservative LE DLE-era default that matches what BlueZ negotiates on almost all
    // modern hardware. See also `iroh-ble-transport` discussion on MTU-at-rest defaults.
    247_u16
}
```

- [ ] **Step 2: File a follow-up issue (or add a `// TODO(mtu-plumb)` comment) for the real fix**

Threading the real MTU requires tracking an open `CharacteristicWriter` per device (or reading `/sys/kernel/debug/bluetooth/…`). Defer to a dedicated mini-plan — the 247 default is 10× better than 23 and unblocks users.

- [ ] **Step 3: Commit**

```bash
git add crates/blew/src/platform/linux/central.rs
git commit -m "fix(linux): default ATT MTU to 247 instead of 23"
```

### Task 6.2: Guard against duplicate `subscribe_characteristic` on Linux

**Files:**
- Modify: `crates/blew/src/platform/linux/central.rs`

Two subscribes on the same `(device, char)` each spawn a reader task on the same `notify_io()` stream — each gets half the data.

- [ ] **Step 1: Check the map before inserting**

Around line 473 where `notify_tasks` is populated:

```rust
{
    let mut tasks = handle.notify_tasks.lock();
    if tasks.contains_key(&(device_id.clone(), char_uuid)) {
        return Err(BlewError::AlreadySubscribed {
            device_id: device_id.clone(),
            char_uuid,
        });
    }
    tasks.insert((device_id.clone(), char_uuid), tokio::spawn(...));
}
```

- [ ] **Step 2: Add the error variant**

In `error.rs`:
```rust
#[error("already subscribed to characteristic {char_uuid} on {device_id}")]
AlreadySubscribed { device_id: DeviceId, char_uuid: Uuid },
```

- [ ] **Step 3: Add a mock parity test**

In `testing.rs` tests: double-subscribing on the mock should return the same error. Extend the mock to enforce this invariant (tighter than today).

- [ ] **Step 4: Build + test + commit**

```bash
mise run test
git add crates/blew/src/error.rs crates/blew/src/platform/linux/central.rs crates/blew/src/testing.rs
git commit -m "fix(linux): reject duplicate subscribe_characteristic calls"
```

### Task 6.3: Apply timeout to Linux `connect`

**Files:**
- Modify: `crates/blew/src/platform/linux/central.rs:~313`

- [ ] **Step 1: Wrap the call**

```rust
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
match tokio::time::timeout(CONNECT_TIMEOUT, device.connect()).await {
    Ok(Ok(())) => {}
    Ok(Err(e)) => return Err(BlewError::Central { source: Box::new(e) }),
    Err(_) => return Err(BlewError::Timeout),
}
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/platform/linux/central.rs
git commit -m "fix(linux): timeout device.connect() at 30s instead of blocking indefinitely"
```

---

## Phase 7 — Typed error variants

Reduce `BlewError::Internal(String)` sprawl. Introduce typed variants for recurring platform-reportable conditions.

### Task 7.1: Inventory current `Internal(...)` construction sites

- [ ] **Step 1: Enumerate**

Run: `rg 'BlewError::Internal\(' crates/blew/src/`

Classify each site into buckets:
- adapter gone / not found → already covered by `AdapterNotFound` / `NotPowered`
- stream closed (e.g., `wait_ready`'s "adapter event stream closed") → new variant `AdapterStreamClosed`
- peer disconnected mid-op → new variant `DisconnectedDuringOperation`
- service/characteristic discovery failure → new variant `DiscoveryFailed { reason: String }`
- truly unexpected → keep as `Internal`

Document the inventory inline in this step (add as a comment or a git commit body) before making changes.

### Task 7.2: Add typed variants + migrate call sites

**Files:**
- Modify: `crates/blew/src/error.rs`
- Modify: across Apple / Linux / Android / testing

- [ ] **Step 1: Add to `error.rs`**

```rust
#[error("event stream closed unexpectedly")]
StreamClosed,

#[error("device {0} disconnected during operation")]
DisconnectedDuringOperation(DeviceId),

#[error("GATT discovery failed on {device_id}: {reason}")]
DiscoveryFailed { device_id: DeviceId, reason: String },

#[error("MTU negotiation failed: {reason}")]
MtuNegotiationFailed { reason: String },
```

(Only add what the inventory in 7.1 demands — don't speculate.)

- [ ] **Step 2: Migrate call sites per bucket**

For each `Internal(...)` classified above, replace with the appropriate typed variant.

- [ ] **Step 3: Update `Central::wait_ready` / `Peripheral::wait_ready`**

`peripheral/mod.rs:138` currently uses `BlewError::Internal("adapter event stream closed".into())`. Replace with `BlewError::StreamClosed`. Same for `central/mod.rs:197`.

- [ ] **Step 4: Test + commit**

```bash
mise run test
git add -A
git commit -m "feat(error): promote recurring Internal(String) sites to typed variants"
```

---

## Phase 8 — PeripheralEvent split (the breaking change)

Split `PeripheralEvent` into `PeripheralStateEvent` (Clone, broadcast, multi-subscriber) and `PeripheralRequest` (carries responders, single-consumer via `&mut self`).

This is the biggest change. Allocate a fresh feature branch and do this last so earlier phases stay trivially mergeable.

### Task 8.1: Define new types

**Files:**
- Modify: `crates/blew/src/peripheral/types.rs`

- [ ] **Step 1: Replace the monolithic `PeripheralEvent` enum**

Replace the existing `PeripheralEvent` with:

```rust
/// Non-request events from the peripheral role. Clone-able; multiple subscribers welcome.
#[derive(Debug, Clone)]
pub enum PeripheralStateEvent {
    AdapterStateChanged { powered: bool },
    SubscriptionChanged {
        client_id: DeviceId,
        char_uuid: Uuid,
        subscribed: bool,
    },
}

/// Inbound GATT requests. Each carries an owned responder that must be consumed exactly once
/// (or dropped, which sends an ATT Application Error).
#[derive(Debug)]
pub enum PeripheralRequest {
    Read {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        offset: u16,
        responder: ReadResponder,
    },
    Write {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        value: Vec<u8>,
        responder: Option<WriteResponder>,
    },
}
```

Keep `ReadResponder` / `WriteResponder` unchanged.

- [ ] **Step 2: Keep a deprecated `PeripheralEvent` re-export? No.**

Since the user is the only consumer and we're pre-1.0, delete `PeripheralEvent` outright. No shim.

- [ ] **Step 3: Commit (it'll break the build; that's fine, the next tasks fix it)**

```bash
git add crates/blew/src/peripheral/types.rs
git commit -m "feat(peripheral): split PeripheralEvent into StateEvent and Request"
```

### Task 8.2: Update the `PeripheralBackend` trait

**Files:**
- Modify: `crates/blew/src/peripheral/backend.rs`

- [ ] **Step 1: Replace the single event stream with two streams**

```rust
pub trait PeripheralBackend: private::Sealed + Send + Sync + 'static {
    type StateEvents: Stream<Item = PeripheralStateEvent> + Send + Unpin + 'static;
    type Requests: Stream<Item = PeripheralRequest> + Send + Unpin + 'static;

    fn new() -> impl Future<Output = BlewResult<Self>> + Send where Self: Sized;
    fn is_powered(&self) -> impl Future<Output = BlewResult<bool>> + Send;
    fn add_service(&self, service: &GattService) -> impl Future<Output = BlewResult<()>> + Send;
    fn start_advertising(&self, config: &AdvertisingConfig) -> impl Future<Output = BlewResult<()>> + Send;
    fn stop_advertising(&self) -> impl Future<Output = BlewResult<()>> + Send;
    fn notify_characteristic(
        &self,
        device_id: &DeviceId,
        char_uuid: Uuid,
        value: Vec<u8>,
    ) -> impl Future<Output = BlewResult<()>> + Send;
    fn l2cap_listener(
        &self,
    ) -> impl Future<
        Output = BlewResult<(
            Psm,
            impl Stream<Item = BlewResult<(DeviceId, L2capChannel)>> + Send + 'static,
        )>,
    > + Send;

    /// State events are clonable and can be subscribed to by multiple consumers.
    fn state_events(&self) -> Self::StateEvents;

    /// Inbound GATT requests. The returned stream is single-consumer by construction:
    /// each call to `take_requests` consumes the internal receiver and returns it. The second
    /// call returns `None`.
    fn take_requests(&self) -> Option<Self::Requests>;
}
```

Note: `take_requests` returns `Option<Self::Requests>`. First call returns `Some`, subsequent `None`. This expresses single-consumer at the type level without needing `&mut self` on `Peripheral` (which would break `Arc<Peripheral>` sharing).

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/peripheral/backend.rs
git commit -m "feat(peripheral): PeripheralBackend trait vends state_events + take_requests"
```

### Task 8.3: Update `Peripheral` API

**Files:**
- Modify: `crates/blew/src/peripheral/mod.rs`

- [ ] **Step 1: Replace the `events()` method**

```rust
pub struct Peripheral<B: PeripheralBackend = PlatformPeripheral> {
    pub(crate) backend: B,
}

impl<B: PeripheralBackend> Peripheral<B> {
    pub async fn new() -> BlewResult<Self> { Ok(Self { backend: B::new().await? }) }

    /// Subscribe to state events. Each call returns an independent stream.
    pub fn state_events(&self) -> EventStream<PeripheralStateEvent, B::StateEvents> {
        EventStream::new(self.backend.state_events())
    }

    /// Take ownership of the GATT request stream. Returns `None` on the second call.
    pub fn take_requests(&self) -> Option<EventStream<PeripheralRequest, B::Requests>> {
        self.backend.take_requests().map(EventStream::new)
    }

    // ... all other methods unchanged ...

    pub async fn wait_ready(&self, timeout: Duration) -> BlewResult<()> {
        if self.backend.is_powered().await.unwrap_or(false) { return Ok(()); }
        let mut events = self.state_events();  // state_events is multi-subscriber now, no bypass needed
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { return Err(BlewError::Timeout); }
            match tokio::time::timeout(remaining, tokio_stream::StreamExt::next(&mut events)).await {
                Err(_) => return Err(BlewError::Timeout),
                Ok(None) => return Err(BlewError::StreamClosed),
                Ok(Some(PeripheralStateEvent::AdapterStateChanged { powered: true })) => return Ok(()),
                Ok(Some(_)) => {}
            }
        }
    }
}
```

Delete `events_inner`, the `events_taken` atomic, and the debug-only assert — no longer needed.

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/peripheral/mod.rs
git commit -m "feat(peripheral): new API with state_events() + take_requests()"
```

### Task 8.4: Update each platform backend

**Files:**
- Modify: `crates/blew/src/platform/apple/peripheral.rs`
- Modify: `crates/blew/src/platform/linux/peripheral.rs`
- Modify: `crates/blew/src/platform/android/peripheral.rs`
- Modify: `crates/blew/src/testing.rs`

Each backend needs:
- A `parking_lot::Mutex<Option<mpsc::UnboundedReceiver<PeripheralRequest>>>` for the single-consumer request stream.
- A `tokio::sync::broadcast::Sender<PeripheralStateEvent>` for multi-consumer state events.
- Internal fan: callback emits a `PeripheralRequest` to the request channel OR a `PeripheralStateEvent` to the broadcast channel, depending on type.

- [ ] **Step 1: Apple backend**

Rewrite `ApplePeripheralInner` fields:
```rust
request_tx: parking_lot::Mutex<Option<mpsc::UnboundedSender<PeripheralRequest>>>,
request_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<PeripheralRequest>>>,
state_tx: tokio::sync::broadcast::Sender<PeripheralStateEvent>,
```

In `ApplePeripheral::new`: create both channels, populate the inner, store the rx in `request_rx`.

In delegate callbacks: route `didReceiveReadRequest`/`didReceiveWriteRequests` to the `request_tx`, route power-state/subscription callbacks to `state_tx.send(...)`.

Implement trait:
```rust
type StateEvents = BroadcastStream<PeripheralStateEvent>;
type Requests = UnboundedReceiverStream<PeripheralRequest>;

fn state_events(&self) -> Self::StateEvents {
    BroadcastStream::new(self.inner.state_tx.subscribe())
}

fn take_requests(&self) -> Option<Self::Requests> {
    self.inner.request_rx.lock().take().map(UnboundedReceiverStream::new)
}
```

Import `tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream}`. Add to `crates/blew/Cargo.toml` if not already present:
```toml
tokio-stream = { version = "0.1", features = ["sync"] }
```

- [ ] **Step 2: Linux backend**

Same structural change. The bluer callback closures in `peripheral.rs:70-155` (the `CharacteristicRead::fun` / `CharacteristicWrite::fun` blocks) already emit events via `emit(&inner, event)`. Replace `emit` to route by event kind — requests go to `request_tx`, state events go to `state_tx.send`.

- [ ] **Step 3: Android backend**

Same structural change inside `platform/android/peripheral.rs`. The JNI hooks already classify events (onReadRequest / onWriteRequest / onSubscriptionChanged / onAdapterStateChanged); just route each to the right channel.

- [ ] **Step 4: testing.rs mock**

Update `MockPeripheral` to expose the same two-channel shape. The mock should return an already-taken `None` on the second `take_requests()` call, matching real backend semantics.

- [ ] **Step 5: Update examples**

`examples/advertise.rs` currently uses `peripheral.events()`. Split it:

```rust
let peripheral: Peripheral = Peripheral::new().await?;
// ... add_service, start_advertising ...
let mut requests = peripheral.take_requests().expect("requests already taken");
let mut state = peripheral.state_events();

tokio::spawn(async move {
    while let Some(ev) = tokio_stream::StreamExt::next(&mut state).await {
        println!("state: {ev:?}");
    }
});

while let Some(req) = tokio_stream::StreamExt::next(&mut requests).await {
    match req {
        PeripheralRequest::Read { responder, .. } => responder.respond(b"hello".to_vec()),
        PeripheralRequest::Write { responder, .. } => {
            if let Some(r) = responder { r.success(); }
        }
    }
}
```

- [ ] **Step 6: Update `single_consumer_test` in `peripheral/mod.rs`**

Delete the debug-panic test at lines 146-155. Replace with:

```rust
#[cfg(all(test, feature = "testing"))]
mod take_requests_tests {
    #[tokio::test]
    async fn second_take_returns_none() {
        let p = crate::testing::MockPeripheral::new_powered();
        assert!(p.take_requests().is_some());
        assert!(p.take_requests().is_none());
    }
}
```

- [ ] **Step 7: Run the full test suite**

```bash
mise run test
mise run lint
```
Expected: everything green.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(peripheral): implement state_events/take_requests across all backends"
```

---

## Phase 9 — Tighten the mocks

### Task 9.1: Enforce connection state + service existence in `testing.rs`

**Files:**
- Modify: `crates/blew/src/testing.rs`

Current mock accepts read/write/subscribe on any device without connection or service checks. Real backends reject these. Tests pass shapes that fail on hardware.

- [ ] **Step 1: Add a `connected: AtomicBool` per `MockLink` device entry**

Set true on `connect`, false on `disconnect`. Check in `read_characteristic`, `write_characteristic`, `subscribe_characteristic`, and reject with `BlewError::NotConnected(device_id)` if false.

- [ ] **Step 2: Enforce service/char existence**

Check the mock's registered services against the UUID in read/write/subscribe. If absent, return `BlewError::CharacteristicNotFound { device_id, char_uuid }`.

- [ ] **Step 3: Enforce MTU on write**

The mock already stores MTU (hardcoded 512). Change writes to reject `>= mtu - 3` with `BlewError::ValueTooLarge { got, max: mtu - 3 }`.

- [ ] **Step 4: Reject duplicate subscribes (matching Phase 6.2)**

- [ ] **Step 5: Run tests — expect some existing tests to fail**

Run: `cargo nextest run -p blew --features testing`
Expected: some failures as loose tests are caught. Fix them — each failure is a test that was passing on shapes the real backend would reject.

- [ ] **Step 6: Commit**

```bash
git add crates/blew/src/testing.rs
git commit -m "test(mock): enforce connection state, service existence, MTU, duplicate-subscribe"
```

---

## Phase 10 — Docs migration

Move load-bearing invariants from CLAUDE.md into rustdoc that ships with the crate.

### Task 10.1: Promote CoreBluetooth crashes into `GattCharacteristic` docs

**Files:**
- Modify: `crates/blew/src/gatt/service.rs`

- [ ] **Step 1: Add doc comment**

On `GattCharacteristic`:

```rust
/// A GATT characteristic.
///
/// # Platform caveats
///
/// **Apple:** If `value` is non-empty **and** `properties` includes
/// [`CharacteristicProperties::WRITE`], CoreBluetooth treats the characteristic as static and
/// raises `NSInvalidArgumentException` → `SIGABRT` when a central writes to it. For any
/// writable characteristic, set `value: vec![]` and handle reads via
/// [`PeripheralRequest::Read`](crate::peripheral::PeripheralRequest::Read).
pub struct GattCharacteristic { ... }
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/gatt/service.rs
git commit -m "docs(gatt): document CoreBluetooth writable+static-value crash on GattCharacteristic"
```

### Task 10.2: Promote BlueZ cache quirk into `ScanFilter` docs

**Files:**
- Modify: `crates/blew/src/central/types.rs`

- [ ] **Step 1: Document on `ScanFilter`**

```rust
/// Filter for a BLE scan.
///
/// # Platform caveats
///
/// **Linux/BlueZ:** BlueZ's cache may emit `DeviceAdded` events for devices that do not match
/// `services` on the first tick after a fresh scan start. Filter client-side on the
/// [`CentralEvent::DeviceDiscovered`](super::CentralEvent::DeviceDiscovered) stream if you need strict behavior.
pub struct ScanFilter { ... }
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/central/types.rs
git commit -m "docs(central): document BlueZ cache behavior on ScanFilter"
```

### Task 10.3: Promote the default-type-parameter requirement into crate root docs

**Files:**
- Modify: `crates/blew/src/lib.rs`

- [ ] **Step 1: Add a visible note at the top of the crate docs**

```rust
//! # Getting started
//!
//! Construct [`Central`] or [`Peripheral`] with an explicit type annotation:
//!
//! ```no_run
//! # async fn example() -> blew::error::BlewResult<()> {
//! use blew::central::Central;
//! let central: Central = Central::new().await?;
//! # Ok(()) }
//! ```
//!
//! The annotation is required because Rust's default type-parameter inference does not fire on
//! method calls. If you see compiler error E0283 ("type annotations needed"), you forgot the
//! `: Central`.
```

- [ ] **Step 2: Commit**

```bash
git add crates/blew/src/lib.rs
git commit -m "docs(lib): document explicit-type-annotation requirement in crate root"
```

### Task 10.4: Add a "Platform notes" section to the top-level README

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add under the platform-support table**

```markdown
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
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs(readme): add platform-notes section covering per-backend quirks"
```

---

## Phase 11 — Final sweep

### Task 11.1: Remove obsolete `Internal(String)` fallbacks and `#[allow(dead_code)]` holdouts

- [ ] **Step 1: Enumerate remaining `Internal(` sites after Phase 7**

Run: `rg 'Internal\(' crates/blew/src/`
Expected: a small residue of truly-unexpected-error sites. If any fit the buckets from Phase 7.1, file a follow-up.

- [ ] **Step 2: Audit `#[allow(dead_code)]`**

Run: `rg '#\[allow\(dead_code\)\]' crates/blew/src/`
Expected: entries on `ReadResponder::new`/`WriteResponder::new` that are actually used by backends via `pub(crate) fn new` — if still unused, delete the impl + the allow; if used, delete the allow.

- [ ] **Step 3: Final pedantic sweep**

Run: `mise run lint`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore: final sweep of dead_code allows and residual Internal errors"
```

### Task 11.2: Update CLAUDE.md with the final architecture

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Reflect all the phase changes**

- Remove `PeripheralEvent` mentions → replace with `PeripheralStateEvent` + `PeripheralRequest`.
- Remove the "only most-recent subscriber receives events" note (no longer true for state events).
- Remove the "Android BLE uses adapter-wide `Semaphore(1)`" note → replace with per-device queue note.
- Update `Mutex` pattern references from `std::sync::Mutex` to `parking_lot::Mutex`.
- Update the module-structure diagram for the co-located Android source.

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude): update architecture notes for v0.2 overhaul"
```

---

## Self-review

Spec coverage (from the user's 6-item list):

1. **PeripheralEvent: option (D) Split by kind** — Phase 8. Implemented via `state_events()` (broadcast) + `take_requests()` (single-consumer, None on second call). ✓
2. **Apple MTU guards** — Phase 4 (ValueTooLarge + guard). ✓
3. **iroh-ble-transport note in CLAUDE.md** — Phase 0 Task 0.1. ✓
4. **Per-connection Android serialization via OperationQueue** — Phase 2. Channel+coroutine worker per device; covers timeout + cancel-on-disconnect. ✓
5. **Tauri plugin split / Kotlin into blew** — Phase 1. `tauri-plugin-blew` retained as thin Tauri wrapper. ✓
6. **parking_lot everywhere** — Phase 0 Tasks 0.2–0.4. ✓

Additional review findings also covered:
- `catch_unwind` on JNI hooks — Phase 3.1. ✓
- `BlewError::PermissionDenied` and denial wiring — Phase 3.2. ✓
- Apple request-map consolidation — Phase 5. ✓
- Linux MTU, duplicate-sub, connect timeout — Phase 6. ✓
- Typed error variants instead of `Internal(String)` — Phase 7. ✓
- Mock invariant tightening — Phase 9. ✓
- Docs migration (CoreBluetooth write quirk, BlueZ cache, annotation requirement) — Phase 10. ✓

Not covered (intentionally deferred):
- Unified `BleAdapter` type combining Central + Peripheral — larger breaking change; worth its own plan.
- Real-hardware CI lanes — infrastructure change, not a code change.
- Full Linux MTU plumbing via `CharacteristicWriter::mtu()` — Task 6.1 explicitly defers this to a follow-up mini-plan.
- Broadcast-lagged handling UX for central `EventFanout` — no user report yet; can wait.

Placeholder scan: no TBDs, no "add error handling" hand-waves. Code blocks included wherever the change is non-mechanical. For the parking_lot sweep (~114 sites across 13 files), the pattern is identical so one example + a walk instruction is given rather than 114 separate tasks.

Type consistency:
- `PeripheralStateEvent` / `PeripheralRequest` naming matches across Tasks 8.1–8.4, examples, and mock updates.
- `take_requests` (not `requests` / `request_stream`) is used consistently.
- `BlewError::ValueTooLarge { got, max }` field names match between Apple (Phase 4.2) and mock (Phase 9.1).
- `BlewError::AlreadySubscribed { device_id, char_uuid }` fields match between error definition and call site.

---

Plan complete and saved to `docs/superpowers/plans/2026-04-16-blew-architectural-overhaul.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
