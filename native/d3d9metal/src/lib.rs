//! `libd3d9metal`: the D3D9 API side and the native runtime linked in process, with no Wine.
//!
//! The Wine build has three linkage units: `d3d9.dll` (COM, state capture and
//! application-visible memory), `mtld3d.dll` (the shim that owns Wine's
//! unix-call entry) and `mtld3d.so` (Metal). This crate is the shim of the
//! native host build: it links the other two as libraries, defines the
//! `mtld3d_unix_call` symbol `d3d9` calls, which runs the native runtime's
//! dispatcher on the calling thread with the same thunk codes and parameter
//! records, and exports the C API of `native/include/d3d9metal.h`.

use core::ffi::c_void;

use mtld3d_core::native_host::{self, InstallError, NativeHost};
use mtld3d_shared::Thunks;

/// `d3d9metal_init` succeeded.
const D3D9METAL_OK: i32 = 0;
/// `d3d9metal_init` was handed a null table.
const D3D9METAL_E_NULL: i32 = -1;
/// The table is too short or misses a required callback.
const D3D9METAL_E_INVALID: i32 = -2;
/// A table is installed already.
const D3D9METAL_E_ALREADY: i32 = -3;

/// `d3d9metal_init`: install the host's callbacks, then attach the API side.
///
/// # Safety
///
/// `host` is null or points at a `D3D9MetalHost` whose `size` bytes are
/// readable and whose callbacks and pointers keep the contract the header
/// states for the rest of the process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn d3d9metal_init(host: *const NativeHost) -> i32 {
    if host.is_null() {
        return D3D9METAL_E_NULL;
    }
    // SAFETY: the caller's contract, which `install` restates.
    match unsafe { native_host::install(host) } {
        Ok(()) => {}
        Err(InstallError::TooShort | InstallError::MissingCallback) => return D3D9METAL_E_INVALID,
        Err(InstallError::AlreadyInstalled) => return D3D9METAL_E_ALREADY,
    }
    d3d9::host::attach();
    D3D9METAL_OK
}

/// `d3d9metal_Direct3DCreate9`: an `IDirect3D9`, or null before `d3d9metal_init`.
#[unsafe(export_name = "d3d9metal_Direct3DCreate9")]
#[must_use]
pub extern "C" fn direct3d_create9(sdk_version: u32) -> *mut c_void {
    if native_host::get().is_none() {
        return core::ptr::null_mut();
    }
    d3d9::direct3d_create9(sdk_version)
}

/// The native runtime's dispatcher, called in process where Wine's would be.
///
/// Every thunk but the logger's runs between the host's `blocking_begin` and
/// `blocking_end`: a thunk is where the API side waits (backpressure at
/// `Present`, the frame-latency bound, read-backs, `Reset`, teardown), and
/// thunks are rare enough (one per frame on the ordinary path) that the two
/// calls cost nothing measurable.
///
/// # Safety
///
/// `args` is the parameter record of the thunk `code` names, live and
/// exclusively borrowed for the call: what `d3d9`'s `unix_call` passes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mtld3d_unix_call(code: u32, args: *mut c_void) -> i32 {
    let logger = code == Thunks::WriteLog as u32
        || code == Thunks::InitLogger as u32
        || code == Thunks::OpenLog as u32;
    let host = native_host::get().filter(|_| !logger);
    if let Some(host) = host {
        host.blocking_begin();
    }
    // SAFETY: the caller's contract is the dispatcher's.
    let status = unsafe { mtld3d_unix::native_unix_call(code, args) };
    if let Some(host) = host {
        host.blocking_end();
    }
    status
}
