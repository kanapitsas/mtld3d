//! The log location of a native host build.
//!
//! The API side and the native runtime share one logger here, so there is no
//! log thread and no line crosses the boundary. What is left is the location:
//! `log.dir` names a directory, relative to the executable's, and without it
//! the lines go to stderr, the host's own stream.

use log::warn;
use mtld3d_shared::OpenLogParams;

use crate::{LOG_TARGET, unix_call::unix_call};

/// Name the log location to the native runtime: `log.dir`, or stderr when it is unset.
pub fn open(cfg: &mtld3d_core::config::Mtld3dConfig) {
    let dir = if cfg.log_dir.is_empty() {
        None
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(&cfg.log_dir)))
            .and_then(|dir| match std::fs::create_dir_all(&dir) {
                Ok(()) => dir.to_str().map(str::to_owned),
                Err(e) => {
                    warn!(target: LOG_TARGET, "log file: cannot create {} ({e}), logging to stderr", dir.display());
                    None
                }
            })
    };
    let dir = dir.unwrap_or_default();
    let stem = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| String::from("mtld3d"));
    let (Ok(dir_len), Ok(stem_len)) = (u32::try_from(dir.len()), u32::try_from(stem.len())) else {
        return;
    };
    let mut params = OpenLogParams {
        dir_ptr: dir.as_ptr() as u64,
        stem_ptr: stem.as_ptr() as u64,
        dir_len,
        stem_len,
        main_thread_checker: 0,
        pad0: 0,
    };
    unix_call(&mut params);
}

/// One more `IDirect3D9`; no log thread to start.
pub const fn acquire() {}

/// One `IDirect3D9` fewer; no log thread to stop.
pub const fn release() {}

/// A native host build has no loader lock.
#[must_use]
pub const fn caller_holds_loader_lock() -> bool {
    false
}
