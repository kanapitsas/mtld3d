//! The life of `d3d9.dll` as a Windows image: `DllMain`, the loader pin, the PE logger.
//!
//! A native host build has no image of its own to load or pin, and shares the
//! native runtime's logger; `crate::host` is its counterpart.

use core::{ffi::c_void, sync::atomic::Ordering};

use mtld3d_shared::{InitLoggerParams, identity};

#[cfg(any(target_arch = "aarch64", target_arch = "arm64ec"))]
use crate::arm64_crt;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use crate::guest_mem;
use crate::{
    LOG_TARGET, USED,
    crash::{
        self, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
        GetModuleHandleExA,
    },
    exit_code_hook, log_sink, mode_list_hook,
    unix_call::unix_call,
};

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;
/// The reason a thread's exit gives a TLS callback, which only the ARM64X halves install.
#[cfg(any(target_arch = "aarch64", target_arch = "arm64ec"))]
pub const DLL_THREAD_DETACH: u32 = 3;

unsafe extern "system" {
    fn DisableThreadLibraryCalls(lib_module: *mut c_void) -> i32;
    fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

#[unsafe(export_name = "DllMain")]
pub extern "system" fn dll_main(instance: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_DETACH && USED.load(Ordering::Relaxed) {
        // The process is exiting. A device was created, which pinned the
        // image, so no `FreeLibrary` reaches this detach unless the pin
        // failed, which was logged; only process exit does (Wine passes
        // `_reserved` as 1 here and as 0 for the detach of a
        // `FreeLibrary`). Skip every remaining destructor on the calling
        // thread: snmalloc's C++ thread_local teardown walks pools deeply
        // enough to overflow the 1 MB Wine main-thread stack and Wine then
        // aborts exception dispatch, hanging the process. TerminateProcess
        // is the only call that skips DLL_PROCESS_DETACH and TLS callbacks
        // while naming an exit code; ExitProcess / std::process::exit run
        // them, abort uses fast-fail. Its code is the one the unix side of
        // Wine exits with, so it carries the status the process asked to
        // exit with rather than a zero that would hide a failing run from a
        // unix parent.
        let status = exit_code_hook::status();
        // SAFETY: Win32 GetCurrentProcess returns a pseudo-handle for the
        // current process; passing it to TerminateProcess is the documented
        // self-exit form.
        let proc = unsafe { GetCurrentProcess() };
        // SAFETY: pseudo-handle to current process, with the status the
        // process asked to exit with.
        unsafe { TerminateProcess(proc, status) };
    }
    if reason == DLL_PROCESS_DETACH {
        // A FreeLibrary before any device, which the process survives (or
        // process exit before any device): take the process-wide pointers
        // into this image down with it.
        crash::uninstall();
        mode_list_hook::uninstall();
        exit_code_hook::uninstall();
    }
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "arm64ec"))]
    arm64_crt::attach();
    init_logger(instance);
    attach_process(instance);
    1
}

/// Keep this image loaded until the process ends, called once at the first `CreateDevice`.
///
/// A pinned module ignores `FreeLibrary`, so a launcher that creates a
/// device, releases it and frees `d3d9.dll` keeps running with the image
/// mapped, and the next `LoadLibrary` finds it already there. Unloading for
/// real after a device existed would run the allocator's thread-local
/// teardown on the caller's thread and leave process-wide registrations
/// (window subclasses, notification observers, Metal completion handlers)
/// pointing into unmapped code. The pin waits for a device rather than
/// happening at load, because a load that only probes `Direct3DCreate9` and
/// frees the library again has to unload for real.
pub fn pin_image() {
    let mut module: *mut c_void = core::ptr::null_mut();
    // SAFETY: kernel32 export; `USED` is a static of this image, so its
    // address names the module, and `out` is a writable local.
    let ok = unsafe {
        GetModuleHandleExA(
            GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            (&raw const USED).cast::<u8>(),
            &raw mut module,
        )
    };
    if ok == 0 || module.is_null() {
        mtld3d_shared::log_once_warn!(
            target: LOG_TARGET,
            "CreateDevice: GetModuleHandleEx could not pin d3d9.dll; a later FreeLibrary that unloads it ends the process"
        );
    }
}

// Wires up the PE-side `env_logger` for this cdylib, then fires a
// one-shot `InitLogger` thunk so the unix .so registers its own
// (each cdylib has its own `log` crate statics). Runs from DllMain
// after mtld3d.dll's DllMain has already wired up the unix-call
// dispatcher — DLL load ordering is guaranteed by d3d9.dll's implicit
// import of `mtld3d_unix_call` from mtld3d.dll.
fn init_logger(instance: *mut c_void) {
    mtld3d_shared::init_logger_to(Box::new(log_sink::Sink));
    log_identity(instance);
    // Latch the d3d9-side perf-tracking gate (`PERF_TRACKING_ENABLED`)
    // from `RUST_LOG`. Per-cdylib because each cdylib has its own
    // `log` statics; the unix side latches its own copy in
    // `init_logger_handler`.
    mtld3d_core::perf::init_tracking_enabled();
    mtld3d_core::state_trace::init_enabled();
    // Map the shared crash crumb (cfg-gated no-op in production) and
    // install the always-on VEH-based crash handler. Both sides write
    // into the same `/tmp/mtld3d-crumb.bin` file so PE+unix events
    // interleave by seq.
    mtld3d_shared::crumb::init();
    crash::install(instance);
    mtld3d_shared::crumb::set_write_sink(log_sink::write_fault);
    let filter = std::env::var("RUST_LOG").unwrap_or_default();
    // Where a crash report goes if one comes before `Direct3DCreate9`
    // names the log location; empty when it cannot be derived.
    let (early_dir, early_stem) = log_sink::early_location().unwrap_or_else(|| {
        mtld3d_shared::log_once_warn!(
            target: LOG_TARGET,
            "log file: no default location at load, a crash before Direct3DCreate9 reports to stderr"
        );
        (String::new(), String::new())
    });
    let mut params = InitLoggerParams {
        filter_ptr: filter.as_ptr() as u64,
        early_dir_ptr: early_dir.as_ptr() as u64,
        early_stem_ptr: early_stem.as_ptr() as u64,
        filter_len: u32::try_from(filter.len()).unwrap_or(0),
        early_dir_len: u32::try_from(early_dir.len()).unwrap_or(0),
        early_stem_len: u32::try_from(early_stem.len()).unwrap_or(0),
        pad0: 0,
    };
    unix_call(&mut params);
}

/// Name this build in the log, as the first line the logger emits.
///
/// [`identity::BUILD`] says which release the source came from; the image ID is
/// the PDB GUID the linker assigned, which names this exact binary and picks
/// the `.pdb` that symbolicates it out of the release's debug archive.
fn log_identity(instance: *mut c_void) {
    // SAFETY: `instance` is the HMODULE the loader passed to `DllMain` during
    // `DLL_PROCESS_ATTACH`, so this image is mapped in full.
    let id = unsafe { identity::image_id(instance) };
    let id = id.as_deref().unwrap_or("no-image-id");
    let build = identity::BUILD;
    let arch = identity::ARCH;
    let base = instance as usize;
    log::info!(target: LOG_TARGET, "d3d9.dll {build} {id} loaded at {base:#x} as {arch}");
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    guest_mem::log_route();
}

/// `DLL_PROCESS_ATTACH` body.
///
/// Private helper so the exported `dll_main` stub stays safe — clippy's
/// `not_unsafe_ptr_arg_deref` only checks `pub` functions.
/// `DisableThreadLibraryCalls` so per-thread `DllMain` notifications don't
/// fire.
fn attach_process(instance: *mut c_void) {
    // SAFETY: `instance` is the HMODULE passed by the loader to `DllMain`
    // during `DLL_PROCESS_ATTACH`; Win32 `DisableThreadLibraryCalls` is
    // safe to call from `DLL_PROCESS_ATTACH` with that module handle.
    unsafe { DisableThreadLibraryCalls(instance) };
    mode_list_hook::install();
    exit_code_hook::install();
}
