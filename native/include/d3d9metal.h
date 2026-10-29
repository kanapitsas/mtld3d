/*
 * d3d9metal: mtld3d's Direct3D 9 on Metal, linked into a native host application.
 *
 * libd3d9metal (.dylib or .a) is the D3D9 API side and the Metal runtime of
 * mtld3d in one library, called in process. The host installs its callbacks
 * once with d3d9metal_init, then creates interfaces with
 * d3d9metal_Direct3DCreate9 (or Direct3DCreate9, also exported) and calls them
 * through the ordinary d3d9.h vtables.
 *
 * ABI. Every COM method is a C function pointer in the object's vtable, taking
 * the object first; on arm64 the D3D9 calling convention is the C one. An
 * HWND is the host's own opaque pointer: the layer never dereferences it and
 * only hands it back to metal_layer.
 *
 * Memory. Every pointer the layer hands the application to dereference (the
 * pBits of a LockRect or LockBox, the data of a vertex or index buffer Lock)
 * points into a block the host's alloc_app returned. COM objects, vtables and
 * the layer's private storage come from the layer's own allocator. Pointers
 * the application passes in (out-params, arrays, the data of
 * SetVertexShaderConstantF, ...) are dereferenced as plain host pointers for
 * the duration of the call, with no alignment assumed.
 *
 * Threads. A device created with D3DCREATE_MULTITHREADED may be called from
 * any thread; otherwise from one thread at a time, as D3D9 requires. No call
 * needs the main thread: the layer configures the CAMetalLayer on the calling
 * thread inside its own CATransaction, and acquires and presents drawables on
 * a presenter thread of its own. The host keeps the layer alive, and attached
 * to the window it renders into, for as long as a device presents into it.
 * Calls that wait (Present under backpressure or a frame-latency bound, a
 * read-back such as GetRenderTargetData or a Lock of a GPU-written resource,
 * Reset, the device's last Release, query polls with D3DGETDATA_FLUSH) wait
 * between blocking_begin and blocking_end on the calling thread.
 */
#ifndef D3D9METAL_H
#define D3D9METAL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* One display mode the adapter reports. A refresh rate of 0 reads as 60 Hz. */
typedef struct D3D9MetalDisplayMode {
    uint32_t width;
    uint32_t height;
    uint32_t refresh_hz;
} D3D9MetalDisplayMode;

typedef struct D3D9MetalHost {
    /* sizeof(D3D9MetalHost) as the host was built; fields past it read as absent. */
    uint32_t size;
    /* Passed back as the first argument of every callback. */
    void *user;
    /*
     * The CAMetalLayer behind hwnd, and the size in pixels a back buffer of
     * the window's default size has (what GetClientRect reports). NULL when
     * there is none. Required. Called from any thread, including during
     * CreateDevice and Reset.
     */
    void *(*metal_layer)(void *user, void *hwnd, uint32_t *width, uint32_t *height);
    /*
     * Memory the application may dereference, at least size bytes at align
     * (a power of two, 16 KiB for every block the layer asks for today).
     * NULL refuses. Required. Called from any thread.
     */
    void *(*alloc_app)(void *user, size_t size, size_t align);
    /* Gives back an alloc_app block of that size. Required. Called from any thread. */
    void (*free_app)(void *user, void *ptr, size_t size);
    /* Around every wait the layer makes on an application thread. May be NULL. */
    void (*blocking_begin)(void *user);
    void (*blocking_end)(void *user);
    /* Reported in D3DADAPTER_IDENTIFIER9; 0 keeps the layer's own value. */
    uint16_t vendor_id, device_id;
    /* Directory for the shader cache; NULL for the executable's directory. */
    const char *cache_dir;
    /*
     * Since 0.15.1, both may be absent (size): the display modes the adapter
     * enumerates, the first being the current one. NULL or 0 for 1920x1080 at
     * 60 Hz alone. Kept valid by the host for the life of the process.
     */
    const D3D9MetalDisplayMode *modes;
    uint32_t mode_count;
} D3D9MetalHost;

/* d3d9metal_init results. */
#define D3D9METAL_OK 0
#define D3D9METAL_E_NULL (-1)         /* host is NULL */
#define D3D9METAL_E_INVALID (-2)      /* size too small, or a required callback is NULL */
#define D3D9METAL_E_ALREADY (-3)      /* already initialised; the first table stays */

/*
 * Install the host's callbacks for the rest of the process. The struct is
 * copied; cache_dir and modes are not and stay the host's. Call once, before
 * the first Direct3DCreate9, from any thread. 0 on success.
 */
int d3d9metal_init(const D3D9MetalHost *host);

/*
 * Direct3DCreate9: an IDirect3D9*, or NULL before d3d9metal_init. The same
 * function is exported as Direct3DCreate9 (and Direct3DCreate9Ex), for a port
 * that links d3d9.h directly.
 */
void *d3d9metal_Direct3DCreate9(unsigned sdk_version);

#ifdef __cplusplus
}
#endif

#endif /* D3D9METAL_H */
