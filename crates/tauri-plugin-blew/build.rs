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
