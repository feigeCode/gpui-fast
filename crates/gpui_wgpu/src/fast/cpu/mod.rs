//! Drawing frames on the CPU: a rasterizer that reproduces the wgpu shaders
//! (`raster`), the CPU copy of the sprite atlas it samples (`atlas`), and the
//! interface a platform implements to show the frames it draws without the
//! GPU ([`CpuPresenter`]).
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.

use gpui::{Bounds, DevicePixels};

pub(crate) mod atlas;
pub(crate) mod raster;

/// A frame drawn on the CPU, for a [`CpuPresenter`] to show.
pub struct CpuFrame<'a> {
    /// The frame's pixels, row by row, `width` per row: premultiplied
    /// `0xAARRGGBB`, the layout of `wl_shm`'s `ARGB8888` and of a 32-bit X11
    /// `ZPixmap` on a little-endian machine.
    pub pixels: &'a [u32],
    /// The frame's width in device pixels.
    pub width: u32,
    /// The frame's height in device pixels.
    pub height: u32,
    /// Where the frame differs from the frame presented before it, within
    /// the frame. Everything else is the same pixels as the last frame shown,
    /// whichever path drew it.
    pub damage: &'a [Bounds<DevicePixels>],
    /// Whether the window is opaque: the alpha channel is to be ignored.
    pub opaque: bool,
}

/// Shows frames drawn on the CPU in a window, without the GPU.
pub trait CpuPresenter {
    /// Shows `frame`. The window's frame callback, if the platform asked for
    /// one before drawing, is to be committed with it.
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()>;

    /// Notes that the GPU presented the frames since the last call to
    /// `present`: the next frame presented here must be shown whole, not only
    /// its damage, wherever the platform cannot keep what it last showed.
    fn gpu_presented(&mut self) {}

    /// Releases the memory kept for presenting; the next frame is presented
    /// as if it were the first.
    fn release(&mut self) {}
}
