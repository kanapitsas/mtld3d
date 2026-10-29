//! A device of the native host build, end to end, against a `CAMetalLayer` in no window.
//!
//! What a host does: install its callbacks with `d3d9metal_init`, create an
//! interface and a device for a window handle its `metal_layer` callback
//! answers, and call the device through the `d3d9.h` vtables. The test clears
//! the back buffer, draws a triangle with a `vs_2_0` / `ps_2_0` pair, reads
//! the frame back through `GetRenderTargetData` and `LockRect`, and checks the
//! pixels, the pointers the layer hands out (both inside blocks the host's
//! `alloc_app` returned) and the host's blocking notifications. No window is
//! created and nothing is shown, so it runs on any Mac with a Metal GPU.

use core::{
    ffi::c_void,
    sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering},
};
use std::{
    alloc::Layout,
    sync::{Mutex, Once},
};

use mtld3d_core::native_host::NativeHost;
use mtld3d_types::{
    D3DCLEAR_TARGET, D3DCREATE_HARDWARE_VERTEXPROCESSING, D3DCULL_NONE, D3DDEVTYPE_HAL,
    D3DFMT_A8R8G8B8, D3DFMT_X8R8G8B8, D3DFVF_DIFFUSE, D3DFVF_XYZ, D3DLOCK_READONLY, D3DLOCKED_RECT,
    D3DPOOL_DEFAULT, D3DPOOL_SYSTEMMEM, D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS,
    D3DPT_TRIANGLELIST, D3DRS_CULLMODE, D3DRS_LIGHTING, D3DSWAPEFFECT_DISCARD, D3DUSAGE_WRITEONLY,
    IDirect3D9Vtbl, IDirect3DDevice9Vtbl, IDirect3DSurface9Vtbl, IDirect3DVertexBuffer9Vtbl,
};
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CAMetalLayer;

/// The back buffer's size, which is also the host layer's.
const SIZE: u32 = 64;
/// The window handle the host hands the layer: an opaque value, never dereferenced.
const HWND: usize = 0x5157_0001;
/// The window two devices are created on at once, with a layer of its own.
const SHARED_HWND: usize = 0x5157_0002;
/// The clear colour, opaque red, as `D3DCOLOR`.
const RED: u32 = 0xFFFF_0000;
/// The triangle's colour, opaque green, as `D3DCOLOR`.
const GREEN: u32 = 0xFF00_FF00;

/// `vs_2_0 { dcl_position v0; dcl_color0 v1; mov oPos, v0; mov oD0, v1; }`.
#[rustfmt::skip]
const VS: [u32; 14] = [
    0xFFFE_0200,
    0x0200_001F, 0x8000_0000, 0x900F_0000,
    0x0200_001F, 0x8000_000A, 0x900F_0001,
    0x0200_0001, 0xC00F_0000, 0x90E4_0000,
    0x0200_0001, 0xD00F_0000, 0x90E4_0001,
    0x0000_FFFF,
];

/// `ps_2_0 { dcl v0; mov oC0, v0; }`, the interpolated diffuse.
#[rustfmt::skip]
const PS: [u32; 8] = [
    0xFFFF_0200,
    0x0200_001F, 0x8000_0000, 0x900F_0000,
    0x0200_0001, 0x800F_0800, 0x90E4_0000,
    0x0000_FFFF,
];

/// Blocks the host's `alloc_app` handed out and has not been given back, as `(address, size)`.
static APP_BLOCKS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
/// `blocking_begin` calls minus `blocking_end` calls; never below zero.
static BLOCKING_DEPTH: AtomicU32 = AtomicU32::new(0);
/// `blocking_begin` calls in all.
static BLOCKING_CALLS: AtomicU32 = AtomicU32::new(0);
/// Held by each test for its whole run: the host's counters are process-wide.
static SERIAL: Mutex<()> = Mutex::new(());
/// The host's layer, as an address, for the callback.
static LAYER: AtomicUsize = AtomicUsize::new(0);
/// The layer of [`SHARED_HWND`], which two devices present into, as an address.
static SHARED_LAYER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn metal_layer(
    _user: *mut c_void,
    hwnd: *mut c_void,
    width: *mut u32,
    height: *mut u32,
) -> *mut c_void {
    let layer = match hwnd as usize {
        HWND => LAYER.load(Ordering::Acquire),
        SHARED_HWND => SHARED_LAYER.load(Ordering::Acquire),
        _ => return core::ptr::null_mut(),
    };
    // SAFETY: the layer passes writable out-params.
    unsafe { width.write(SIZE) };
    // SAFETY: as above.
    unsafe { height.write(SIZE) };
    layer as *mut c_void
}

unsafe extern "C" fn alloc_app(_user: *mut c_void, size: usize, align: usize) -> *mut c_void {
    let Ok(layout) = Layout::from_size_align(size, align) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the layer never asks for zero bytes (a page box is at least a page).
    let ptr = unsafe { std::alloc::alloc(layout) };
    if !ptr.is_null() {
        APP_BLOCKS.lock().unwrap().push((ptr as usize, size));
    }
    ptr.cast()
}

unsafe extern "C" fn free_app(_user: *mut c_void, ptr: *mut c_void, size: usize) {
    let mut blocks = APP_BLOCKS.lock().unwrap();
    let index = blocks
        .iter()
        .position(|&(addr, len)| addr == ptr as usize && len == size)
        .expect("free_app of a block alloc_app never handed out");
    blocks.swap_remove(index);
    drop(blocks);
    let layout = Layout::from_size_align(size, 16 * 1024).unwrap();
    // SAFETY: the block came from `alloc_app` with this size; every block the
    // layer asks for is page-aligned, which the layout restates.
    unsafe { std::alloc::dealloc(ptr.cast(), layout) };
}

unsafe extern "C" fn blocking_begin(_user: *mut c_void) {
    BLOCKING_DEPTH.fetch_add(1, Ordering::AcqRel);
    BLOCKING_CALLS.fetch_add(1, Ordering::AcqRel);
}

unsafe extern "C" fn blocking_end(_user: *mut c_void) {
    let before = BLOCKING_DEPTH.fetch_sub(1, Ordering::AcqRel);
    assert!(before > 0, "blocking_end without blocking_begin");
}

/// Whether `len` bytes at `ptr` lie inside one block `alloc_app` handed out.
fn in_app_memory(ptr: *const c_void, len: usize) -> bool {
    let start = ptr as usize;
    APP_BLOCKS
        .lock()
        .unwrap()
        .iter()
        .any(|&(addr, size)| start >= addr && start + len <= addr + size)
}

/// The host's offscreen layer: in no window, `SIZE` points square at scale 1.
fn make_layer() -> Retained<CAMetalLayer> {
    let layer = CAMetalLayer::new();
    layer.setBounds(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: f64::from(SIZE),
            height: f64::from(SIZE),
        },
    });
    layer.setContentsScale(1.0);
    layer
}

/// The vtable of a COM object.
///
/// # Safety
///
/// `this` is a live COM object whose vtable is a `T`.
const unsafe fn vtbl<'a, T>(this: *mut c_void) -> &'a T {
    // SAFETY: the caller's contract: the object's first field is its vtable pointer.
    let table = unsafe { this.cast::<*const T>().read() };
    // SAFETY: the vtable is a static `T` that outlives the object.
    unsafe { &*table }
}

/// The three slots every COM vtable starts with.
#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
}

/// `IUnknown::Release`, answering the count left.
///
/// # Safety
///
/// `this` is a live COM object the caller holds a reference on.
unsafe fn release(this: *mut c_void) -> u32 {
    // SAFETY: every COM vtable starts with the `IUnknown` slots.
    let table = unsafe { vtbl::<IUnknownVtbl>(this) };
    // SAFETY: the caller gives up its reference.
    unsafe { (table.release)(this) }
}

/// Assert a vtable call answers `S_OK`.
macro_rules! check {
    ($call:expr, $what:literal) => {{
        // SAFETY: a vtable call on a live object of the device with live
        // arguments and out-params, per the call's own contract.
        let hr = unsafe { $call };
        assert_eq!(hr, 0, $what);
    }};
}

/// One BGRA pixel of a locked 32-bit surface, as `D3DCOLOR`.
///
/// # Safety
///
/// `rect` is a live lock of a `SIZE`-square 32-bit surface.
unsafe fn pixel(rect: &D3DLOCKED_RECT, x: u32, y: u32) -> u32 {
    let pitch = usize::try_from(rect.pitch).unwrap();
    let offset = y as usize * pitch + x as usize * 4;
    // SAFETY: the caller's contract; the offset is inside the locked rows.
    let texel = unsafe { rect.bits.cast::<u8>().add(offset) };
    // SAFETY: four readable bytes at `texel`, by the same contract.
    unsafe { texel.cast::<u32>().read_unaligned() }
}

fn init_host() {
    static INIT: Once = Once::new();
    static STATUS: AtomicI32 = AtomicI32::new(i32::MIN);
    INIT.call_once(|| {
        // Draws wait for their shaders rather than leaving the frame (the
        // frame is read back at once), and no shader cache is written beside
        // the test.
        // SAFETY: set once, before any interface reads it, under the `Once`
        // every test passes through first; the tests read no environment.
        unsafe {
            std::env::set_var(
                "MTLD3D_CONFIG",
                "shader.asyncCompile = false; shaderCache.enable = false",
            );
        }
        let host = NativeHost {
            size: u32::try_from(size_of::<NativeHost>()).unwrap(),
            user: core::ptr::null_mut(),
            metal_layer: Some(metal_layer),
            alloc_app: Some(alloc_app),
            free_app: Some(free_app),
            blocking_begin: Some(blocking_begin),
            blocking_end: Some(blocking_end),
            vendor_id: 0x10DE,
            device_id: 0x0611,
            cache_dir: core::ptr::null(),
            modes: core::ptr::null(),
            mode_count: 0,
        };
        // SAFETY: a complete table whose callbacks live for the process.
        let first = unsafe { d3d9metal::d3d9metal_init(&raw const host) };
        // SAFETY: as above; a second table is refused and the first stays.
        let second = unsafe { d3d9metal::d3d9metal_init(&raw const host) };
        assert_eq!(second, -3, "a second d3d9metal_init");
        STATUS.store(first, Ordering::Release);
    });
    assert_eq!(STATUS.load(Ordering::Acquire), 0, "d3d9metal_init");
}

#[test]
fn clear_draw_and_read_back_through_host_memory() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let layer = make_layer();
    LAYER.store(Retained::as_ptr(&layer) as usize, Ordering::Release);
    init_host();
    // SAFETY: a null table is refused before it is read.
    assert_eq!(unsafe { d3d9metal::d3d9metal_init(core::ptr::null()) }, -1);

    let d3d = d3d9metal::direct3d_create9(32);
    assert!(!d3d.is_null(), "Direct3DCreate9");
    // SAFETY: `d3d` is the live IDirect3D9 just created.
    let d3d_vtbl = unsafe { vtbl::<IDirect3D9Vtbl>(d3d) };

    // The host's PCI ids reach the adapter identifier.
    // SAFETY: plain data; zeroed is a valid value.
    let mut identifier = unsafe { core::mem::zeroed::<mtld3d_types::D3DADAPTER_IDENTIFIER9>() };
    // SAFETY: vtable call on the live interface with a writable out-param.
    let hr = unsafe { (d3d_vtbl.get_adapter_identifier)(d3d, 0, 0, &raw mut identifier) };
    assert_eq!(hr, 0, "GetAdapterIdentifier");
    assert_eq!(
        (identifier.vendor_id, identifier.device_id),
        (0x10DE, 0x0611)
    );

    let mut pp = D3DPRESENT_PARAMETERS {
        back_buffer_width: SIZE,
        back_buffer_height: SIZE,
        back_buffer_format: D3DFMT_X8R8G8B8,
        back_buffer_count: 1,
        multi_sample_type: 0,
        multi_sample_quality: 0,
        swap_effect: D3DSWAPEFFECT_DISCARD,
        device_window: HWND,
        windowed: 1,
        enable_auto_depth_stencil: 0,
        auto_depth_stencil_format: 0,
        flags: 0,
        full_screen_refresh_rate_in_hz: 0,
        presentation_interval: D3DPRESENT_INTERVAL_IMMEDIATE,
    };
    let mut device: *mut c_void = core::ptr::null_mut();
    // SAFETY: vtable call with live out-params; the window handle is the host's.
    let hr = unsafe {
        (d3d_vtbl.create_device)(
            d3d,
            0,
            D3DDEVTYPE_HAL,
            HWND as *mut c_void,
            D3DCREATE_HARDWARE_VERTEXPROCESSING,
            (&raw mut pp).cast(),
            &raw mut device,
        )
    };
    assert_eq!(hr, 0, "CreateDevice");
    assert!(!device.is_null());
    // SAFETY: `device` is the live IDirect3DDevice9 just created.
    let dev = unsafe { vtbl::<IDirect3DDevice9Vtbl>(device) };

    // A vertex buffer's Lock hands out host memory.
    let vertices: [(f32, f32, f32, u32); 3] = [
        (-0.5, -0.5, 0.5, GREEN),
        (0.0, 0.5, 0.5, GREEN),
        (0.5, -0.5, 0.5, GREEN),
    ];
    let stride = 16_u32;
    let mut vb: *mut c_void = core::ptr::null_mut();
    // SAFETY: vtable call with a live out-param.
    let hr = unsafe {
        (dev.create_vertex_buffer)(
            device,
            stride * 3,
            D3DUSAGE_WRITEONLY,
            D3DFVF_XYZ | D3DFVF_DIFFUSE,
            D3DPOOL_DEFAULT,
            &raw mut vb,
            core::ptr::null_mut(),
        )
    };
    assert_eq!(hr, 0, "CreateVertexBuffer");
    // SAFETY: `vb` is the live vertex buffer just created.
    let vb_vtbl = unsafe { vtbl::<IDirect3DVertexBuffer9Vtbl>(vb) };
    let mut data: *mut c_void = core::ptr::null_mut();
    // SAFETY: vtable call with a live out-param.
    let hr = unsafe { (vb_vtbl.lock)(vb, 0, 0, &raw mut data, 0) };
    assert_eq!(hr, 0, "IDirect3DVertexBuffer9::Lock");
    assert!(
        in_app_memory(data, (stride * 3) as usize),
        "the vertex buffer Lock pointer {data:p} is outside every alloc_app block"
    );
    for (index, &(x, y, z, color)) in vertices.iter().enumerate() {
        let mut bytes = [0_u8; 16];
        bytes[0..4].copy_from_slice(&x.to_le_bytes());
        bytes[4..8].copy_from_slice(&y.to_le_bytes());
        bytes[8..12].copy_from_slice(&z.to_le_bytes());
        bytes[12..16].copy_from_slice(&color.to_le_bytes());
        // SAFETY: the lock covers the whole buffer, three 16-byte vertices.
        let slot = unsafe { data.cast::<u8>().add(index * 16) };
        // SAFETY: 16 writable bytes at `slot`, which `bytes` does not overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), slot, bytes.len()) };
    }
    // SAFETY: vtable call on the locked buffer.
    assert_eq!(unsafe { (vb_vtbl.unlock)(vb) }, 0, "Unlock");

    let mut vs: *mut c_void = core::ptr::null_mut();
    let mut ps: *mut c_void = core::ptr::null_mut();
    check!(
        (dev.create_vertex_shader)(device, VS.as_ptr(), &raw mut vs),
        "CreateVertexShader"
    );
    check!(
        (dev.create_pixel_shader)(device, PS.as_ptr(), &raw mut ps),
        "CreatePixelShader"
    );
    check!(
        (dev.clear)(device, 0, core::ptr::null(), D3DCLEAR_TARGET, RED, 1.0, 0),
        "Clear"
    );
    check!((dev.begin_scene)(device), "BeginScene");
    check!(
        (dev.set_render_state)(device, D3DRS_LIGHTING, 0),
        "SetRenderState(LIGHTING)"
    );
    check!(
        (dev.set_render_state)(device, D3DRS_CULLMODE, D3DCULL_NONE),
        "SetRenderState(CULLMODE)"
    );
    check!((dev.set_vertex_shader)(device, vs), "SetVertexShader");
    check!((dev.set_pixel_shader)(device, ps), "SetPixelShader");
    check!((dev.set_fvf)(device, D3DFVF_XYZ | D3DFVF_DIFFUSE), "SetFVF");
    check!(
        (dev.set_stream_source)(device, 0, vb, 0, stride),
        "SetStreamSource"
    );
    check!(
        (dev.draw_primitive)(device, D3DPT_TRIANGLELIST, 0, 1),
        "DrawPrimitive"
    );
    check!((dev.end_scene)(device), "EndScene");

    // Read the frame back before presenting, as a screenshot would.
    let mut back: *mut c_void = core::ptr::null_mut();
    let mut readback: *mut c_void = core::ptr::null_mut();
    check!(
        (dev.get_back_buffer)(device, 0, 0, 0, &raw mut back),
        "GetBackBuffer"
    );
    check!(
        (dev.create_offscreen_plain_surface)(
            device,
            SIZE,
            SIZE,
            D3DFMT_A8R8G8B8,
            D3DPOOL_SYSTEMMEM,
            &raw mut readback,
            core::ptr::null_mut(),
        ),
        "CreateOffscreenPlainSurface"
    );
    let blocking_before = BLOCKING_CALLS.load(Ordering::Acquire);
    // SAFETY: both surfaces are live and of the device.
    let hr = unsafe { (dev.get_render_target_data)(device, back, readback) };
    assert_eq!(hr, 0, "GetRenderTargetData");
    assert!(
        BLOCKING_CALLS.load(Ordering::Acquire) > blocking_before,
        "the read-back waited outside blocking_begin"
    );
    // SAFETY: `readback` is the live surface just created.
    let surface = unsafe { vtbl::<IDirect3DSurface9Vtbl>(readback) };
    let mut rect = D3DLOCKED_RECT {
        pitch: 0,
        bits: core::ptr::null_mut(),
    };
    check!(
        (surface.lock_rect)(
            readback,
            (&raw mut rect).cast(),
            core::ptr::null(),
            D3DLOCK_READONLY
        ),
        "LockRect"
    );
    let pitch = usize::try_from(rect.pitch).unwrap();
    assert!(
        in_app_memory(rect.bits, pitch * SIZE as usize),
        "the LockRect pointer {:p} is outside every alloc_app block",
        rect.bits
    );
    // SAFETY: the lock is live on a SIZE-square A8R8G8B8 surface.
    let center = unsafe { pixel(&rect, SIZE / 2, SIZE / 2 + 4) };
    // SAFETY: as above.
    let corner = unsafe { pixel(&rect, 1, 1) };
    // SAFETY: as above.
    let top = unsafe { pixel(&rect, SIZE / 2, 2) };
    // SAFETY: vtable call on the locked surface.
    assert_eq!(unsafe { (surface.unlock_rect)(readback) }, 0);
    assert_eq!(
        center & 0x00FF_FFFF,
        GREEN & 0x00FF_FFFF,
        "triangle centre: {center:#010x}"
    );
    assert_eq!(
        corner & 0x00FF_FFFF,
        RED & 0x00FF_FFFF,
        "clear colour in the corner: {corner:#010x}"
    );
    assert_eq!(
        top & 0x00FF_FFFF,
        RED & 0x00FF_FFFF,
        "above the apex: {top:#010x}"
    );

    // One present through the presenter thread into the offscreen layer.
    // SAFETY: vtable call; null rects and window present the whole back buffer.
    let hr = unsafe {
        (dev.present)(
            device,
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null_mut(),
            core::ptr::null(),
        )
    };
    assert_eq!(hr, 0, "Present");

    // Each object is live and released once; the device goes after its
    // children, the interface after the device.
    for child in [readback, back, vb, vs, ps] {
        // SAFETY: the test holds one reference on each child.
        unsafe { release(child) };
    }
    // SAFETY: the test's reference on the device, its last.
    assert_eq!(unsafe { release(device) }, 0, "the device's last reference");
    // SAFETY: the test's reference on the interface, its last.
    assert_eq!(unsafe { release(d3d) }, 0, "the interface's last reference");
    assert_eq!(
        BLOCKING_DEPTH.load(Ordering::Acquire),
        0,
        "unbalanced blocking calls"
    );
    drop(layer);
}

/// The present parameters of a windowed `SIZE`-square device on `hwnd`.
const fn windowed_params(hwnd: usize) -> D3DPRESENT_PARAMETERS {
    D3DPRESENT_PARAMETERS {
        back_buffer_width: SIZE,
        back_buffer_height: SIZE,
        back_buffer_format: D3DFMT_X8R8G8B8,
        back_buffer_count: 1,
        multi_sample_type: 0,
        multi_sample_quality: 0,
        swap_effect: D3DSWAPEFFECT_DISCARD,
        device_window: hwnd,
        windowed: 1,
        enable_auto_depth_stencil: 0,
        auto_depth_stencil_format: 0,
        flags: 0,
        full_screen_refresh_rate_in_hz: 0,
        presentation_interval: D3DPRESENT_INTERVAL_IMMEDIATE,
    }
}

/// A windowed device of `d3d` on `hwnd`.
///
/// # Safety
///
/// `d3d` is a live `IDirect3D9`.
unsafe fn create_device(d3d: *mut c_void, hwnd: usize) -> *mut c_void {
    // SAFETY: the caller's contract.
    let d3d_vtbl = unsafe { vtbl::<IDirect3D9Vtbl>(d3d) };
    let mut pp = windowed_params(hwnd);
    let mut device: *mut c_void = core::ptr::null_mut();
    check!(
        (d3d_vtbl.create_device)(
            d3d,
            0,
            D3DDEVTYPE_HAL,
            hwnd as *mut c_void,
            D3DCREATE_HARDWARE_VERTEXPROCESSING,
            (&raw mut pp).cast(),
            &raw mut device,
        ),
        "CreateDevice"
    );
    assert!(!device.is_null());
    device
}

/// Clear `device`'s back buffer to `color`, read its centre back, and present it.
///
/// # Safety
///
/// `device` is a live `IDirect3DDevice9` with a `SIZE`-square back buffer.
unsafe fn clear_read_and_present(device: *mut c_void, color: u32) -> u32 {
    // SAFETY: the caller's contract.
    let dev = unsafe { vtbl::<IDirect3DDevice9Vtbl>(device) };
    check!(
        (dev.clear)(device, 0, core::ptr::null(), D3DCLEAR_TARGET, color, 1.0, 0),
        "Clear"
    );
    let mut back: *mut c_void = core::ptr::null_mut();
    let mut readback: *mut c_void = core::ptr::null_mut();
    check!(
        (dev.get_back_buffer)(device, 0, 0, 0, &raw mut back),
        "GetBackBuffer"
    );
    check!(
        (dev.create_offscreen_plain_surface)(
            device,
            SIZE,
            SIZE,
            D3DFMT_A8R8G8B8,
            D3DPOOL_SYSTEMMEM,
            &raw mut readback,
            core::ptr::null_mut(),
        ),
        "CreateOffscreenPlainSurface"
    );
    check!(
        (dev.get_render_target_data)(device, back, readback),
        "GetRenderTargetData"
    );
    // SAFETY: `readback` is the live surface just created.
    let surface = unsafe { vtbl::<IDirect3DSurface9Vtbl>(readback) };
    let mut rect = D3DLOCKED_RECT {
        pitch: 0,
        bits: core::ptr::null_mut(),
    };
    check!(
        (surface.lock_rect)(
            readback,
            (&raw mut rect).cast(),
            core::ptr::null(),
            D3DLOCK_READONLY
        ),
        "LockRect"
    );
    // SAFETY: the lock is live on a SIZE-square A8R8G8B8 surface.
    let center = unsafe { pixel(&rect, SIZE / 2, SIZE / 2) };
    check!((surface.unlock_rect)(readback), "UnlockRect");
    for _ in 0..3 {
        check!(
            (dev.present)(
                device,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null_mut(),
                core::ptr::null(),
            ),
            "Present"
        );
    }
    for child in [readback, back] {
        // SAFETY: this function holds one reference on each.
        unsafe { release(child) };
    }
    center
}

/// The retain count of `layer`, read through Core Foundation.
fn retain_count(layer: &CAMetalLayer) -> isize {
    // SAFETY: every Objective-C object is a `CFType` to Core Foundation's
    // retain-count functions, which only read the count.
    let cf = unsafe { &*core::ptr::from_ref(layer).cast::<objc2_core_foundation::CFType>() };
    objc2_core_foundation::CFGetRetainCount(Some(cf))
}

/// Wait for `layer`'s retain count to come down to `expected`, and assert it does.
///
/// A presented drawable keeps its layer retained until Core Animation lets
/// it go, a little after the device that presented it is gone, so the count
/// can stand above `expected` for a moment. It never goes below: that is the
/// over-release this guards against.
fn settle_retain_count(layer: &CAMetalLayer, expected: isize, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count = retain_count(layer);
        assert!(
            count >= expected,
            "{what}: the layer is over-released ({count} < {expected})"
        );
        if count == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: the layer keeps {count} retains, expected {expected}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Two devices on one window share its layer, and either may go first.
///
/// A game creates its next device before it releases the last, or probes
/// with a second one. Each attach owns its retain on the host's layer: the
/// device released first gives back its own and leaves the other's record
/// alone, so the survivor still draws and presents, and once both are gone
/// the layer has the retains it had before.
#[test]
fn two_devices_share_a_layer_in_either_release_order() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let layer = make_layer();
    SHARED_LAYER.store(Retained::as_ptr(&layer) as usize, Ordering::Release);
    init_host();
    let d3d = d3d9metal::direct3d_create9(32);
    assert!(!d3d.is_null(), "Direct3DCreate9");
    let before = retain_count(&layer);

    for older_goes_first in [true, false] {
        // SAFETY: `d3d` is live.
        let older = unsafe { create_device(d3d, SHARED_HWND) };
        // Each attach keeps one retain on the host's layer, its own.
        let with_one = before + 1;
        settle_retain_count(&layer, with_one, "a device holds one retain on the layer");
        // SAFETY: as above.
        let newer = unsafe { create_device(d3d, SHARED_HWND) };
        settle_retain_count(
            &layer,
            with_one + 1,
            "the second device takes a retain of its own",
        );
        // SAFETY: both devices are live and SIZE-square.
        let drawn = unsafe { clear_read_and_present(older, RED) };
        assert_eq!(drawn & 0xFF_FFFF, RED & 0xFF_FFFF, "the older device draws");
        // SAFETY: as above.
        let drawn = unsafe { clear_read_and_present(newer, GREEN) };
        assert_eq!(
            drawn & 0xFF_FFFF,
            GREEN & 0xFF_FFFF,
            "the newer device draws"
        );

        let (first, survivor) = if older_goes_first {
            (older, newer)
        } else {
            (newer, older)
        };
        // SAFETY: the test's only reference on `first`.
        let left = unsafe { release(first) };
        assert_eq!(left, 0, "the first device's last reference");
        settle_retain_count(
            &layer,
            with_one,
            "releasing one device gives back its retains alone",
        );
        let blue = 0xFF00_00FF;
        // SAFETY: the survivor is live and SIZE-square.
        let drawn = unsafe { clear_read_and_present(survivor, blue) };
        assert_eq!(
            drawn & 0xFF_FFFF,
            blue & 0xFF_FFFF,
            "the survivor still draws and presents"
        );
        // SAFETY: the test's only reference on the survivor.
        let left = unsafe { release(survivor) };
        assert_eq!(left, 0, "the survivor's last reference");
        settle_retain_count(
            &layer,
            before,
            "with both devices gone the layer has the retains it had",
        );
    }
    // SAFETY: the test's reference on the interface, its last.
    assert_eq!(unsafe { release(d3d) }, 0, "the interface's last reference");
    drop(layer);
}
