//! Presenting CPU frames on X11 with `PutImage`.
//!
//! STUB: interface only; to be implemented.

use std::rc::Rc;

use gpui_wgpu::WgpuRenderer;
use x11rb::protocol::xproto;
use x11rb::xcb_ffi::XCBConnection;

/// Lets `renderer` present the frames it draws on the CPU in `window`, of
/// `depth` bits per pixel.
pub(crate) fn install(
    _renderer: &mut WgpuRenderer,
    _xcb: &Rc<XCBConnection>,
    _window: xproto::Window,
    _depth: u8,
) {
}
