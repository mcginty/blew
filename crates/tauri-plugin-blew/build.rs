fn main() {
    let mut builder = tauri_plugin::Builder::new(&[]);
    if let Ok(android_dir) = std::env::var("DEP_BLEW_ANDROID_DIR") {
        builder = builder.android_path(android_dir);
    }
    builder.build();

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
