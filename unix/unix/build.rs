const DELEGATE_FORWARD: &str = "src/metal/macdrv/delegate_forward.m";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(perf_tracking)");
    println!("cargo:rerun-if-env-changed=MTLD3D_PERF");
    if std::env::var("MTLD3D_PERF").is_ok_and(|value| !value.is_empty() && value != "0") {
        println!("cargo:rustc-cfg=perf_tracking");
    }
    println!("cargo::rustc-check-cfg=cfg(mtld3d_crumb)");
    println!("cargo:rerun-if-env-changed=MTLD3D_CRUMB");
    if std::env::var("MTLD3D_CRUMB").is_ok_and(|v| !v.is_empty() && v != "0") {
        println!("cargo:rustc-cfg=mtld3d_crumb");
    }

    let target = std::env::var("TARGET").unwrap();
    if !target.contains("apple") {
        return;
    }

    // The native host build links this crate into `libd3d9metal`, which names
    // itself, and has no macdrv, so no delegate to forward to.
    if std::env::var_os("CARGO_FEATURE_NATIVE_HOST").is_some() {
        return;
    }

    println!("cargo:rustc-link-arg-cdylib=-Wl,-install_name,@rpath/mtld3d.so");

    // Every Rust frame of this crate aborts on unwind (`panic = "abort"`),
    // so a message send whose Objective-C exception has to be caught keeps
    // its `@try` and the send together in Objective-C.
    println!("cargo:rerun-if-changed={DELEGATE_FORWARD}");
    cc::Build::new()
        .file(DELEGATE_FORWARD)
        .flag("-fobjc-exceptions")
        .flag("-fno-objc-arc")
        .warnings_into_errors(true)
        .compile("mtld3d_delegate_forward");
}
