//! The host application a native build runs inside, in place of Wine.
//!
//! A native host build links the D3D9 API side and the native runtime into one
//! library and calls it in process (`native/`, `docs/ARCHITECTURE.md` "The
//! native host linkage"). What Wine provides to the other builds, the host
//! provides here through one table of callbacks, installed once by
//! `d3d9metal_init` before the first `Direct3DCreate9`:
//!
//! - the `CAMetalLayer` behind a window handle, and the window's size;
//! - the memory the application may dereference, which is every allocation a
//!   [`crate::page_box::PageBox`] makes, since a `Lock` hands out pointers into
//!   one;
//! - a notification around every wait the layer makes on the caller's thread;
//! - the adapter identity, the shader cache directory and the display modes.
//!
//! [`NativeHost`] is the C layout of `D3D9MetalHost` in
//! `native/include/d3d9metal.h`, field for field; the header owns the contract.

use core::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

/// The host's table, installed once and never removed.
///
/// A static on the process-wide argument: one process has one host
/// application, which every interface and device of the process serves, and
/// the page allocator behind every `PageBox` is that host's.
static HOST: AtomicPtr<NativeHost> = AtomicPtr::new(core::ptr::null_mut());

/// `D3D9MetalHost::metal_layer`: the `CAMetalLayer` behind a window, and its size in pixels.
pub type MetalLayerFn = unsafe extern "C" fn(
    user: *mut c_void,
    hwnd: *mut c_void,
    w: *mut u32,
    h: *mut u32,
) -> *mut c_void;
/// `D3D9MetalHost::alloc_app`: memory the application may dereference.
pub type AllocAppFn =
    unsafe extern "C" fn(user: *mut c_void, size: usize, align: usize) -> *mut c_void;
/// `D3D9MetalHost::free_app`: the release of an `alloc_app` block.
pub type FreeAppFn = unsafe extern "C" fn(user: *mut c_void, ptr: *mut c_void, size: usize);
/// `D3D9MetalHost::blocking_begin` and `blocking_end`.
pub type BlockingFn = unsafe extern "C" fn(user: *mut c_void);

/// One display mode a native host reports, `D3D9MetalDisplayMode` in the header.
#[repr(C)]
pub struct NativeDisplayMode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in Hz; 0 is read as 60.
    pub refresh_hz: u32,
}

/// The host's callbacks, the C layout of `D3D9MetalHost`.
///
/// `size` is the byte size of the caller's struct, so a host built against an
/// older header passes a shorter one: [`install`] copies that much into a
/// zeroed table, and every field past it reads as absent.
#[repr(C)]
pub struct NativeHost {
    pub size: u32,
    pub user: *mut c_void,
    pub metal_layer: Option<MetalLayerFn>,
    pub alloc_app: Option<AllocAppFn>,
    pub free_app: Option<FreeAppFn>,
    pub blocking_begin: Option<BlockingFn>,
    pub blocking_end: Option<BlockingFn>,
    pub vendor_id: u16,
    pub device_id: u16,
    pub cache_dir: *const c_char,
    /// Display modes the adapter reports, in the host's order; null for the default list.
    pub modes: *const NativeDisplayMode,
    pub mode_count: u32,
}

// The C layout `d3d9metal.h` declares, on the 64-bit hosts the build serves.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(core::mem::offset_of!(NativeHost, user) == 8);
    assert!(core::mem::offset_of!(NativeHost, metal_layer) == 16);
    assert!(core::mem::offset_of!(NativeHost, blocking_end) == 48);
    assert!(core::mem::offset_of!(NativeHost, vendor_id) == 56);
    assert!(core::mem::offset_of!(NativeHost, device_id) == 58);
    assert!(core::mem::offset_of!(NativeHost, cache_dir) == 64);
    assert!(core::mem::offset_of!(NativeHost, modes) == 72);
    assert!(core::mem::offset_of!(NativeHost, mode_count) == 80);
    assert!(size_of::<NativeHost>() == 88);
};

/// The display mode a host that names none reports, in pixels and Hz.
pub const DEFAULT_MODE: (u32, u32, u32) = (1920, 1080, 60);

/// Why [`install`] refused a table.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    /// `size` is smaller than the fields every host has to provide.
    TooShort,
    /// `metal_layer`, `alloc_app` or `free_app` is null.
    MissingCallback,
    /// A table is installed already.
    AlreadyInstalled,
}

impl NativeHost {
    /// The `CAMetalLayer` behind window handle `hwnd` and its size, `None` when the host has none.
    #[must_use]
    pub fn metal_layer(&self, hwnd: usize) -> Option<(NonNull<c_void>, u32, u32)> {
        let call = self.metal_layer?;
        let hwnd = core::ptr::without_provenance_mut::<c_void>(hwnd);
        let mut width = 0;
        let mut height = 0;
        // SAFETY: `install`'s contract: the callback stays callable for the
        // process and accepts any window handle; both out-params are locals.
        let layer = unsafe { call(self.user, hwnd, &raw mut width, &raw mut height) };
        NonNull::new(layer).map(|layer| (layer, width, height))
    }

    /// Allocate `size` bytes at `align` from the application's memory.
    #[must_use]
    pub fn alloc_app(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        let call = self.alloc_app?;
        // SAFETY: `install`'s contract; the host answers null for a refusal.
        NonNull::new(unsafe { call(self.user, size, align) }.cast::<u8>())
    }

    /// Give back a block [`Self::alloc_app`] returned for `size` bytes.
    ///
    /// # Safety
    ///
    /// `ptr` came from `alloc_app` with this `size` and is not used afterwards.
    pub unsafe fn free_app(&self, ptr: NonNull<u8>, size: usize) {
        if let Some(call) = self.free_app {
            // SAFETY: the caller hands back an `alloc_app` block it owns.
            unsafe { call(self.user, ptr.as_ptr().cast::<c_void>(), size) };
        }
    }

    /// Tell the host the calling thread is about to wait inside the layer.
    pub fn blocking_begin(&self) {
        if let Some(call) = self.blocking_begin {
            // SAFETY: `install`'s contract.
            unsafe { call(self.user) };
        }
    }

    /// Tell the host the wait [`Self::blocking_begin`] announced is over.
    pub fn blocking_end(&self) {
        if let Some(call) = self.blocking_end {
            // SAFETY: `install`'s contract.
            unsafe { call(self.user) };
        }
    }

    /// The PCI vendor and device ids the adapter identifier reports, `None` for each zero.
    #[must_use]
    pub fn pci_ids(&self) -> (Option<u16>, Option<u16>) {
        let vendor = Some(self.vendor_id).filter(|id| *id != 0);
        let device = Some(self.device_id).filter(|id| *id != 0);
        (vendor, device)
    }

    /// The directory the shader cache lives in, `None` when the host names none.
    #[must_use]
    pub fn cache_dir(&self) -> Option<&str> {
        let dir = Some(self.cache_dir).filter(|dir| !dir.is_null())?;
        // SAFETY: `install`'s contract: a non-null `cache_dir` is a NUL-terminated
        // string that outlives the process's devices.
        unsafe { CStr::from_ptr(dir) }.to_str().ok()
    }

    /// The display modes as `(width, height, refresh_hz)`, the default mode when the host has none.
    #[must_use]
    pub fn display_modes(&self) -> Vec<(u32, u32, u32)> {
        let modes = self.modes;
        let (false, Ok(count @ 1..)) = (modes.is_null(), usize::try_from(self.mode_count)) else {
            return vec![DEFAULT_MODE];
        };
        // SAFETY: `install`'s contract: `modes` names `mode_count` modes that
        // outlive the process's interfaces.
        let modes = unsafe { core::slice::from_raw_parts(modes, count) };
        modes
            .iter()
            .filter(|mode| mode.width != 0 && mode.height != 0)
            .map(|mode| {
                let hz = if mode.refresh_hz == 0 {
                    60
                } else {
                    mode.refresh_hz
                };
                (mode.width, mode.height, hz)
            })
            .collect()
    }
}

/// Install the host's table for the rest of the process.
///
/// The table is copied, so the caller's struct may go away afterwards; the
/// fields past the caller's `size` read as absent.
///
/// # Errors
///
/// [`InstallError`] when the table is too short, misses a required callback,
/// or a table is installed already.
///
/// # Safety
///
/// `host` points at `host.size` readable bytes laid out as `NativeHost`. Its
/// callbacks are callable from any thread for the rest of the process,
/// `alloc_app` returns null or a block of at least `size` bytes at `align`,
/// `free_app` accepts each such block once, and `cache_dir` and `modes`, when
/// set, stay valid for the rest of the process.
pub unsafe fn install(host: *const NativeHost) -> Result<(), InstallError> {
    const REQUIRED: usize = core::mem::offset_of!(NativeHost, modes);
    let mut table = NativeHost {
        size: 0,
        user: core::ptr::null_mut(),
        metal_layer: None,
        alloc_app: None,
        free_app: None,
        blocking_begin: None,
        blocking_end: None,
        vendor_id: 0,
        device_id: 0,
        cache_dir: core::ptr::null(),
        modes: core::ptr::null(),
        mode_count: 0,
    };
    // SAFETY: the caller's contract makes `size` readable first.
    let size = unsafe { core::ptr::read_unaligned(host.cast::<u32>()) };
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    if size < REQUIRED {
        return Err(InstallError::TooShort);
    }
    let copied = size.min(size_of::<NativeHost>());
    // SAFETY: `copied` bytes are readable at `host` per the caller's contract
    // and fit `table`; every bit pattern the caller's C struct holds is a valid
    // `NativeHost` (nullable pointers and integers).
    unsafe {
        core::ptr::copy_nonoverlapping(host.cast::<u8>(), (&raw mut table).cast::<u8>(), copied);
    }
    table.size = u32::try_from(copied).unwrap_or(u32::MAX);
    if table.metal_layer.is_none() || table.alloc_app.is_none() || table.free_app.is_none() {
        return Err(InstallError::MissingCallback);
    }
    let leaked = Box::into_raw(Box::new(table));
    HOST.compare_exchange(
        core::ptr::null_mut(),
        leaked,
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .map(|_| ())
    .map_err(|_| {
        // SAFETY: the exchange failed, so `leaked` was never published.
        drop(unsafe { Box::from_raw(leaked) });
        InstallError::AlreadyInstalled
    })
}

/// The installed host, `None` before `d3d9metal_init` (and in every Wine build).
#[must_use]
pub fn get() -> Option<&'static NativeHost> {
    let host = HOST.load(Ordering::Acquire);
    // SAFETY: a non-null value is the leaked table `install` published, never freed.
    unsafe { host.as_ref() }
}
