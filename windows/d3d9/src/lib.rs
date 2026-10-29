#[cfg(all(windows, any(target_arch = "aarch64", target_arch = "arm64ec")))]
mod arm64_crt;
mod bound_buffers;
mod bound_rt;
mod capture;
mod com_ref;
mod config;
#[cfg(windows)]
mod crash;
#[cfg(all(windows, mtld3d_crumb))]
mod crumb_allocator;
mod cursor;
mod device;
mod direct3d9;
#[cfg(windows)]
mod dll;
mod draw;
mod encoder;
#[cfg(windows)]
mod exit_code_hook;
mod fpu;
mod fullscreen;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
mod guest_mem;
#[macro_use]
mod hookable;
#[cfg(not(windows))]
pub mod host;
#[cfg(windows)]
mod import_patch;
mod index_buffer;
#[cfg(windows)]
mod log_sink;
#[cfg(windows)]
mod mode_list_hook;
mod page_box_pool;
mod pixel_shader;
mod private_data;
mod query;
mod shader_bindings;
mod shader_validator;
mod stage_bindings;
mod state_block;
mod surface;
mod swapchain;
mod texture;
mod unix_call;
mod vertex_buffer;
mod vertex_decl;
mod vertex_shader;
#[cfg(windows)]
mod wine_path;

use core::{ffi::c_void, sync::atomic::AtomicBool};
use std::sync::Arc;

// HRESULT codes live in `mtld3d_types` (shared with the integration-test
// harness); re-exported under the crate root so every in-crate
// `use super::{D3D_OK, …}` path stays valid.
use mtld3d_types::{
    D3D_OK, D3DERR_INVALIDCALL, D3DERR_NOTAVAILABLE, D3DERR_NOTFOUND, E_FAIL, E_NOINTERFACE,
    S_FALSE,
};

use crate::direct3d9::Direct3D9;
#[cfg(windows)]
pub use crate::dll::pin_image;
#[cfg(not(windows))]
pub use crate::host::pin_image;
#[cfg(not(windows))]
use crate::host::{crash, log_sink, wine_path};

/// Single source of truth for the `log` crate target, so callers don't hard-code the string.
///
/// Every `log!(target: LOG_TARGET, ...)` site in the crate uses it. Lives
/// under the project-wide `mtld3d::*` root so `RUST_LOG=mtld3d=...` flips the
/// whole project; `RUST_LOG=mtld3d::d3d9=...` scopes to the COM layer.
const LOG_TARGET: &str = "mtld3d::d3d9";

// The baseline this replaces is not a modern libc malloc. With no global
// allocator, i686-pc-windows-msvc routes every allocation to HeapAlloc,
// which under Wine is RtlAllocateHeap: one shared heap lock across the
// API and encoder threads, and a per-block virtual mapping for large
// blocks, paid from emulated x86 crossing the PE/unix boundary. snmalloc
// serves from per-thread slabs and never calls HeapAlloc at any size —
// it takes address space from VirtualAlloc directly.
//
// Its lock-free remote-free ring also batches cross-thread frees, though
// not for PageBox: the ring's budget is 16 KiB and a PageBox is at least
// one 16 KiB page, so every box posts on its own. The batching earns its
// keep on the smaller traffic crossing the same boundary (Arc control
// blocks, boxed closures, FrameData internals).
//
// mimalloc is not usable here: its cross-thread free path faults on
// 16 KiB-aligned PageBox allocations.
//
// Under `cfg(mtld3d_crumb)` the allocator is swapped for
// `crumb_allocator::CrumbAllocator` — a thin `SnMalloc` wrapper that
// records PageBox-shape alloc/dealloc events into the shared crash
// breadcrumb. Production builds get plain `SnMalloc` with zero overhead.
//
// A native host build links this crate into one library with the native
// runtime, whose global allocator serves both.
#[cfg(all(windows, not(mtld3d_crumb)))]
#[global_allocator]
static ALLOCATOR: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;
#[cfg(all(windows, mtld3d_crumb))]
#[global_allocator]
static ALLOCATOR: crate::crumb_allocator::CrumbAllocator = crate::crumb_allocator::CrumbAllocator;

/// Set to true when the game calls `IDirect3D9::CreateDevice`.
///
/// Latched on entry to the call, before argument validation, so a rejected
/// `CreateDevice` still counts as the game having reached for a device. The
/// first latch also pins this image ([`pin_image`]), so from then on no
/// `FreeLibrary` unloads it and the only `DLL_PROCESS_DETACH` still to come is
/// the one at process exit.
///
/// `DllMain`'s detach handler reads it to choose between ending the process
/// and unhooking for an unload. Wine's loader passes `lpvReserved` as 0 when
/// the detach comes from `FreeLibrary` and as 1 when it comes from process
/// exit, the contract Windows documents, but the pin already makes every
/// detach after a device a process exit, so the flag alone decides.
/// `Direct3DCreate9` is too early a signal: launcher and mod DLLs commonly
/// probe-call it to verify the export resolves, then `FreeLibrary`, and that
/// unload has to stay real. `CreateDevice` requires a real HWND and
/// presentation parameters, so reaching it implies actual use.
pub static USED: AtomicBool = AtomicBool::new(false);

// The exports of the d3d9 API. Each is an entry with a fixed prologue that hook
// engines relocate (`hookable.rs`), and jumps to the function after `=`.

hookable_export! {
    /// `Direct3DCreate9`: an `IDirect3D9`.
    #[must_use]
    "Direct3DCreate9" => fn direct3d_create9(sdk_version: u32) -> *mut c_void = create9;
}

hookable_export! {
    /// `Direct3DCreate9Ex`: an `IDirect3D9Ex`, the factory of extended devices.
    ///
    /// The interface is the one `Direct3DCreate9` makes with its extended flag
    /// set: it answers `IID_IDirect3D9Ex`, and every device it creates, through
    /// `CreateDeviceEx` or plain `CreateDevice`, is an extended device. A null
    /// out slot is `D3DERR_INVALIDCALL` and creates nothing.
    "Direct3DCreate9Ex" => fn direct3d_create9_ex(sdk_version: u32, out: *mut *mut c_void) -> i32 = create9_ex;
}

hookable_export! {
    /// `Direct3DShaderValidatorCreate9`: the shader validator interface.
    #[must_use]
    "Direct3DShaderValidatorCreate9" => fn direct3d_shader_validator_create9() -> *mut c_void = shader_validator_create9;
}

// The `D3DPERF_*` family: PIX event markers a game emits around its draw
// groups. Without a profiler attached the real d3d9.dll does nothing and
// reports no nesting, no repeat-frame request and no attached tool, which is
// the complete behaviour here too. They are exported because engines resolve
// the whole family by name in one table and treat a missing entry as a broken
// d3d9.dll: an in-game overlay SDK refuses to initialise when any of them
// resolves to null, even though the game itself renders fine.

hookable_export! {
    /// `D3DPERF_BeginEvent`: opens a PIX event; returns the nesting level.
    ///
    /// Logged once so a game that emits PIX markers is visible in triage; the
    /// markers are not forwarded to a Metal capture.
    "D3DPERF_BeginEvent" => fn d3dperf_begin_event(color: u32, name: *const u16) -> i32 = perf_begin_event;
}

hookable_export! {
    /// `D3DPERF_EndEvent`: closes a PIX event; returns the nesting level.
    #[must_use]
    "D3DPERF_EndEvent" => const fn d3dperf_end_event() -> i32 = perf_end_event;
}

hookable_export! {
    /// `D3DPERF_SetMarker`: a single PIX marker, not forwarded.
    "D3DPERF_SetMarker" => const fn d3dperf_set_marker(color: u32, name: *const u16) = perf_set_marker;
}

hookable_export! {
    /// `D3DPERF_SetRegion`: a PIX region marker, not forwarded.
    "D3DPERF_SetRegion" => const fn d3dperf_set_region(color: u32, name: *const u16) = perf_set_region;
}

hookable_export! {
    /// `D3DPERF_QueryRepeatFrame`: `FALSE`, no profiler asks for a frame replay.
    #[must_use]
    "D3DPERF_QueryRepeatFrame" => const fn d3dperf_query_repeat_frame() -> i32 = perf_query_repeat_frame;
}

hookable_export! {
    /// `D3DPERF_SetOptions`: profiler permission flags, nothing to apply them to.
    "D3DPERF_SetOptions" => const fn d3dperf_set_options(options: u32) = perf_set_options;
}

hookable_export! {
    /// `D3DPERF_GetStatus`: `0`, no profiler attached.
    #[must_use]
    "D3DPERF_GetStatus" => const fn d3dperf_get_status() -> u32 = perf_get_status;
}

extern "system" fn create9(_sdk_version: u32) -> *mut c_void {
    create_interface(false).cast::<c_void>()
}

extern "system" fn create9_ex(_sdk_version: u32, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        mtld3d_shared::log_once_warn!(
            target: LOG_TARGET,
            "reject Direct3DCreate9Ex: null out slot → INVALIDCALL"
        );
        return D3DERR_INVALIDCALL;
    }
    write_out(out, create_interface(true).cast::<c_void>());
    log::info!(target: LOG_TARGET, "Direct3DCreate9Ex: extended interface created");
    D3D_OK
}

/// Create the interface both factory exports hand out, `extended` for `Direct3DCreate9Ex`.
fn create_interface(extended: bool) -> *mut Direct3D9 {
    // Resolves `mtld3d.conf` for this interface and logs the option set. The
    // log location it names reaches the unix side before the logging thread
    // starts, so every line queued since `DllMain` lands in the file; a later
    // interface names it again and the unix side keeps the file it has.
    let cfg = config::load();
    log_sink::open(&cfg);
    // The page-box pool is one per process; the interface resolved most
    // recently sizes it.
    page_box_pool::PAGEBOX_POOL
        .set_cap(usize::try_from(cfg.pagebox_pool_cap_bytes).unwrap_or(usize::MAX));
    let interface = Box::into_raw(Box::new(Direct3D9::new(Arc::new(cfg), extended)));
    // The first entry point outside `DllMain`: the logging thread can start
    // here (DllMain runs under the loader lock and must not spawn threads).
    // It runs until the last interface is released, holding this image. Last,
    // so the lines the interface's creation logged are among those `acquire`
    // forwards before it returns.
    log_sink::acquire();
    interface
}

extern "system" fn shader_validator_create9() -> *mut c_void {
    shader_validator::create()
}

extern "system" fn perf_begin_event(_color: u32, _name: *const u16) -> i32 {
    mtld3d_shared::log_once_info!(
        target: LOG_TARGET,
        "D3DPERF_BeginEvent: PIX event markers not forwarded (no profiler)"
    );
    0
}

const extern "system" fn perf_end_event() -> i32 {
    0
}

const extern "system" fn perf_set_marker(_color: u32, _name: *const u16) {}

const extern "system" fn perf_set_region(_color: u32, _name: *const u16) {}

const extern "system" fn perf_query_repeat_frame() -> i32 {
    0
}

const extern "system" fn perf_set_options(_options: u32) {}

const extern "system" fn perf_get_status() -> u32 {
    0
}
/// Null a COM `**out` parameter before returning a failing HRESULT.
///
/// Callers that ignore the HRESULT and read the out-pointer get `null`
/// instead of stack garbage, which would otherwise read as a bogus COM
/// `this` pointer.
fn null_out(out: *mut *mut c_void) {
    write_out(out, core::ptr::null_mut());
}

/// Write a COM `**out` parameter, the success counterpart of [`null_out`].
///
/// A private helper so the exported entry point that calls it stays a safe
/// function; `out` is null or a writable slot per the COM ABI.
fn write_out(out: *mut *mut c_void, value: *mut c_void) {
    // SAFETY: per the COM ABI `out` is null, which `write_opt` skips, or a
    // writable `*mut c_void` slot owned by the caller.
    unsafe { mtld3d_shared::OutPtr::write_opt(out, value) };
}
