//! Android launch smoke test for `tauri-plugin-blew`.
//!
//! The failures this catches happen while a real activity starts: tao
//! publishes `ndk_context` in `onCreate`, the plugin's `setup` reads it back,
//! and `init_jvm` loads the Kotlin managers through the application's
//! classloader. None of that runs outside an app on a device, so CI installs
//! this on an emulator and reads the verdict from logcat: `BLEW_SMOKE_OK`, or
//! `BLEW_SMOKE_FAIL` with the reason. A crash prints neither.

use blew::{Central, Peripheral};
use tauri_plugin_blew::BlewPluginConfig;

const TAG: &str = "blew-smoke";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag(TAG),
    );
    // Rust's panic message goes to stderr, which Android discards, so a panic
    // in plugin setup would otherwise show up as a bare SIGABRT.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(&format!("BLEW_SMOKE_PANIC {info}"));
        default_hook(info);
    }));

    tauri::Builder::default()
        // CI grants the runtime permissions at install; a dialog would only
        // sit in front of the activity.
        .plugin(tauri_plugin_blew::init_with_config(BlewPluginConfig {
            auto_request_permissions: false,
        }))
        .setup(|_app| {
            tauri::async_runtime::spawn(async {
                match smoke().await {
                    Ok(summary) => report(&format!("BLEW_SMOKE_OK {summary}")),
                    Err(e) => report(&format!("BLEW_SMOKE_FAIL {e}")),
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn smoke() -> Result<String, String> {
    if !tauri_plugin_blew::is_initialized() {
        return Err("plugin setup did not initialise blew's JNI bridge".into());
    }
    if !tauri_plugin_blew::are_ble_permissions_granted() {
        return Err("BLE permissions not granted (install with `adb install -g`)".into());
    }
    // From a Tokio worker, not the main thread: the JNI calls here only find
    // the Kotlin classes through what init_jvm cached from ndk_context.
    let central: Central = Central::new()
        .await
        .map_err(|e| format!("Central::new: {e}"))?;
    let central_powered = central
        .is_powered()
        .await
        .map_err(|e| format!("Central::is_powered: {e}"))?;
    let peripheral: Peripheral = Peripheral::new()
        .await
        .map_err(|e| format!("Peripheral::new: {e}"))?;
    let peripheral_powered = peripheral
        .is_powered()
        .await
        .map_err(|e| format!("Peripheral::is_powered: {e}"))?;
    Ok(format!(
        "central_powered={central_powered} peripheral_powered={peripheral_powered}"
    ))
}

fn report(line: &str) {
    #[cfg(target_os = "android")]
    log::info!(target: TAG, "{line}");
    #[cfg(not(target_os = "android"))]
    println!("{TAG}: {line}");
}
