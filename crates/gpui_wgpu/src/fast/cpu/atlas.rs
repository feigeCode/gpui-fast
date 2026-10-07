//! The CPU copy of the sprite atlas the CPU rasterizer samples, and the log
//! of what was written to it, for the renderer's damage.
//!
//! The atlas uploads a tile's pixels to the GPU once, when the tile is
//! inserted. The mirror keeps the same bytes, as uploaded (8-bit coverage for
//! monochrome textures, BGRA for color textures, before the swizzle the atlas
//! applies for an RGBA texture format), in a buffer per texture the size of
//! the texture. It only does so once a renderer that draws on the CPU enabled
//! it ([`AtlasMirror::enable`]), so that windows that never draw on the CPU
//! pay nothing.
//!
//! The scene diff cannot see atlas content: a texture freed and allocated
//! again reuses its id, and a tile freed and allocated again to another image
//! keeps its texture and bounds. So every write is logged (texture and
//! rectangle), and the renderer drains the log each frame and redraws the
//! sprites of the new scene that sample a written rectangle.

use std::mem;

use collections::{FxHashMap, FxHashSet};
use gpui::{AtlasTextureId, Bounds, DevicePixels};
use parking_lot::MappedMutexGuard;
use parking_lot::MutexGuard;

use crate::WgpuAtlas;
use crate::fast::cpu::raster::{SpritePixels, TexturePixels};
use crate::wgpu_atlas::WgpuAtlasTexture;

/// How many writes the log keeps between two frames. Past it, writes to a
/// texture already in the log are unioned into its entry, and a write to
/// another texture makes the log report that everything changed.
pub(crate) const MAX_LOGGED_WRITES: usize = 256;

/// A CPU copy of every atlas texture, kept as the atlas uploads to the GPU.
#[derive(Default)]
pub(crate) struct AtlasMirror {
    /// Whether uploads are copied. Off until a renderer drawing on the CPU
    /// shares this atlas.
    enabled: bool,
    textures: FxHashMap<AtlasTextureId, MirroredTexture>,
    /// Live textures uploaded to while the mirror was off: their pixels are
    /// unknown here, so scenes that sample them must be drawn on the GPU.
    missed: FxHashSet<AtlasTextureId>,
    writes: Vec<(AtlasTextureId, Bounds<DevicePixels>)>,
    /// The log overflowed or the atlas was cleared since the last
    /// `take_writes`: every sprite may have changed.
    everything: bool,
}

struct MirroredTexture {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    data: Vec<u8>,
}

impl AtlasMirror {
    /// Starts copying uploads. Textures uploaded before stay unknown
    /// ([`AtlasMirror::is_missing`]) until they are freed.
    pub(crate) fn enable(this: &mut Self) {
        this.enabled = true;
    }

    /// Stops copying uploads and frees the copies.
    pub(crate) fn disable(this: &mut Self) {
        if !this.enabled {
            return;
        }
        this.enabled = false;
        let ids: Vec<_> = this.textures.drain().map(|(id, _)| id).collect();
        this.missed.extend(ids);
        this.writes.clear();
        this.everything = true;
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(this: &Self) -> bool {
        this.enabled
    }

    /// Whether some live texture's pixels are unknown to the mirror.
    pub(crate) fn has_missing(this: &Self) -> bool {
        !this.missed.is_empty()
    }

    /// Whether texture `id` is live but its pixels are unknown to the mirror.
    pub(crate) fn is_missing(this: &Self, id: AtlasTextureId) -> bool {
        this.missed.contains(&id)
    }

    /// Copies `bytes`, uploaded to `bounds` of `texture`, as uploaded (before
    /// any swizzle), and logs the write.
    pub(crate) fn upload(
        this: &mut Self,
        texture: Option<&WgpuAtlasTexture>,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) {
        let Some(texture) = texture else {
            return;
        };
        if !this.enabled {
            this.missed.insert(texture.id);
            return;
        }
        Self::upload_raw(
            this,
            texture.id,
            texture.texture.width(),
            texture.texture.height(),
            texture.bytes_per_pixel() as u32,
            bounds,
            bytes,
        );
    }

    /// [`AtlasMirror::upload`] for a texture of `width` × `height` pixels of
    /// `bytes_per_pixel` bytes.
    pub(crate) fn upload_raw(
        this: &mut Self,
        id: AtlasTextureId,
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) {
        if !this.enabled {
            this.missed.insert(id);
            return;
        }
        if this.missed.contains(&id) {
            // Part of the texture is unknown; it stays so.
            return;
        }
        let texture = this.textures.entry(id).or_insert_with(|| MirroredTexture {
            width,
            height,
            bytes_per_pixel,
            data: vec![0; width as usize * height as usize * bytes_per_pixel as usize],
        });
        let x = bounds.origin.x.0.max(0) as usize;
        let y = bounds.origin.y.0.max(0) as usize;
        let row_len = bounds.size.width.0.max(0) as usize * texture.bytes_per_pixel as usize;
        let rows = bounds.size.height.0.max(0) as usize;
        let stride = texture.width as usize * texture.bytes_per_pixel as usize;
        let start_in_row = x * texture.bytes_per_pixel as usize;
        if row_len == 0 || start_in_row + row_len > stride || y + rows > texture.height as usize {
            log::error!("atlas upload {bounds:?} outside texture {id:?}");
            return;
        }
        for (row, source) in bytes.chunks_exact(row_len).take(rows).enumerate() {
            let start = (y + row) * stride + start_in_row;
            texture.data[start..start + row_len].copy_from_slice(source);
        }
        Self::log_write(this, id, bounds);
    }

    /// Forgets a texture whose last tile was removed. Its id may be reused
    /// by a new texture; sprites a scene still has over it are logged as
    /// changed, as the GPU no longer draws them.
    pub(crate) fn remove_texture(this: &mut Self, id: AtlasTextureId) {
        this.missed.remove(&id);
        if let Some(texture) = this.textures.remove(&id) {
            let bounds = Bounds {
                origin: Default::default(),
                size: gpui::Size {
                    width: DevicePixels(texture.width as i32),
                    height: DevicePixels(texture.height as i32),
                },
            };
            Self::log_write(this, id, bounds);
        }
    }

    /// Forgets every texture, as the atlas does when cleared.
    pub(crate) fn clear(this: &mut Self) {
        this.textures.clear();
        this.missed.clear();
        this.writes.clear();
        this.everything = true;
    }

    /// The rectangles written since the last call, and whether the log
    /// overflowed or textures were cleared, so that every sprite must be
    /// taken as changed.
    pub(crate) fn take_writes(
        this: &mut Self,
        writes: &mut Vec<(AtlasTextureId, Bounds<DevicePixels>)>,
    ) -> bool {
        writes.clear();
        mem::swap(writes, &mut this.writes);
        mem::take(&mut this.everything)
    }

    fn log_write(this: &mut Self, id: AtlasTextureId, bounds: Bounds<DevicePixels>) {
        if this.everything {
            return;
        }
        if this.writes.len() < MAX_LOGGED_WRITES {
            this.writes.push((id, bounds));
        } else if let Some((_, logged)) = this.writes.iter_mut().find(|(logged, _)| *logged == id) {
            *logged = logged.union(&bounds);
        } else {
            this.writes.clear();
            this.everything = true;
        }
    }
}

impl SpritePixels for AtlasMirror {
    fn texture(&self, id: AtlasTextureId) -> Option<TexturePixels<'_>> {
        let texture = self.textures.get(&id)?;
        Some(TexturePixels {
            width: texture.width,
            height: texture.height,
            bytes_per_pixel: texture.bytes_per_pixel,
            data: &texture.data,
        })
    }
}

impl WgpuAtlas {
    /// Locks the atlas and gives its CPU copy.
    pub(crate) fn cpu_mirror(&self) -> MappedMutexGuard<'_, AtlasMirror> {
        MutexGuard::map(self.0.lock(), |state| &mut state.backend.fast_cpu)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AtlasTextureKind, Point, Size};

    use super::{AtlasMirror, MAX_LOGGED_WRITES};
    use crate::fast::cpu::raster::SpritePixels;
    use gpui::{AtlasTextureId, Bounds, DevicePixels};

    fn id(index: u32, kind: AtlasTextureKind) -> AtlasTextureId {
        AtlasTextureId { index, kind }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: Point {
                x: DevicePixels(x),
                y: DevicePixels(y),
            },
            size: Size {
                width: DevicePixels(w),
                height: DevicePixels(h),
            },
        }
    }

    fn enabled() -> AtlasMirror {
        let mut mirror = AtlasMirror::default();
        AtlasMirror::enable(&mut mirror);
        mirror
    }

    #[test]
    fn copies_rows_into_lazily_created_textures() {
        let mut mirror = enabled();
        let mono = id(0, AtlasTextureKind::Monochrome);
        AtlasMirror::upload_raw(
            &mut mirror,
            mono,
            8,
            4,
            1,
            rect(2, 1, 3, 2),
            &[1, 2, 3, 4, 5, 6],
        );
        let texture = mirror.texture(mono).unwrap();
        assert_eq!(
            (texture.width, texture.height, texture.bytes_per_pixel),
            (8, 4, 1)
        );
        assert_eq!(&texture.data[8..16], &[0, 0, 1, 2, 3, 0, 0, 0]);
        assert_eq!(&texture.data[16..24], &[0, 0, 4, 5, 6, 0, 0, 0]);
        assert!(texture.data[..8].iter().all(|&b| b == 0));

        let color = id(0, AtlasTextureKind::Polychrome);
        let bgra: Vec<u8> = (0..8).collect();
        AtlasMirror::upload_raw(&mut mirror, color, 4, 4, 4, rect(3, 3, 1, 1), &bgra[..4]);
        AtlasMirror::upload_raw(&mut mirror, color, 4, 4, 4, rect(0, 0, 2, 1), &bgra);
        let texture = mirror.texture(color).unwrap();
        assert_eq!(&texture.data[..8], &bgra[..]);
        assert_eq!(&texture.data[(3 * 4 + 3) * 4..][..4], &bgra[..4]);
        // A write running off the texture is refused rather than wrapping.
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 4, 1, rect(7, 0, 2, 1), &[8, 8]);
        assert_eq!(mirror.texture(mono).unwrap().data[7], 0);
        let mut writes = Vec::new();
        assert!(!AtlasMirror::take_writes(&mut mirror, &mut writes));
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0], (mono, rect(2, 1, 3, 2)));
    }

    #[test]
    fn disabled_mirror_copies_nothing_and_notes_missed_textures() {
        let mut mirror = AtlasMirror::default();
        let mono = id(0, AtlasTextureKind::Monochrome);
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 8, 1, rect(0, 0, 1, 1), &[9]);
        assert!(mirror.texture(mono).is_none());
        AtlasMirror::enable(&mut mirror);
        assert!(AtlasMirror::is_missing(&mirror, mono));
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 8, 1, rect(1, 0, 1, 1), &[9]);
        assert!(
            mirror.texture(mono).is_none(),
            "a partly known texture stays unknown"
        );
        AtlasMirror::remove_texture(&mut mirror, mono);
        assert!(!AtlasMirror::has_missing(&mirror));
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 8, 1, rect(1, 0, 1, 1), &[9]);
        assert_eq!(mirror.texture(mono).unwrap().data[1], 9);
    }

    #[test]
    fn removed_texture_is_dropped_and_logged_whole() {
        let mut mirror = enabled();
        let mono = id(3, AtlasTextureKind::Monochrome);
        AtlasMirror::upload_raw(&mut mirror, mono, 16, 8, 1, rect(0, 0, 1, 1), &[1]);
        let mut writes = Vec::new();
        AtlasMirror::take_writes(&mut mirror, &mut writes);
        AtlasMirror::remove_texture(&mut mirror, mono);
        assert!(mirror.texture(mono).is_none());
        assert!(!AtlasMirror::take_writes(&mut mirror, &mut writes));
        assert_eq!(writes, vec![(mono, rect(0, 0, 16, 8))]);
        // The id reused by a new texture starts blank.
        AtlasMirror::upload_raw(&mut mirror, mono, 16, 8, 1, rect(5, 5, 1, 1), &[7]);
        let data = &mirror.texture(mono).unwrap().data;
        assert_eq!(data[0], 0);
        assert_eq!(data[5 * 16 + 5], 7);
    }

    #[test]
    fn clear_reports_everything_once() {
        let mut mirror = enabled();
        let mono = id(0, AtlasTextureKind::Monochrome);
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 8, 1, rect(0, 0, 1, 1), &[1]);
        AtlasMirror::clear(&mut mirror);
        assert!(mirror.texture(mono).is_none());
        let mut writes = Vec::new();
        assert!(AtlasMirror::take_writes(&mut mirror, &mut writes));
        assert!(writes.is_empty());
        assert!(!AtlasMirror::take_writes(&mut mirror, &mut writes));
    }

    #[test]
    fn log_overflow_unions_per_texture_then_reports_everything() {
        let mut mirror = enabled();
        let a = id(0, AtlasTextureKind::Monochrome);
        let b = id(1, AtlasTextureKind::Monochrome);
        for i in 0..MAX_LOGGED_WRITES as i32 + 10 {
            AtlasMirror::upload_raw(&mut mirror, a, 1024, 1024, 1, rect(i, 0, 1, 1), &[1]);
        }
        let mut writes = Vec::new();
        assert!(!AtlasMirror::take_writes(&mut mirror, &mut writes));
        assert_eq!(writes.len(), MAX_LOGGED_WRITES);
        assert_eq!(writes[0].1, rect(0, 0, MAX_LOGGED_WRITES as i32 + 10, 1));

        for i in 0..MAX_LOGGED_WRITES as i32 {
            AtlasMirror::upload_raw(&mut mirror, a, 1024, 1024, 1, rect(i, 0, 1, 1), &[1]);
        }
        AtlasMirror::upload_raw(&mut mirror, b, 1024, 1024, 1, rect(0, 0, 1, 1), &[1]);
        assert!(AtlasMirror::take_writes(&mut mirror, &mut writes));
        assert!(writes.is_empty());
    }

    #[test]
    fn disable_frees_copies() {
        let mut mirror = enabled();
        let mono = id(0, AtlasTextureKind::Monochrome);
        AtlasMirror::upload_raw(&mut mirror, mono, 8, 8, 1, rect(0, 0, 1, 1), &[1]);
        AtlasMirror::disable(&mut mirror);
        assert!(mirror.texture(mono).is_none());
        AtlasMirror::enable(&mut mirror);
        assert!(AtlasMirror::is_missing(&mirror, mono));
    }
}
