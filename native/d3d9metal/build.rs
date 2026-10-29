fn main() {
    // Found by a host through its rpath, under the name it links against.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" || target_os == "ios" {
        println!("cargo:rustc-link-arg-cdylib=-Wl,-install_name,@rpath/libd3d9metal.dylib");
    }
}
