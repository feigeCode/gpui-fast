//! Presenting CPU frames on Wayland through `wl_shm` buffers.
//!
//! STUB: interface only; to be implemented.

use gpui_wgpu::WgpuRenderer;
use wayland_client::protocol::wl_surface;

use crate::linux::Globals;

/// `renderer`, letting it present the frames it draws on the CPU on
/// `surface`.
pub(crate) fn with_presenter(
    renderer: WgpuRenderer,
    _globals: &Globals,
    _surface: &wl_surface::WlSurface,
) -> WgpuRenderer {
    renderer
}
