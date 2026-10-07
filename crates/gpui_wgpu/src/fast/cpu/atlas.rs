//! The CPU copy of the sprite atlas the CPU rasterizer samples, and the log
//! of what was written to it, for the renderer's damage.
//!
//! STUB: interface only; to be implemented.

use gpui::{AtlasTextureId, Bounds, DevicePixels};

use crate::fast::cpu::raster::{SpritePixels, TexturePixels};
use crate::wgpu_atlas::WgpuAtlasTexture;

/// A CPU copy of every atlas texture, kept as the atlas uploads to the GPU.
#[derive(Default)]
pub(crate) struct AtlasMirror {}

impl AtlasMirror {
    /// Copies `bytes`, uploaded to `bounds` of `texture`, as uploaded (before
    /// any swizzle), and logs the write.
    pub(crate) fn upload(
        _this: &mut Self,
        _texture: Option<&WgpuAtlasTexture>,
        _bounds: Bounds<DevicePixels>,
        _bytes: &[u8],
    ) {
    }

    /// Forgets a texture whose last tile was removed.
    pub(crate) fn remove_texture(_this: &mut Self, _id: AtlasTextureId) {}

    /// Forgets every texture, as the atlas does when cleared.
    pub(crate) fn clear(_this: &mut Self) {}

    /// The rectangles written since the last call, and whether the log
    /// overflowed or textures were cleared, so that every sprite must be
    /// taken as changed.
    pub(crate) fn take_writes(
        _this: &mut Self,
        _writes: &mut Vec<(AtlasTextureId, Bounds<DevicePixels>)>,
    ) -> bool {
        false
    }
}

impl SpritePixels for AtlasMirror {
    fn texture(&self, _id: AtlasTextureId) -> Option<TexturePixels<'_>> {
        None
    }
}
