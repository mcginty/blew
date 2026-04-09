pub mod central;
pub mod jni_globals;
pub mod jni_hooks;
pub(crate) mod l2cap_state;
pub mod peripheral;

pub use central::AndroidCentral;
pub use jni_globals::init_jvm;
pub use peripheral::AndroidPeripheral;

/// Check whether the app is running on an Android emulator.
///
/// Checks `Build.FINGERPRINT` for known emulator markers ("generic", "emulator", "sdk").
pub fn is_emulator() -> bool {
    use jni::{jni_sig, jni_str};
    let result: Result<bool, jni::errors::Error> =
        jni_globals::jvm().attach_current_thread(|env| {
            let build_class = env.find_class(jni_str!("android/os/Build"))?;
            let fingerprint = env.get_static_field(
                &build_class,
                jni_str!("FINGERPRINT"),
                jni_sig!("Ljava/lang/String;"),
            )?;
            let obj = fingerprint.l()?;
            let jstr = unsafe { jni::objects::JString::from_raw(env, obj.as_raw()) };
            #[allow(deprecated)]
            let fp: String = env.get_string(&jstr)?.into();
            let fp = fp.to_lowercase();
            Ok(fp.contains("generic") || fp.contains("emulator") || fp.contains("sdk"))
        });
    result.unwrap_or(false)
}

/// Check whether Android BLE runtime permissions have been granted.
///
/// Returns `true` when all required permissions are in place, `false` otherwise.
/// This is Android-specific; other platforms always return `true`.
pub fn are_ble_permissions_granted() -> bool {
    use jni::{jni_sig, jni_str};
    let result: Result<bool, jni::errors::Error> =
        jni_globals::jvm().attach_current_thread(|env| {
            let result = env.call_static_method(
                jni_globals::peripheral_class(),
                jni_str!("areBlePermissionsGranted"),
                jni_sig!("()Z"),
                &[],
            )?;
            Ok(result.z()?)
        });
    result.unwrap_or(false)
}
