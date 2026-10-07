//! A CPU rasterizer for finished scenes that reproduces `shaders.wgsl`.
//!
//! STUB: interface only; to be implemented.

use gpui::{AtlasTextureId, Bounds, DevicePixels, Scene};

/// A frame in memory: premultiplied `0xAARRGGBB` pixels, row by row.
#[derive(Default)]
pub(crate) struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Canvas {
    /// A canvas of `width` × `height` transparent pixels.
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Canvas {
            width,
            height,
            pixels: vec![0; width as usize * height as usize],
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    pub(crate) fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }
}

/// What the GPU renderer draws with that the shaders read besides the scene.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RasterParams {
    pub(crate) gamma_ratios: [f32; 4],
    pub(crate) grayscale_enhanced_contrast: f32,
    pub(crate) subpixel_enhanced_contrast: f32,
    pub(crate) is_bgr: bool,
    /// Whether the target composites premultiplied (`GlobalParams::premultiplied_alpha`).
    pub(crate) premultiplied_alpha: bool,
    /// Whether subpixel sprites are drawn with dual-source blending, as the
    /// GPU renderer does when the adapter supports it.
    pub(crate) dual_source_blending: bool,
}

/// The pixels of an atlas texture, as they were uploaded.
pub(crate) struct TexturePixels<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// 1 for monochrome textures (`R8`), 4 for color textures (`BGRA8`).
    pub(crate) bytes_per_pixel: u32,
    pub(crate) data: &'a [u8],
}

/// Where sprites' pixels come from.
pub(crate) trait SpritePixels: Sync {
    fn texture(&self, id: AtlasTextureId) -> Option<TexturePixels<'_>>;
}

/// Why a scene must be drawn on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeedsGpu {
    /// The scene has surfaces (video frames), which only the GPU samples.
    Surfaces,
}

/// Whether the CPU can draw `scene` inside `regions`.
pub(crate) fn can_draw(scene: &Scene, _regions: &[Bounds<DevicePixels>]) -> Result<(), NeedsGpu> {
    if scene.surfaces.is_empty() {
        Ok(())
    } else {
        Err(NeedsGpu::Surfaces)
    }
}

/// Draws `scene` into `canvas` inside `regions` only, as the GPU renderer
/// draws it into a target cleared to transparent black: each region is
/// cleared, then every primitive that reaches it is drawn, clipped to it.
/// Large regions are drawn on up to `threads` threads.
pub(crate) fn draw(
    _canvas: &mut Canvas,
    _scene: &Scene,
    _regions: &[Bounds<DevicePixels>],
    _sprites: &dyn SpritePixels,
    _params: &RasterParams,
    _threads: usize,
) {
}
