use std::sync::OnceLock;

use jni::objects::{JClass, JObject, JValue};
use jni::refs::Global;
use jni::{Env, JavaVM, jni_sig, jni_str};

static JVM: OnceLock<JavaVM> = OnceLock::new();
static CLASS_CENTRAL: OnceLock<Global<JClass<'static>>> = OnceLock::new();
static CLASS_PERIPHERAL: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// Store the JVM reference and cache class lookups for later use by the Android BLE backends.
///
/// Uses the activity's classloader (via `ndk_context`) to find classes, so this works
/// even when called from a Rust background thread. `env.find_class()` would fail there
/// because Rust threads get the system classloader which can't see APK classes.
///
/// Called during plugin initialization (e.g. by `tauri-plugin-blew`). Calling
/// it again is a no-op. Panics if class lookup or manager init fails, since
/// nothing downstream can work without them.
pub fn init_jvm(vm: JavaVM) {
    // A plugin `setup` that aborts the process when it runs twice is a sharp
    // edge, and nothing about registering the plugin twice is unrecoverable --
    // the JVM and the classes are the same either way.
    if JVM.get().is_some() {
        tracing::debug!("init_jvm called again; keeping the existing JVM");
        return;
    }
    vm.attach_current_thread(|env| {
        // The application context, not an Activity -- see
        // `tauri-plugin-blew`'s `install_android_context`.
        let context =
            unsafe { JObject::from_raw(env, ndk_context::android_context().context().cast()) };
        let class_loader = env
            .call_method(
                &context,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )
            .expect("getClassLoader failed")
            .l()
            .expect("not an object");

        let central_ref = load_class(env, &class_loader, "org.jakebot.blew.BleCentralManager");
        let peripheral_ref =
            load_class(env, &class_loader, "org.jakebot.blew.BlePeripheralManager");

        // Hand the managers their Context here rather than waiting for
        // `BlewPlugin.load()`.
        //
        // Without this there is a window where the JVM is registered -- so
        // `Central::new()` is reachable -- but the Kotlin singletons have no
        // Context yet, and `areBlePermissionsGranted()` returns false for a
        // missing Context exactly as it does for a denied permission. An app
        // constructing a Central from its own Tauri setup hook would be told
        // its permissions were denied when they were fine. `init` is
        // idempotent, so `load()` calling it again is harmless.
        let app_context = env
            .call_method(
                &context,
                jni_str!("getApplicationContext"),
                jni_sig!("()Landroid/content/Context;"),
                &[],
            )
            .expect("getApplicationContext failed")
            .l()
            .expect("not an object");
        for manager_class in [&central_ref, &peripheral_ref] {
            env.call_static_method(
                manager_class,
                jni_str!("init"),
                jni_sig!("(Landroid/content/Context;)V"),
                &[JValue::Object(&app_context)],
            )
            .expect("manager init failed");
        }

        let _ = CLASS_CENTRAL.set(central_ref);
        let _ = CLASS_PERIPHERAL.set(peripheral_ref);

        // `context` is a borrowed local ref from ndk_context; `JObject` has no
        // Drop impl, so letting it fall out of scope is a no-op.
        let _ = context;

        Ok::<_, jni::errors::Error>(())
    })
    .expect("init_jvm failed");

    let _ = JVM.set(vm);
}

/// Load a class by name using the given classloader, returning a Global.
fn load_class(env: &mut Env, class_loader: &JObject, class_name: &str) -> Global<JClass<'static>> {
    let j_name = env.new_string(class_name).expect("new_string");
    let cls = env
        .call_method(
            class_loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(&j_name)],
        )
        .unwrap_or_else(|e| panic!("{class_name} not found: {e}"))
        .l()
        .expect("not an object");
    let cls = unsafe { JClass::from_raw(env, cls.as_raw()) };
    env.new_global_ref(cls).expect("global ref")
}

/// Whether [`init_jvm`] has run.
///
/// The accessors below panic without it, which is the right behaviour deep in
/// the backend but a poor way to greet an application that simply forgot to
/// register the plugin — the role constructors check this first and return
/// [`BlewError::NotInitialized`](crate::error::BlewError::NotInitialized).
#[must_use]
pub fn is_initialized() -> bool {
    JVM.get().is_some() && CLASS_CENTRAL.get().is_some() && CLASS_PERIPHERAL.get().is_some()
}

/// Get a reference to the stored JVM.
///
/// Panics if [`init_jvm`] was not called first.
pub(crate) fn jvm() -> &'static JavaVM {
    JVM.get()
        .expect("JVM not initialized -- did you register tauri-plugin-blew?")
}

/// Get a cached `Global<JClass>` for `org.jakebot.blew.BleCentralManager`.
pub(crate) fn central_class() -> &'static Global<JClass<'static>> {
    CLASS_CENTRAL.get().expect("JNI classes not initialized")
}

/// Get a cached `Global<JClass>` for `org.jakebot.blew.BlePeripheralManager`.
pub(crate) fn peripheral_class() -> &'static Global<JClass<'static>> {
    CLASS_PERIPHERAL.get().expect("JNI classes not initialized")
}
