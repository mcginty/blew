use tauri::{plugin::TauriPlugin, Runtime};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "org.jakebot.blew";

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

pub fn init<R: Runtime>() -> TauriPlugin<R> {
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
