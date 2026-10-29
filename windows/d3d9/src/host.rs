//! The native host build's side of the platform boundary: what Windows and Wine give `d3d9.dll`.
//!
//! A native host build (`native/`, `docs/ARCHITECTURE.md` "The native host
//! linkage") compiles this crate for the host's own target and links it into
//! one library with the native runtime, so there is no image to load or pin,
//! no loader lock, no Win32 window and no Wine path mapping. The host
//! application installs its callbacks first (`mtld3d_core::native_host`), and
//! `d3d9metal_init` then calls [`attach`] where a PE build runs `DllMain`.
//!
//! The submodules stand in for the Windows modules of the same names, with
//! the same surface the rest of the crate calls: [`crash`] for the
//! address-space walk, [`log_sink`] for the log location, [`wine_path`] for
//! path mapping, and [`win32`] for the user32, gdi32 and kernel32 calls the
//! platform-neutral modules make, answered for a window the host owns.

use mtld3d_shared::{InitLoggerParams, identity};

use crate::{LOG_TARGET, unix_call::unix_call};

pub mod crash;
pub mod log_sink;
pub mod win32;
pub mod wine_path;

/// The `DllMain` attach of a native host build, run once by `d3d9metal_init`.
///
/// The API side and the native runtime share one `log` crate here, so the
/// runtime's logger, which `InitLogger` installs, serves this crate too, and
/// no log thread forwards lines across the boundary.
pub fn attach() {
    let filter = std::env::var("RUST_LOG").unwrap_or_default();
    let mut params = InitLoggerParams {
        filter_ptr: filter.as_ptr() as u64,
        early_dir_ptr: 0,
        early_stem_ptr: 0,
        filter_len: u32::try_from(filter.len()).unwrap_or(0),
        early_dir_len: 0,
        early_stem_len: 0,
        pad0: 0,
    };
    unix_call(&mut params);
    mtld3d_core::perf::init_tracking_enabled();
    mtld3d_core::state_trace::init_enabled();
    let id = identity::image_id();
    let id = id.as_deref().unwrap_or("no-image-id");
    log::info!(
        target: LOG_TARGET,
        "d3d9 {} {id} linked into the native host as {}",
        identity::BUILD,
        identity::ARCH,
    );
}

/// Nothing to pin: the layer is linked into the host and is never unloaded.
pub const fn pin_image() {}
