use std::sync::OnceLock;

use jni::objects::{JClass, JObject, JValue};
use jni::refs::Global;
use jni::{Env, JavaVM, jni_sig, jni_str};

jni::bind_java_type! { pub JContext => android.content.Context }

static JVM: OnceLock<JavaVM> = OnceLock::new();
static CLASS_CENTRAL: OnceLock<Global<JClass<'static>>> = OnceLock::new();
static CLASS_PERIPHERAL: OnceLock<Global<JClass<'static>>> = OnceLock::new();
static APP_CONTEXT: OnceLock<Global<JObject<'static>>> = OnceLock::new();

/// Store the JVM and the application context, and cache the class lookups the
/// Android BLE backends need.
///
/// `context` is any `android.content.Context` of the app; its application
/// context is what gets kept. Classes are resolved through that context's
/// classloader, so this works even though the backends later run on Rust
/// threads: `env.find_class()` there gets the system classloader, which can't
/// see APK classes.
///
/// Call it from a Kotlin `external fun` taking a `Context` (`tauri-plugin-blew`
/// does this from `BlewPlugin`'s constructor). Calling it again is a no-op.
/// Fails if class lookup or manager init fails, since nothing downstream can
/// work without them.
pub fn init_jvm(env: &mut Env<'_>, context: &JContext<'_>) -> jni::errors::Result<()> {
    // A plugin that fails when it is registered twice is a sharp edge, and
    // nothing about registering it twice is unrecoverable -- the JVM and the
    // classes are the same either way.
    if is_initialized() {
        tracing::debug!("init_jvm called again; keeping the existing JVM");
        return Ok(());
    }
    let vm = env.get_java_vm()?;
    // The application context, not an Activity: this is held for the life of
    // the process, and an Activity held that long drags its view hierarchy
    // past every recreation.
    let app_context = env
        .call_method(
            context,
            jni_str!("getApplicationContext"),
            jni_sig!("()Landroid/content/Context;"),
            &[],
        )?
        .l()?;
    let app_context = if app_context.is_null() {
        env.new_global_ref(&**context)?
    } else {
        env.new_global_ref(app_context)?
    };
    let class_loader = env
        .call_method(
            &app_context,
            jni_str!("getClassLoader"),
            jni_sig!("()Ljava/lang/ClassLoader;"),
            &[],
        )?
        .l()?;

    let central_ref = load_class(env, &class_loader, "org.jakebot.blew.BleCentralManager")?;
    let peripheral_ref = load_class(env, &class_loader, "org.jakebot.blew.BlePeripheralManager")?;

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
    for manager_class in [&central_ref, &peripheral_ref] {
        env.call_static_method(
            manager_class,
            jni_str!("init"),
            jni_sig!("(Landroid/content/Context;)V"),
            &[JValue::Object(&app_context)],
        )?;
    }

    let _ = CLASS_CENTRAL.set(central_ref);
    let _ = CLASS_PERIPHERAL.set(peripheral_ref);
    let _ = APP_CONTEXT.set(app_context);
    let _ = JVM.set(vm);
    Ok(())
}

/// Load a class by name using the given classloader, returning a Global.
fn load_class(
    env: &mut Env,
    class_loader: &JObject,
    class_name: &str,
) -> jni::errors::Result<Global<JClass<'static>>> {
    let j_name = env.new_string(class_name)?;
    let cls = env
        .call_method(
            class_loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(&j_name)],
        )?
        .l()?;
    let cls = unsafe { JClass::from_raw(env, cls.as_raw()) };
    env.new_global_ref(cls)
}

/// Whether [`init_jvm`] has run.
///
/// The accessors below panic without it, which is the right behaviour deep in
/// the backend but a poor way to greet an application that simply forgot to
/// register the plugin — the role constructors check this first and return
/// [`BlewError::NotInitialized`](crate::error::BlewError::NotInitialized).
#[must_use]
pub fn is_initialized() -> bool {
    JVM.get().is_some()
        && CLASS_CENTRAL.get().is_some()
        && CLASS_PERIPHERAL.get().is_some()
        && APP_CONTEXT.get().is_some()
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

/// The application context [`init_jvm`] was given.
pub(crate) fn app_context() -> &'static Global<JObject<'static>> {
    APP_CONTEXT.get().expect("JNI app context not initialized")
}

/// The `JavaVM` [`init_jvm`] stored, or `None` before it has run.
///
/// For other crates in the process that need JNI. On Tauri < 2.12, where
/// nothing publishes `ndk_context`, this and [`application_context`] are what
/// a host needs to publish it so `ndk_context` readers such as iroh's system
/// DNS lookup work. blew itself never touches `ndk_context`.
#[must_use]
pub fn java_vm() -> Option<&'static JavaVM> {
    JVM.get()
}

/// The global reference to the application context [`init_jvm`] kept, or
/// `None` before it has run.
///
/// blew holds the reference for the life of the process and never deletes
/// it, so its raw pointer stays valid until exit. See [`java_vm`].
#[must_use]
pub fn application_context() -> Option<&'static Global<JObject<'static>>> {
    if is_initialized() {
        APP_CONTEXT.get()
    } else {
        None
    }
}
