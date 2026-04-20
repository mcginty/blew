use tauri::{plugin::TauriPlugin, Runtime};

#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "android")]
use std::sync::OnceLock;

#[cfg(target_os = "android")]
use tokio::sync::broadcast;

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "org.jakebot.blew";

#[cfg(target_os = "android")]
static AUTO_REQUEST_PERMISSIONS: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "android")]
static PERMISSIONS_TX: OnceLock<broadcast::Sender<BlePermissionStatus>> = OnceLock::new();

/// Current status of the aggregate Android BLE runtime permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlePermissionStatus {
    /// All BLE runtime permissions required for the current Android API level
    /// have been granted.
    Granted,
    /// At least one required BLE runtime permission is not granted.
    Denied,
}

impl BlePermissionStatus {
    #[must_use]
    pub fn is_granted(self) -> bool {
        matches!(self, BlePermissionStatus::Granted)
    }
}

/// Plugin configuration.
#[derive(Clone, Debug)]
pub struct BlewPluginConfig {
    /// If `true` (default), Android BLE runtime permissions are requested
    /// immediately when the plugin loads. Set to `false` to defer the request
    /// so your app can show an explanation modal first, then call
    /// [`request_ble_permissions`] when the user is ready.
    pub auto_request_permissions: bool,
}

impl Default for BlewPluginConfig {
    fn default() -> Self {
        Self {
            auto_request_permissions: true,
        }
    }
}

/// Check whether Android BLE runtime permissions have been granted.
///
/// Always returns `true` on non-Android platforms.
pub fn are_ble_permissions_granted() -> bool {
    #[cfg(target_os = "android")]
    {
        blew::platform::android::are_ble_permissions_granted()
    }
    #[cfg(not(target_os = "android"))]
    {
        true
    }
}

/// Trigger the Android BLE runtime permissions dialog.
///
/// Fire-and-forget — the dialog is presented asynchronously on the host
/// activity's UI thread. Use [`permission_events`] to observe when the user
/// responds (or check [`are_ble_permissions_granted`]).
///
/// Must be called after the Tauri app has finished initializing the plugin
/// (the Android side needs the host `Activity`, which is captured during
/// plugin load). No-op on non-Android platforms.
pub fn request_ble_permissions() {
    #[cfg(target_os = "android")]
    {
        blew::platform::android::request_ble_permissions();
    }
}

/// Subscribe to Android BLE permission-change events.
///
/// Emits a [`BlePermissionStatus`] whenever the aggregate BLE-permission state
/// flips between granted and denied. Changes are detected on each activity
/// resume, so this catches both in-app dialog responses and out-of-app toggles
/// (e.g. the user flipping a switch in system Settings while the app is
/// backgrounded).
///
/// On non-Android platforms this returns an empty stream — the pattern is
/// iOS-specific on Apple (use the existing Central/Peripheral state streams,
/// which surface `CBManager` authorization changes via `centralManagerDidUpdateState:`).
#[cfg(target_os = "android")]
pub fn permission_events() -> blew::util::BroadcastEventStream<BlePermissionStatus> {
    let tx = PERMISSIONS_TX.get_or_init(|| broadcast::channel(16).0);
    blew::util::BroadcastEventStream::new(tx.subscribe())
}

/// Check whether the app is running on an emulator or simulator.
///
/// Returns `true` on Android emulators and iOS simulators, `false` on real devices
/// and non-mobile platforms.
pub fn is_emulator() -> bool {
    #[cfg(target_os = "android")]
    {
        blew::platform::android::is_emulator()
    }
    #[cfg(not(target_os = "android"))]
    {
        std::env::var("SIMULATOR_DEVICE_NAME").is_ok()
    }
}

/// Initialize the plugin with default configuration (auto-requests BLE
/// permissions on load).
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    init_with_config(BlewPluginConfig::default())
}

/// Initialize the plugin with a custom [`BlewPluginConfig`].
pub fn init_with_config<R: Runtime>(config: BlewPluginConfig) -> TauriPlugin<R> {
    #[cfg(target_os = "android")]
    {
        AUTO_REQUEST_PERMISSIONS.store(config.auto_request_permissions, Ordering::Relaxed);
        let _ = PERMISSIONS_TX.get_or_init(|| broadcast::channel(16).0);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = config;
    }

    tauri::plugin::Builder::<R>::new("blew")
        .setup(|_app, api| {
            #[cfg(target_os = "android")]
            {
                let ctx = ndk_context::android_context();
                let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
                blew::platform::android::init_jvm(vm);
                api.register_android_plugin(PLUGIN_IDENTIFIER, "BlewPlugin")?;
            }
            let _ = api;
            Ok(())
        })
        .build()
}

/// JNI entry point invoked from `BlewPluginNative.autoRequestPermissionsEnabled()`
/// on Android to read the flag set by [`init_with_config`].
///
/// # Safety
///
/// Invoked by the JVM through the normal JNI calling convention; safe provided
/// the signature matches the Kotlin `external fun` declaration.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlewPluginNative_autoRequestPermissionsEnabled(
    _env: jni::EnvUnowned,
    _class: jni::objects::JClass,
) -> jni::sys::jboolean {
    AUTO_REQUEST_PERMISSIONS.load(Ordering::Relaxed)
}

/// JNI entry point invoked from `BlewPluginNative.onPermissionsChanged(granted)`
/// on Android when the plugin's `onResume` detects a change in the BLE
/// runtime-permission state.
///
/// # Safety
///
/// Invoked by the JVM through the normal JNI calling convention; safe provided
/// the signature matches the Kotlin `external fun` declaration.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_org_jakebot_blew_BlewPluginNative_onPermissionsChanged(
    _env: jni::EnvUnowned,
    _class: jni::objects::JClass,
    granted: jni::sys::jboolean,
) {
    let status = if granted {
        BlePermissionStatus::Granted
    } else {
        BlePermissionStatus::Denied
    };
    if let Some(tx) = PERMISSIONS_TX.get() {
        let _ = tx.send(status);
    }
}
