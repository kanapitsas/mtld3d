//! The Win32 calls of the platform-neutral modules, answered for a window the native host owns.
//!
//! On Windows the device, cursor and fullscreen code reach user32, gdi32,
//! kernel32 and advapi32 through a few concentrated wrappers. A native host
//! build imports these functions under the Win32 names in their place, so
//! those modules compile unchanged. A window handle is the host's own opaque
//! pointer, and the one thing the layer can learn about it is what the host's
//! `metal_layer` callback answers: whether there is a layer behind it, and
//! the layer's size in pixels. Everything else has the answer a window with
//! no Win32 side gives:
//!
//! - No window procedure runs, so no message is posted, subclassed or sent,
//!   and nothing the layer would do in one (auto-resize on `WM_SIZE`, the
//!   fullscreen repairs) happens. A host resizes by calling `Reset`.
//! - The cursor is the host's: no cursor is created, set or shown.
//! - The display is the host's: the modes are the host's list
//!   (`mtld3d_core::native_host::NativeHost::display_modes`), the current mode
//!   is the first of them, and a mode change is accepted without changing
//!   anything.
//! - The DPI is 96 and the windows are per-monitor aware, so nothing scales.
//! - No GDI device context and no D3DKMT adapter exist, so `GetDC` on a
//!   surface fails and the adapter LUID is one allocated for the process.
//!
//! Every function is `unsafe` with the contract of the Win32 function it
//! stands in for, so a call site reads the same on both builds.

use core::{
    ffi::c_void,
    sync::atomic::{AtomicU32, Ordering},
};

use mtld3d_types::{CURSORINFO, ICONINFO, LUID, POINT};

use crate::{
    LOG_TARGET,
    fullscreen::{
        DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DevModeW,
        ENUM_CURRENT_SETTINGS, ENUM_REGISTRY_SETTINGS, MonitorInfo, Rect,
    },
};

/// `STATUS_NOT_SUPPORTED`, what the D3DKMT stand-ins answer.
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
/// `DPI_AWARENESS_PER_MONITOR_AWARE`: nothing scales the host's windows.
const DPI_AWARENESS_PER_MONITOR_AWARE: i32 = 2;
/// `USER_DEFAULT_SCREEN_DPI`.
const DEFAULT_DPI: u32 = 96;
/// `MONITORINFOF_PRIMARY`.
const MONITORINFOF_PRIMARY: u32 = 1;
/// `DISP_CHANGE_SUCCESSFUL`.
const DISP_CHANGE_SUCCESSFUL: i32 = 0;
/// The one monitor's handle: any non-null value, never dereferenced.
const PRIMARY_MONITOR: usize = 1;

/// Log once that a Win32 call was answered for a window with no Win32 side.
macro_rules! stub {
    ($name:literal) => {
        mtld3d_shared::log_once_info!(
            target: LOG_TARGET,
            concat!("native host: ", $name, " has no Win32 counterpart; answered as for a window the host owns"),
        )
    };
}

/// The size of the host's layer behind `hwnd`, `None` with no host or no layer.
fn layer_size(hwnd: *mut c_void) -> Option<(u32, u32)> {
    let host = mtld3d_core::native_host::get()?;
    host.metal_layer(hwnd.addr())
        .map(|(_, width, height)| (width, height))
}

/// The rect of a `width` x `height` area at the origin.
fn origin_rect(width: u32, height: u32) -> Rect {
    Rect {
        left: 0,
        top: 0,
        right: i32::try_from(width).unwrap_or(i32::MAX),
        bottom: i32::try_from(height).unwrap_or(i32::MAX),
    }
}

/// The host's display modes, the first being the current one.
fn display_modes() -> Vec<(u32, u32, u32)> {
    mtld3d_core::native_host::get().map_or_else(
        || vec![mtld3d_core::native_host::DEFAULT_MODE],
        mtld3d_core::native_host::NativeHost::display_modes,
    )
}

/// `GetClientRect`: the host layer's size.
///
/// # Safety
///
/// `rect` is writable, as `GetClientRect` requires.
pub unsafe fn get_client_rect(hwnd: *mut c_void, rect: *mut Rect) -> i32 {
    let Some((width, height)) = layer_size(hwnd) else {
        return 0;
    };
    // SAFETY: the caller's contract; no alignment is assumed.
    unsafe { rect.write_unaligned(origin_rect(width, height)) };
    1
}

/// `GetWindowRect`: a host window has no frame, so its rect is its client rect.
///
/// # Safety
///
/// `rect` is writable, as `GetWindowRect` requires.
pub unsafe fn get_window_rect(hwnd: *mut c_void, rect: *mut Rect) -> i32 {
    // SAFETY: the same contract.
    unsafe { get_client_rect(hwnd, rect) }
}

/// `IsWindow`: whether the host has a layer behind the handle.
///
/// # Safety
///
/// None beyond `IsWindow`'s, which accepts any handle.
pub unsafe fn is_window(hwnd: *mut c_void) -> i32 {
    i32::from(layer_size(hwnd).is_some())
}

/// `IsZoomed`: a host window is never maximised by the layer's reckoning.
///
/// # Safety
///
/// None.
pub unsafe fn is_zoomed(_hwnd: *mut c_void) -> i32 {
    stub!("IsZoomed");
    0
}

/// `GetWindowLongW`: no window styles.
///
/// # Safety
///
/// None.
pub unsafe fn get_window_long_w(_hwnd: *mut c_void, _index: i32) -> i32 {
    stub!("GetWindowLongW");
    0
}

/// `SetWindowLongPtrW`: nothing to set; reports a previous value of 0.
///
/// # Safety
///
/// None.
pub unsafe fn set_window_long_ptr_w(_hwnd: *mut c_void, _index: i32, _value: isize) -> isize {
    stub!("SetWindowLongPtrW");
    0
}

/// `SetWindowPos`: the host places its own windows; accepted without effect.
///
/// # Safety
///
/// None.
pub unsafe fn set_window_pos(
    _hwnd: *mut c_void,
    _insert_after: *mut c_void,
    _x: i32,
    _y: i32,
    _cx: i32,
    _cy: i32,
    _flags: u32,
) -> i32 {
    stub!("SetWindowPos");
    1
}

/// `GetClassLongPtrW`: no window class.
///
/// # Safety
///
/// None.
pub unsafe fn get_class_long_ptr_w(_hwnd: *mut c_void, _index: i32) -> usize {
    stub!("GetClassLongPtrW");
    0
}

/// `CallWindowProcW`: never reached, since no window procedure is installed.
///
/// # Safety
///
/// None.
pub unsafe fn call_window_proc_w(
    _prev: *mut c_void,
    _hwnd: *mut c_void,
    _msg: u32,
    _wp: usize,
    _lp: isize,
) -> isize {
    stub!("CallWindowProcW");
    0
}

/// `DefWindowProcW`: no default handling.
///
/// # Safety
///
/// None.
pub unsafe fn def_window_proc_w(_hwnd: *mut c_void, _msg: u32, _wp: usize, _lp: isize) -> isize {
    stub!("DefWindowProcW");
    0
}

/// `PostMessageW`: no message queue, so the post fails.
///
/// # Safety
///
/// None.
pub unsafe fn post_message_w(_hwnd: *mut c_void, _msg: u32, _wp: usize, _lp: isize) -> i32 {
    stub!("PostMessageW");
    0
}

/// `GetForegroundWindow`: unknown to the layer.
///
/// # Safety
///
/// None.
pub unsafe fn get_foreground_window() -> *mut c_void {
    stub!("GetForegroundWindow");
    core::ptr::null_mut()
}

/// `GetWindowThreadProcessId`: the calling thread, so the layer treats every window as its own.
///
/// # Safety
///
/// `process_id` is null or writable, as the Win32 call requires.
pub unsafe fn get_window_thread_process_id(_hwnd: *mut c_void, process_id: *mut u32) -> u32 {
    if !process_id.is_null() {
        // SAFETY: the caller's contract; no alignment is assumed.
        unsafe { process_id.write_unaligned(std::process::id()) };
    }
    // SAFETY: no precondition.
    unsafe { get_current_thread_id() }
}

/// `GetCurrentThreadId`: a nonzero id per thread, stable for the thread's life.
///
/// # Safety
///
/// None.
#[must_use]
pub unsafe fn get_current_thread_id() -> u32 {
    /// The next id to hand out; ids are only compared, never resolved.
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! {
        static ID: u32 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    ID.with(|id| *id)
}

/// `GetLastError`: no Win32 error state.
///
/// # Safety
///
/// None.
pub unsafe fn get_last_error() -> u32 {
    stub!("GetLastError");
    0
}

/// `GetCurrentProcess`: the pseudo-handle, which nothing dereferences.
///
/// # Safety
///
/// None.
pub unsafe fn get_current_process() -> *mut c_void {
    stub!("GetCurrentProcess");
    core::ptr::null_mut::<c_void>().wrapping_sub(1)
}

/// `SetCursor`: the host owns the cursor; reports no previous one.
///
/// # Safety
///
/// None.
pub unsafe fn set_cursor(_cursor: *mut c_void) -> *mut c_void {
    stub!("SetCursor");
    core::ptr::null_mut()
}

/// `SetCursorPos`: the host owns the cursor; accepted without effect.
///
/// # Safety
///
/// None.
pub unsafe fn set_cursor_pos(_x: i32, _y: i32) -> i32 {
    stub!("SetCursorPos");
    1
}

/// `GetCursorPos`: the origin.
///
/// # Safety
///
/// `point` is writable, as `GetCursorPos` requires.
pub unsafe fn get_cursor_pos(point: *mut POINT) -> i32 {
    stub!("GetCursorPos");
    // SAFETY: the caller's contract; no alignment is assumed.
    unsafe { point.write_unaligned(POINT { x: 0, y: 0 }) };
    1
}

/// `GetCursor`: no cursor.
///
/// # Safety
///
/// None.
pub unsafe fn get_cursor() -> *mut c_void {
    stub!("GetCursor");
    core::ptr::null_mut()
}

/// `GetCursorInfo`: fails, the cursor being the host's.
///
/// # Safety
///
/// None.
pub unsafe fn get_cursor_info(_info: *mut CURSORINFO) -> i32 {
    stub!("GetCursorInfo");
    0
}

/// `CreateIconIndirect`: no cursor is created.
///
/// # Safety
///
/// None.
pub unsafe fn create_icon_indirect(_info: *const ICONINFO) -> *mut c_void {
    stub!("CreateIconIndirect");
    core::ptr::null_mut()
}

/// `DestroyCursor`: nothing was created to destroy.
///
/// # Safety
///
/// None.
pub unsafe fn destroy_cursor(_cursor: *mut c_void) -> i32 {
    stub!("DestroyCursor");
    1
}

/// `LoadCursorW`: no system cursors.
///
/// # Safety
///
/// None.
pub unsafe fn load_cursor_w(_instance: *mut c_void, _name: *const u16) -> *mut c_void {
    stub!("LoadCursorW");
    core::ptr::null_mut()
}

/// `CreateBitmap`: no GDI.
///
/// # Safety
///
/// None.
pub unsafe fn create_bitmap(
    _width: i32,
    _height: i32,
    _planes: u32,
    _bpp: u32,
    _bits: *const c_void,
) -> *mut c_void {
    stub!("CreateBitmap");
    core::ptr::null_mut()
}

/// `DeleteObject`: nothing was created to delete.
///
/// # Safety
///
/// None.
pub unsafe fn delete_object(_object: *mut c_void) -> i32 {
    stub!("DeleteObject");
    1
}

/// `CreateCompatibleDC`: no GDI.
///
/// # Safety
///
/// None.
pub unsafe fn create_compatible_dc(_dc: *mut c_void) -> *mut c_void {
    stub!("CreateCompatibleDC");
    core::ptr::null_mut()
}

/// `DeleteDC`: nothing was created to delete.
///
/// # Safety
///
/// None.
pub unsafe fn delete_dc(_dc: *mut c_void) -> i32 {
    stub!("DeleteDC");
    1
}

/// `D3DKMTCreateDCFromMemory`: no kernel-mode display driver.
///
/// # Safety
///
/// None.
pub unsafe fn d3dkmt_create_dc_from_memory<T>(_desc: *mut T) -> u32 {
    stub!("D3DKMTCreateDCFromMemory");
    STATUS_NOT_SUPPORTED
}

/// `D3DKMTDestroyDCFromMemory`: no kernel-mode display driver.
///
/// # Safety
///
/// None.
pub unsafe fn d3dkmt_destroy_dc_from_memory<T>(_desc: *const T) -> u32 {
    stub!("D3DKMTDestroyDCFromMemory");
    STATUS_NOT_SUPPORTED
}

/// `D3DKMTOpenAdapterFromGdiDisplayName`: no kernel-mode display driver.
///
/// # Safety
///
/// None.
pub unsafe fn d3dkmt_open_adapter_from_gdi_display_name<T>(_desc: *mut T) -> u32 {
    stub!("D3DKMTOpenAdapterFromGdiDisplayName");
    STATUS_NOT_SUPPORTED
}

/// `D3DKMTCloseAdapter`: no kernel-mode display driver.
///
/// # Safety
///
/// None.
pub unsafe fn d3dkmt_close_adapter<T>(_desc: *const T) -> u32 {
    stub!("D3DKMTCloseAdapter");
    STATUS_NOT_SUPPORTED
}

/// `AllocateLocallyUniqueId`: a process-unique LUID.
///
/// # Safety
///
/// `luid` is writable, as the Win32 call requires.
pub unsafe fn allocate_locally_unique_id(luid: *mut LUID) -> i32 {
    /// The next LUID's low part; starts above the reserved well-known LUIDs.
    static NEXT: AtomicU32 = AtomicU32::new(0x1000);
    let low_part = NEXT.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the caller's contract; no alignment is assumed.
    unsafe {
        luid.write_unaligned(LUID {
            low_part,
            high_part: 0,
        });
    }
    1
}

/// `GetAsyncKeyState`: the keyboard is the host's; no key reads as down.
///
/// # Safety
///
/// None.
pub unsafe fn get_async_key_state(_vkey: i32) -> i16 {
    stub!("GetAsyncKeyState");
    0
}

/// `MonitorFromPoint`: the one monitor.
///
/// # Safety
///
/// None.
pub unsafe fn monitor_from_point<P>(_point: P, _flags: u32) -> *mut c_void {
    core::ptr::without_provenance_mut(PRIMARY_MONITOR)
}

/// `GetMonitorInfoW`: the current mode's rect, as the primary monitor.
///
/// # Safety
///
/// `info` is writable, as `GetMonitorInfoW` requires.
pub unsafe fn get_monitor_info_w(_monitor: *mut c_void, info: *mut MonitorInfo) -> i32 {
    let Some(&(width, height, _)) = display_modes().first() else {
        return 0;
    };
    let rect = origin_rect(width, height);
    // SAFETY: the caller's contract; no alignment is assumed.
    let mut value = unsafe { info.read_unaligned() };
    value.rc_monitor = rect;
    value.rc_work = rect;
    value.dw_flags = MONITORINFOF_PRIMARY;
    // SAFETY: as above.
    unsafe { info.write_unaligned(value) };
    1
}

/// `EnumDisplaySettingsW`: the host's modes; the current and registry modes are the first.
///
/// # Safety
///
/// `dev_mode` is writable, as `EnumDisplaySettingsW` requires.
pub unsafe fn enum_display_settings_w(
    _device_name: *const u16,
    mode_num: u32,
    dev_mode: *mut DevModeW,
) -> i32 {
    let modes = display_modes();
    let index = if mode_num == ENUM_CURRENT_SETTINGS || mode_num == ENUM_REGISTRY_SETTINGS {
        0
    } else {
        usize::try_from(mode_num).unwrap_or(usize::MAX)
    };
    let Some(&(width, height, refresh_hz)) = modes.get(index) else {
        return 0;
    };
    // SAFETY: the caller's contract; no alignment is assumed.
    let mut value = unsafe { dev_mode.read_unaligned() };
    value.pels_width = width;
    value.pels_height = height;
    value.display_frequency = refresh_hz;
    value.bits_per_pel = 32;
    value.fields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL;
    // SAFETY: as above.
    unsafe { dev_mode.write_unaligned(value) };
    1
}

/// `ChangeDisplaySettingsW`: the host owns the display; accepted without changing it.
///
/// # Safety
///
/// None.
pub unsafe fn change_display_settings_w(_dev_mode: *mut DevModeW, _flags: u32) -> i32 {
    mtld3d_shared::log_once_info!(
        target: LOG_TARGET,
        "ChangeDisplaySettings on a native host: the host owns the display mode, accepted without change"
    );
    DISP_CHANGE_SUCCESSFUL
}

/// `GetSystemDpiForProcess`: 96.
///
/// # Safety
///
/// None.
pub unsafe fn get_system_dpi_for_process(_process: *mut c_void) -> u32 {
    stub!("GetSystemDpiForProcess");
    DEFAULT_DPI
}

/// `GetWindowDpiAwarenessContext`: no context.
///
/// # Safety
///
/// None.
pub unsafe fn get_window_dpi_awareness_context(_hwnd: *mut c_void) -> *mut c_void {
    stub!("GetWindowDpiAwarenessContext");
    core::ptr::null_mut()
}

/// `GetAwarenessFromDpiAwarenessContext`: per-monitor aware.
///
/// # Safety
///
/// None.
pub unsafe fn get_awareness_from_dpi_awareness_context(_context: *mut c_void) -> i32 {
    stub!("GetAwarenessFromDpiAwarenessContext");
    DPI_AWARENESS_PER_MONITOR_AWARE
}
