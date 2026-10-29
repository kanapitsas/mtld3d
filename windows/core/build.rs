use std::{fs, hash::Hasher, path::Path};

use xxhash_rust::xxh3::Xxh3;

fn main() {
    emitter_fingerprint();
    // `pe_recording`: this build of the crate serves the D3D9 API side, which
    // records each frame directly into the wire command stream. Every PE target
    // is one; so is a native host build, where the API side and the native
    // runtime share one copy of this crate. The host tests and the Wine-hosted
    // unix dylib keep the owned-operation path instead.
    println!("cargo::rustc-check-cfg=cfg(pe_recording)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" || std::env::var_os("CARGO_FEATURE_NATIVE_HOST").is_some() {
        println!("cargo:rustc-cfg=pe_recording");
    }
    println!("cargo::rustc-check-cfg=cfg(perf_tracking)");
    println!("cargo:rerun-if-env-changed=MTLD3D_PERF");
    if std::env::var("MTLD3D_PERF").is_ok_and(|v| !v.is_empty() && v != "0") {
        println!("cargo:rustc-cfg=perf_tracking");
    }
}

// Paths and lengths delimit the hash input, independent of checkout path and target.
fn emitter_fingerprint() {
    let mut paths = Vec::new();
    collect_sources(Path::new("src/dxso"), &mut paths);
    paths.extend([
        "src/dxso.rs".into(),
        "src/vs_draw.rs".into(),
        "src/ps_draw.rs".into(),
        "../../unix/shared/src/mtl.rs".into(),
        "../../unix/shared/src/shader_create.rs".into(),
    ]);
    paths.sort();
    let mut hash = Xxh3::new();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.to_str().expect("source path is UTF-8");
        let bytes = fs::read(&path).expect("read shader emission source");
        hash.write(&(name.len() as u64).to_le_bytes());
        hash.write(name.as_bytes());
        hash.write(&(bytes.len() as u64).to_le_bytes());
        hash.write(&bytes);
    }
    let out = std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR");
    fs::write(
        Path::new(&out).join("emitter_version.rs"),
        format!("u64::from_le_bytes({:?})", hash.finish().to_le_bytes()),
    )
    .expect("write shader emitter fingerprint");
}

fn collect_sources(dir: &Path, paths: &mut Vec<std::path::PathBuf>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir).expect("read shader source directory") {
        let path = entry.expect("shader source entry").path();
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .expect("source name");
        if name == "tests" || name.ends_with("_tests") {
            continue;
        }
        if path.is_dir() {
            collect_sources(&path, paths);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            paths.push(path);
        }
    }
}
