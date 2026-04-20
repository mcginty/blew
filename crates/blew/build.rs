fn main() {
    let android_dir =
        std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("android");
    println!("cargo:android_dir={}", android_dir.display());
}
