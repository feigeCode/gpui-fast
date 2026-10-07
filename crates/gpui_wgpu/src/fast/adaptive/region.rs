//! Regions of a frame, as a few rectangles in device pixels, merged as scene
//! damage merges them, and the extents of the sprites that sample atlas
//! rectangles written since the last frame.

use collections::FxHashMap;
use gpui::{AtlasTextureId, AtlasTile, Bounds, DevicePixels, Point, ScaledPixels, Scene, Size};

/// A rectangle within this many pixels of one already in a region is
/// unioned into it.
pub(crate) const MERGE_DISTANCE: i32 = 8;

/// Past this many rectangles, a region is unioned into one.
pub(crate) const MAX_RECTS: usize = 16;

/// A region of a frame: rectangles in device pixels, none empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Region {
    rects: Vec<Bounds<DevicePixels>>,
}

impl Region {
    pub(crate) fn rects(&self) -> &[Bounds<DevicePixels>] {
        &self.rects
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }

    /// The pixels the rectangles cover, counting overlaps once per rectangle.
    pub(crate) fn area(&self) -> i64 {
        self.rects.iter().map(area).sum()
    }

    /// Adds `rect`, clipped to `clip`, merging it with the rectangles within
    /// [`MERGE_DISTANCE`] of it.
    pub(crate) fn add(&mut self, rect: Bounds<DevicePixels>, clip: Bounds<DevicePixels>) {
        let mut rect = intersect(rect, clip);
        if is_empty(&rect) {
            return;
        }
        while let Some(ix) = self.rects.iter().position(|kept| near(kept, &rect)) {
            rect = union(self.rects.swap_remove(ix), rect);
        }
        self.rects.push(rect);
        if self.rects.len() > MAX_RECTS {
            let all = self.rects.drain(..).reduce(union).expect("not empty");
            self.rects.push(all);
        }
    }

    pub(crate) fn add_all(&mut self, rects: &[Bounds<DevicePixels>], clip: Bounds<DevicePixels>) {
        for rect in rects {
            self.add(*rect, clip);
        }
    }

    /// Makes the region `rect` alone.
    pub(crate) fn set(&mut self, rect: Bounds<DevicePixels>) {
        self.rects.clear();
        if !is_empty(&rect) {
            self.rects.push(rect);
        }
    }
}

/// The rectangle at the origin of `width` × `height` pixels.
pub(crate) fn whole(width: u32, height: u32) -> Bounds<DevicePixels> {
    rect(0, 0, width as i32, height as i32)
}

pub(crate) fn rect(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
    Bounds {
        origin: Point {
            x: DevicePixels(x),
            y: DevicePixels(y),
        },
        size: Size {
            width: DevicePixels(width),
            height: DevicePixels(height),
        },
    }
}

pub(crate) fn area(rect: &Bounds<DevicePixels>) -> i64 {
    rect.size.width.0.max(0) as i64 * rect.size.height.0.max(0) as i64
}

fn is_empty(rect: &Bounds<DevicePixels>) -> bool {
    rect.size.width.0 <= 0 || rect.size.height.0 <= 0
}

fn edges(rect: &Bounds<DevicePixels>) -> (i32, i32, i32, i32) {
    let x = rect.origin.x.0;
    let y = rect.origin.y.0;
    (x, y, x + rect.size.width.0, y + rect.size.height.0)
}

fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Bounds<DevicePixels> {
    rect(left, top, (right - left).max(0), (bottom - top).max(0))
}

fn intersect(a: Bounds<DevicePixels>, b: Bounds<DevicePixels>) -> Bounds<DevicePixels> {
    let (al, at, ar, ab) = edges(&a);
    let (bl, bt, br, bb) = edges(&b);
    from_edges(al.max(bl), at.max(bt), ar.min(br), ab.min(bb))
}

fn union(a: Bounds<DevicePixels>, b: Bounds<DevicePixels>) -> Bounds<DevicePixels> {
    let (al, at, ar, ab) = edges(&a);
    let (bl, bt, br, bb) = edges(&b);
    from_edges(al.min(bl), at.min(bt), ar.max(br), ab.max(bb))
}

fn near(a: &Bounds<DevicePixels>, b: &Bounds<DevicePixels>) -> bool {
    let (al, at, ar, ab) = edges(a);
    let (bl, bt, br, bb) = edges(b);
    al <= br + MERGE_DISTANCE
        && bl <= ar + MERGE_DISTANCE
        && at <= bb + MERGE_DISTANCE
        && bt <= ab + MERGE_DISTANCE
}

fn overlaps(a: &Bounds<DevicePixels>, b: &Bounds<DevicePixels>) -> bool {
    let (al, at, ar, ab) = edges(a);
    let (bl, bt, br, bb) = edges(b);
    al < br && bl < ar && at < bb && bt < ab
}

/// Adds to `region` the pixels the sprites of `scene` can touch whose tile
/// overlaps a rectangle of `writes`.
pub(crate) fn add_written_sprites(
    region: &mut Region,
    scene: &Scene,
    writes: &[(AtlasTextureId, Bounds<DevicePixels>)],
    clip: Bounds<DevicePixels>,
    by_texture: &mut FxHashMap<AtlasTextureId, Vec<Bounds<DevicePixels>>>,
) {
    if writes.is_empty() {
        return;
    }
    by_texture.values_mut().for_each(Vec::clear);
    for (id, bounds) in writes {
        by_texture.entry(*id).or_default().push(*bounds);
    }
    let written = |tile: &AtlasTile| {
        by_texture
            .get(&tile.texture_id)
            .is_some_and(|rects| rects.iter().any(|rect| overlaps(rect, &tile.bounds)))
    };
    for sprite in &scene.monochrome_sprites {
        if written(&sprite.tile) {
            let rect = sprite_extent(
                &sprite.bounds,
                &sprite.content_mask.bounds,
                Some(&sprite.transformation.rotation_scale),
                sprite.transformation.translation,
            );
            region.add(rect, clip);
        }
    }
    for sprite in &scene.subpixel_sprites {
        if written(&sprite.tile) {
            let rect = sprite_extent(
                &sprite.bounds,
                &sprite.content_mask.bounds,
                Some(&sprite.transformation.rotation_scale),
                sprite.transformation.translation,
            );
            region.add(rect, clip);
        }
    }
    for sprite in &scene.polychrome_sprites {
        if written(&sprite.tile) {
            let rect = sprite_extent(&sprite.bounds, &sprite.content_mask.bounds, None, [0.; 2]);
            region.add(rect, clip);
        }
    }
}

/// Whether a sprite of `scene` samples a texture `missing` reports.
pub(crate) fn samples_any(scene: &Scene, missing: impl Fn(AtlasTextureId) -> bool) -> bool {
    scene
        .monochrome_sprites
        .iter()
        .any(|sprite| missing(sprite.tile.texture_id))
        || scene
            .subpixel_sprites
            .iter()
            .any(|sprite| missing(sprite.tile.texture_id))
        || scene
            .polychrome_sprites
            .iter()
            .any(|sprite| missing(sprite.tile.texture_id))
}

/// The device pixels a sprite of `bounds`, clipped to `mask`, can touch:
/// its bounds transformed as the shaders transform them (`rotation_scale`
/// row-major, then `translation`), clipped to the mask, rounded out and
/// widened by a pixel for the edges' antialiasing.
fn sprite_extent(
    bounds: &Bounds<ScaledPixels>,
    mask: &Bounds<ScaledPixels>,
    rotation_scale: Option<&[[f32; 2]; 2]>,
    translation: [f32; 2],
) -> Bounds<DevicePixels> {
    let x0 = bounds.origin.x.0;
    let y0 = bounds.origin.y.0;
    let x1 = x0 + bounds.size.width.0;
    let y1 = y0 + bounds.size.height.0;
    let (mut left, mut top, mut right, mut bottom) = (x0, y0, x1, y1);
    if let Some(m) = rotation_scale {
        left = f32::INFINITY;
        top = f32::INFINITY;
        right = f32::NEG_INFINITY;
        bottom = f32::NEG_INFINITY;
        for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            let tx = m[0][0] * x + m[0][1] * y + translation[0];
            let ty = m[1][0] * x + m[1][1] * y + translation[1];
            left = left.min(tx);
            top = top.min(ty);
            right = right.max(tx);
            bottom = bottom.max(ty);
        }
    }
    left = left.max(mask.origin.x.0);
    top = top.max(mask.origin.y.0);
    right = right.min(mask.origin.x.0 + mask.size.width.0);
    bottom = bottom.min(mask.origin.y.0 + mask.size.height.0);
    if !(left < right && top < bottom) {
        return rect(0, 0, 0, 0);
    }
    let clamp = |v: f32| v.clamp(i32::MIN as f32 / 2., i32::MAX as f32 / 2.) as i32;
    from_edges(
        clamp(left.floor()) - 1,
        clamp(top.floor()) - 1,
        clamp(right.ceil()) + 1,
        clamp(bottom.ceil()) + 1,
    )
}

#[cfg(test)]
mod tests {
    use gpui::{
        AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask, MonochromeSprite, Point,
        PolychromeSprite, ScaledPixels, Scene, Size, TransformationMatrix,
    };

    use super::{MAX_RECTS, Region, add_written_sprites, rect, whole};

    #[test]
    fn merges_near_rects_and_caps_count() {
        let clip = whole(1000, 1000);
        let mut region = Region::default();
        region.add(rect(0, 0, 10, 10), clip);
        region.add(rect(18, 0, 10, 10), clip);
        assert_eq!(region.rects(), &[rect(0, 0, 28, 10)]);
        region.add(rect(100, 100, 10, 10), clip);
        assert_eq!(region.rects().len(), 2);
        // A rect bridging two kept ones merges all three.
        region.add(rect(20, 5, 85, 100), clip);
        assert_eq!(region.rects(), &[rect(0, 0, 110, 110)]);

        let mut region = Region::default();
        for i in 0..=MAX_RECTS as i32 {
            region.add(rect(i * 50, 0, 10, 10), clip);
        }
        assert_eq!(
            region.rects(),
            &[rect(0, 0, MAX_RECTS as i32 * 50 + 10, 10)]
        );
    }

    #[test]
    fn clips_and_drops_empty() {
        let mut region = Region::default();
        region.add(rect(-5, -5, 10, 10), whole(100, 100));
        region.add(rect(200, 0, 10, 10), whole(100, 100));
        assert_eq!(region.rects(), &[rect(0, 0, 5, 5)]);
        assert_eq!(region.area(), 25);
    }

    fn scaled(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: Point {
                x: ScaledPixels(x),
                y: ScaledPixels(y),
            },
            size: Size {
                width: ScaledPixels(w),
                height: ScaledPixels(h),
            },
        }
    }

    fn tile(index: u32, kind: AtlasTextureKind, x: i32, y: i32) -> AtlasTile {
        AtlasTile {
            texture_id: AtlasTextureId { index, kind },
            tile_id: gpui::TileId(0),
            padding: 0,
            bounds: rect(x, y, 16, 16),
        }
    }

    #[test]
    fn sprites_over_written_tiles_are_added() {
        let mut scene = Scene::default();
        let mut mono = MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: scaled(10.5, 20.0, 16.0, 16.0),
            content_mask: ContentMask {
                bounds: scaled(0.0, 0.0, 1000.0, 1000.0),
            },
            color: Default::default(),
            tile: tile(0, AtlasTextureKind::Monochrome, 0, 0),
            transformation: TransformationMatrix::unit(),
        };
        scene.monochrome_sprites.push(mono);
        mono.tile = tile(0, AtlasTextureKind::Monochrome, 32, 0);
        mono.bounds = scaled(300.0, 300.0, 16.0, 16.0);
        scene.monochrome_sprites.push(mono);
        // Transformed: scaled by 2 and moved by 100.
        mono.tile = tile(0, AtlasTextureKind::Monochrome, 64, 0);
        mono.bounds = scaled(0.0, 0.0, 16.0, 16.0);
        mono.transformation = TransformationMatrix {
            rotation_scale: [[2.0, 0.0], [0.0, 2.0]],
            translation: [100.0, 50.0],
        };
        scene.monochrome_sprites.push(mono);
        scene.polychrome_sprites.push(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: Default::default(),
            opacity: 1.0,
            bounds: scaled(500.0, 0.0, 16.0, 16.0),
            content_mask: ContentMask {
                bounds: scaled(0.0, 0.0, 508.0, 1000.0),
            },
            corner_radii: Default::default(),
            tile: tile(0, AtlasTextureKind::Polychrome, 0, 0),
        });

        let writes = [
            (
                AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                rect(0, 0, 16, 16),
            ),
            (
                AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                rect(70, 0, 4, 4),
            ),
            (
                AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Polychrome,
                },
                rect(8, 8, 1, 1),
            ),
        ];
        let mut region = Region::default();
        let mut scratch = Default::default();
        add_written_sprites(
            &mut region,
            &scene,
            &writes,
            whole(1000, 1000),
            &mut scratch,
        );
        let mut rects = region.rects().to_vec();
        rects.sort_by_key(|r| (r.origin.x.0, r.origin.y.0));
        assert_eq!(
            rects,
            vec![
                rect(9, 19, 19, 18),
                rect(99, 49, 34, 34),
                rect(499, 0, 10, 17),
            ]
        );
    }
}
