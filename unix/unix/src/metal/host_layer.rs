//! The layer a native host build presents into: the `CAMetalLayer` the host application owns.
//!
//! The Wine build gets its layer from a metal view Wine creates in the
//! window's client area, and follows the window, its screen and its cursor
//! through `AppKit` (`macdrv`). A native host build has no Wine and no window
//! of its own: the host's `metal_layer` callback
//! (`mtld3d_core::native_host`) answers the layer behind a window handle, and
//! the host owns everything about the window. This module has the surface of
//! `macdrv` that the rest of the runtime calls, so the presenter, the gamma
//! path and device teardown are the same code in both builds:
//!
//! - Each attach has an identity of its own, the address of a box holding
//!   the layer retain that attach took, which stands in for the view handle
//!   the PE side keeps and keys the attachment record. Two devices on one
//!   window share the host's one layer, as two devices of the Wine build
//!   share the window (a game creates its next device before it releases
//!   the last, or probes with a second one), so the layer's address cannot
//!   name a record: each device retires its own record and gives back its
//!   own retain, whichever goes first. The newest device configured the
//!   layer last and is the one its layer-keyed calls (`Reset`'s pacing, the
//!   gamma ramp) find, as in the Wine build the newest device's view is the
//!   one shown.
//! - The layer is configured on the attaching thread, the device's API
//!   thread, inside an explicit transaction: device, pixel format, opacity,
//!   `framebufferOnly`, pacing and drawable size, never anything of a view or
//!   a window. The presenter then calls `nextDrawable` on its own thread and
//!   resizes the drawable to follow the layer's bounds, as in the Wine build.
//!   The host keeps the layer alive and in place for as long as a device
//!   presents into it, and resizes it from whichever thread it likes.
//! - The layer reports SDR, a backing scale of 1 and no occlusion: the
//!   display, the cursor and the window's visibility are the host's.
//! - Nothing runs on the main thread, so there is no main-thread hop for a
//!   caller to deadlock on.

use std::sync::Arc;

use log::info;
use mtld3d_shared::{
    MetalHandle, SetCursorOverlayParams,
    mtl::ClientSurfaceRequest,
    mtl_handle::{CAMetalLayerKind, MTLDeviceKind, NSViewKind},
};
use objc2::rc::Retained;
use objc2_quartz_core::{CAMetalLayer, CATransaction};

use super::layer::{
    DisplayCaps, LayerAttachRequest, PresentPacing, min_present_duration, pack_pacing,
    sync_drawable_size, unpack_pacing,
};
use crate::{LOG_TARGET, metal::handle::IntoRetained};

#[path = "macdrv/attachment.rs"]
pub mod attachment;

use attachment::{AttachFlags, AttachLatches, Attachment};

/// How many drawables the layer keeps, as the Wine build's metal view does.
const DRAWABLE_POOL_DEPTH: usize = 3;

/// Attach the host's layer behind `request.hwnd` to the device, and register its record.
///
/// Returns the view handle (the layer's address), the layer handle and the
/// display caps; `None` when there is no host, or the host has no layer for
/// the window, logged.
pub fn attach_metal_layer(
    device_handle: MetalHandle<MTLDeviceKind>,
    request: LayerAttachRequest,
) -> Option<(
    MetalHandle<NSViewKind>,
    MetalHandle<CAMetalLayerKind>,
    DisplayCaps,
)> {
    let LayerAttachRequest {
        hwnd,
        width,
        height,
        pacing,
        hdr_enable,
        color_space,
        backing_scale_sink_ptr,
        software_cursor,
    } = request;
    if hwnd == 0 || device_handle.is_null() {
        return None;
    }
    let host = mtld3d_core::native_host::get()?;
    let window = usize::try_from(hwnd).expect("a 64-bit host addresses every window handle");
    let Some((raw_layer, _, _)) = host.metal_layer(window) else {
        log::error!(
            target: LOG_TARGET,
            "present: the host has no CAMetalLayer for window {hwnd:#x}",
        );
        return None;
    };
    // SAFETY: the host's contract: `metal_layer` answers a live `CAMetalLayer`,
    // which it keeps alive while a device presents into it; the retain taken
    // here is this record's, given back in `retire_metal_view`.
    let layer = unsafe { Retained::retain(raw_layer.as_ptr().cast::<CAMetalLayer>()) }?;
    let device = device_handle.into_retained()?;
    mtld3d_shared::log_once_info!(
        target: LOG_TARGET,
        "present: color.space {color_space:?} on a native host: the host's layer keeps its own \
         colorspace",
    );
    if hdr_enable {
        mtld3d_shared::log_once_info!(
            target: LOG_TARGET,
            "present: color.hdr.enable on a native host: the host's layer stays SDR",
        );
    }
    if software_cursor.resolve(false) {
        mtld3d_shared::log_once_info!(
            target: LOG_TARGET,
            "cursor: cursor.software on a native host: the host draws its own cursor",
        );
    }
    configure_layer(&layer, &device);
    let layer_addr = Retained::as_ptr(&layer) as usize;
    let panel_hz = host.display_modes().first().map_or(60, |&(_, _, hz)| hz);
    let min_duration = min_present_duration(f64::from(panel_hz), &pacing);
    let bounds = layer.bounds();
    let contents_scale = layer.contentsScale();
    // The retain taken above moves into the attach's own box, whose address
    // is the view handle and the record's key until `retire_metal_view`
    // gives the box, and the retain with it, back.
    let token = Box::into_raw(Box::new(layer)) as usize;
    let att = attachment::register(
        token,
        layer_addr,
        &AttachLatches {
            hwnd,
            flags: AttachFlags::empty(),
            pacing_bits: pack_pacing(&pacing),
            backing_scale: 1,
            backing_scale_sink: usize::try_from(backing_scale_sink_ptr)
                .expect("a 64-bit host addresses every sink"),
        },
    );
    att.set_min_present_duration(min_duration);
    attachment::publish_backing_scale(&att, 1);
    info!(
        target: LOG_TARGET,
        "present: host layer {layer_addr:#x} of window {hwnd:#x} attached as {token:#x}: guest \
         {width}x{height}, layer {:.0}x{:.0}pt @{contents_scale:.2}x, minimum present duration \
         {min_duration:.5}s ({panel_hz} Hz)",
        bounds.size.width,
        bounds.size.height,
    );
    // SAFETY: `token` is the box that owns this attach's layer retain.
    let view_handle = unsafe { MetalHandle::<NSViewKind>::new(token as u64) };
    // SAFETY: the layer that box keeps retained until the view is retired.
    let layer_handle = unsafe { MetalHandle::<CAMetalLayerKind>::new(layer_addr as u64) };
    Some((
        view_handle,
        layer_handle,
        DisplayCaps {
            backing_scale: 1,
            software_cursor_active: false,
        },
    ))
}

/// Retire the record `view_handle` names, at device teardown or a retargeting `Reset`.
///
/// The layer's gamma table goes with the last record on the layer: another
/// device presenting into it keeps the ramp it set.
pub fn detach_metal_layer(view_handle: MetalHandle<NSViewKind>) -> Option<Arc<Attachment>> {
    let addr = usize::try_from(view_handle.raw()).expect("a 64-bit host addresses every view");
    let att = attachment::unregister(addr)?;
    if attachment::find_by_layer(att.layer()).is_none() {
        super::gamma::detach(&att);
    }
    Some(att)
}

/// Give back the layer retain this view's attach took; the host keeps the layer itself.
pub fn retire_metal_view(view_handle: MetalHandle<NSViewKind>, _record: Option<&Attachment>) {
    if view_handle.is_null() {
        return;
    }
    let token = usize::try_from(view_handle.raw()).expect("a 64-bit host addresses every view");
    // SAFETY: the view handle is the box `attach_metal_layer` handed out,
    // retired once by the device that holds it; dropping it gives back the
    // retain that attach took.
    drop(unsafe { Box::from_raw(token as *mut Retained<CAMetalLayer>) });
}

/// A host has no client surface to show; marks the device as having drawn.
pub fn present_client_surface(
    view_handle: MetalHandle<NSViewKind>,
    _reasons: ClientSurfaceRequest,
) {
    let addr = usize::try_from(view_handle.raw()).expect("a 64-bit host addresses every layer");
    if let Some(att) = attachment::find(addr) {
        att.note_frame_submitted();
    }
}

/// Apply a `Reset`'s new present pacing to the layer's record.
pub fn set_display_sync_enabled(
    layer_handle: MetalHandle<CAMetalLayerKind>,
    pacing: &PresentPacing,
) {
    let addr = usize::try_from(layer_handle.raw()).expect("a 64-bit host addresses every layer");
    let Some(att) = attachment::find_by_layer(addr) else {
        mtld3d_shared::log_once_warn!(
            target: LOG_TARGET,
            "present: SetDisplaySyncEnabled for layer {addr:#x} with no attachment record; the \
             present throttle is unchanged",
        );
        return;
    };
    att.set_pacing_bits(pack_pacing(pacing));
    info!(
        target: LOG_TARGET,
        "present: layer {addr:#x} re-paced (vsync {}, maxFps {})",
        if pacing.vsync_requested { "on" } else { "off" },
        pacing.max_fps,
    );
    if att.request_refresh() {
        run_on_main_thread_async(move || refresh_attachment_on_main(&att));
    }
}

/// The cursor is the host's: no overlay is drawn, and the request is refused.
pub fn set_cursor_overlay(_params: &SetCursorOverlayParams, _pixels: Option<&[u8]>) -> bool {
    mtld3d_shared::log_once_info!(
        target: LOG_TARGET,
        "cursor: a native host draws its own cursor; the software cursor overlay is off",
    );
    false
}

/// No cursor overlay to follow the present.
pub const fn poll_from_present() {}

/// A native host decides its own process activity; nothing to declare.
pub const fn declare_latency_critical_activity() {}

/// Run what `attachment` would hop to the main thread, here and now.
///
/// The only work it queues is the display reconciliation, which on a native
/// host touches no `AppKit` object.
fn run_on_main_thread_async<F: FnOnce() + Send + 'static>(f: F) {
    f();
}

/// The display reconciliation of a native host, against the host's first display mode.
///
/// The host owns the window and its screen, so the display-derived state is
/// what a layer with no screen has: SDR at headroom 1, never occluded, a
/// backing scale of 1. What a `Reset` changed is the pacing, so the throttle
/// is derived again from the pacing the record now holds.
fn refresh_attachment_on_main(att: &Arc<Attachment>) {
    att.end_headroom_refresh();
    att.set_hdr_active(false);
    att.set_window_occluded(false);
    att.set_headroom(1.0);
    att.set_backing_scale(1);
    let panel_hz = mtld3d_core::native_host::get()
        .and_then(|host| host.display_modes().first().map(|&(_, _, hz)| hz))
        .unwrap_or(60);
    let pacing = unpack_pacing(att.pacing_bits());
    att.set_min_present_duration(min_present_duration(f64::from(panel_hz), &pacing));
}

/// Configure a host layer for the device, in an explicit transaction on the calling thread.
fn configure_layer(
    layer: &CAMetalLayer,
    device: &objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>,
) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setDevice(Some(device));
    layer.setPixelFormat(objc2_metal::MTLPixelFormat::BGRA8Unorm);
    layer.setOpaque(true);
    // Nothing reads the drawable: every guest read of the frame goes to the
    // back-buffer texture, and present writes it through render passes.
    layer.setFramebufferOnly(true);
    // Pacing rides `presentDrawable:afterMinimumDuration:`, as in the Wine build.
    #[cfg(target_os = "macos")]
    layer.setDisplaySyncEnabled(false);
    layer.setMaximumDrawableCount(DRAWABLE_POOL_DEPTH);
    layer.setAllowsNextDrawableTimeout(true);
    layer.setPresentsWithTransaction(false);
    sync_drawable_size(layer);
    crate::hud_state::show_row(layer);
    CATransaction::commit();
}
