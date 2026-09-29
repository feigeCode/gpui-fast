//! Glyph painting that works out a run's rendering once, not once a glyph.

use crate::{
    App, AtlasTile, Bounds, ContentMask, DecorationRun, DevicePixels, FontId, GlyphId, Hsla,
    IsZero, MonochromeSprite, Pixels, Point, RenderGlyphParams, SUBPIXEL_VARIANTS_X,
    SUBPIXEL_VARIANTS_Y, ScaledPixels, SubpixelSprite, TransformationMatrix, Window,
    util::round_half_toward_zero,
};
use anyhow::Result;
use std::borrow::Cow;

/// How the glyphs of a run are rendered: what painting a glyph needs that
/// depends on its run, not on the glyph. See [`Window::glyph_run_rendering`].
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct GlyphRunRendering {
    subpixel_rendering: bool,
    dilation: u8,
}

impl Window {
    /// How the glyphs of a run in `font_id` at `font_size` and in `color` are
    /// rendered, which [`Window::paint_glyph_in_run`] takes so that painting a
    /// line works it out once a run rather than once a glyph: it asks the
    /// window how it is drawn and converts the colour to find its dilation.
    pub(crate) fn glyph_run_rendering(
        &self,
        font_id: FontId,
        font_size: Pixels,
        color: Hsla,
    ) -> GlyphRunRendering {
        GlyphRunRendering {
            subpixel_rendering: self.should_use_subpixel_rendering(font_id, font_size),
            dilation: self.text_system().glyph_dilation_for_color(color),
        }
    }

    /// [`Window::paint_glyph`], for a glyph in a run whose rendering and
    /// snapped content mask the caller has already worked out.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_glyph_in_run(
        &mut self,
        origin: Point<Pixels>,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
        color: Hsla,
        rendering: GlyphRunRendering,
        content_mask: ContentMask<ScaledPixels>,
    ) -> Result<()> {
        self.invalidator.debug_assert_paint();

        let element_opacity = self.element_opacity();
        let scale_factor = self.scale_factor();
        let glyph_origin = origin.scale(scale_factor);

        let quantized_origin = Point::new(
            round_half_toward_zero(glyph_origin.x.0 * SUBPIXEL_VARIANTS_X as f32)
                / SUBPIXEL_VARIANTS_X as f32,
            round_half_toward_zero(glyph_origin.y.0 * SUBPIXEL_VARIANTS_Y as f32)
                / SUBPIXEL_VARIANTS_Y as f32,
        );
        let subpixel_variant = Point::new(
            (quantized_origin.x.fract() * SUBPIXEL_VARIANTS_X as f32) as u8,
            (quantized_origin.y.fract() * SUBPIXEL_VARIANTS_Y as f32) as u8,
        );
        let integer_origin = quantized_origin.map(|c| ScaledPixels(c.trunc()));
        let GlyphRunRendering {
            subpixel_rendering,
            dilation,
        } = rendering;
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size,
            subpixel_variant,
            scale_factor,
            is_emoji: false,
            subpixel_rendering,
            dilation,
        };

        let (raster_bounds, tile) = match self.fast_glyph_bounds.lookup(&params) {
            Some(kept) => kept,
            None => {
                let raster_bounds = self.text_system().raster_bounds(&params)?;
                self.fast_glyph_bounds.insert(&params, raster_bounds);
                (raster_bounds, None)
            }
        };
        if !raster_bounds.is_zero() {
            let tile = match tile {
                Some(tile) => tile,
                None => {
                    let tile = self
                        .sprite_atlas
                        .get_or_insert_with(&params.clone().into(), &mut || {
                            let (size, bytes) = self.text_system().rasterize_glyph(&params)?;
                            Ok(Some((size, Cow::Owned(bytes))))
                        })?
                        .expect("Callback above only errors or returns Some");
                    self.fast_glyph_bounds.insert_tile(&params, tile);
                    tile
                }
            };
            let bounds = Bounds {
                origin: integer_origin + raster_bounds.origin.map(Into::into),
                size: tile.bounds.size.map(Into::into),
            };

            if subpixel_rendering {
                self.next_frame.scene.insert_primitive(SubpixelSprite {
                    order: 0,
                    pad: 0,
                    bounds,
                    content_mask,
                    color: color.opacity(element_opacity),
                    tile,
                    transformation: TransformationMatrix::unit(),
                });
            } else {
                self.next_frame.scene.insert_primitive(MonochromeSprite {
                    order: 0,
                    pad: 0,
                    bounds,
                    content_mask,
                    color: color.opacity(element_opacity),
                    tile,
                    transformation: TransformationMatrix::unit(),
                });
            }
        }
        Ok(())
    }
}

/// Paints the glyphs of a line, working out what they share once rather than
/// once a glyph: the snapped content mask, which nothing painted along a line
/// changes, and the rendering of each run, which only changes with the run's
/// font or color.
pub(crate) struct LineGlyphPainter {
    snapped_content_mask: ContentMask<ScaledPixels>,
    run_rendering: Option<(FontId, Hsla, GlyphRunRendering)>,
}

impl LineGlyphPainter {
    pub(crate) fn new(window: &Window) -> Self {
        Self {
            snapped_content_mask: window.snapped_content_mask(),
            run_rendering: None,
        }
    }

    /// [`Window::paint_glyph`], for the next glyph of the line.
    pub(crate) fn paint_glyph(
        &mut self,
        window: &mut Window,
        origin: Point<Pixels>,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
        color: Hsla,
    ) -> Result<()> {
        let rendering = match self.run_rendering {
            Some((run_font_id, run_color, rendering))
                if run_font_id == font_id && run_color == color =>
            {
                rendering
            }
            _ => {
                let rendering = window.glyph_run_rendering(font_id, font_size, color);
                self.run_rendering = Some((font_id, color, rendering));
                rendering
            }
        };
        window.paint_glyph_in_run(
            origin,
            font_id,
            glyph_id,
            font_size,
            color,
            rendering,
            self.snapped_content_mask,
        )
    }
}

/// How many glyphs' raster bounds a window keeps at hand, as a power of two.
/// See [`GlyphBoundsCache`].
const GLYPH_BOUNDS_SLOT_BITS: u32 = 12;

/// The raster bounds of the glyphs a window painted lately, so painting a
/// glyph needn't ask the text system, which locks its map of every glyph's
/// raster bounds and hashes the glyph's whole description to look it up. A
/// glyph's raster bounds only depend on that description, so the answer is
/// the text system's own. Each glyph has one slot it can be kept in; a glyph
/// that needs a slot another holds takes it over.
///
/// A glyph's slot also keeps where the glyph is in the window's sprite atlas,
/// for the rest of the frame it was looked up in: the same digits are painted
/// in hundreds of places a frame, and each lookup locks the atlas and hashes
/// the glyph's description again. A tile is only kept for the frame, since
/// the atlas may be cleared when the frame is presented (after a run of GPU
/// errors, or when the device is lost), but glyphs are never removed from it
/// while a frame is painted.
///
/// The bounding boxes of the fonts lines were painted in lately, at the sizes
/// they were painted at, are kept here too. See [`bounding_box`].
pub(crate) struct GlyphBoundsCache {
    slots: Box<[Option<GlyphSlot>]>,
    /// Counts the frames the window finished painting, telling a tile looked
    /// up in this one from one looked up before.
    frame: u64,
    bounding_boxes: Vec<(FontId, Pixels, Bounds<Pixels>)>,
}

/// What [`GlyphBoundsCache`] keeps of a glyph.
#[derive(Clone)]
struct GlyphSlot {
    params: RenderGlyphParams,
    raster_bounds: Bounds<DevicePixels>,
    /// The glyph's tile, and the frame it was looked up in.
    tile: Option<(u64, AtlasTile)>,
}

impl Default for GlyphBoundsCache {
    fn default() -> Self {
        Self {
            slots: vec![None; 1 << GLYPH_BOUNDS_SLOT_BITS].into_boxed_slice(),
            frame: 0,
            bounding_boxes: Vec::new(),
        }
    }
}

impl GlyphBoundsCache {
    /// The slot a glyph is kept in, from everything that tells apart the
    /// glyphs a window paints: the same digit in two sizes, or in two colors
    /// dilated differently, or at another subpixel offset, would otherwise
    /// keep taking each other's slot.
    fn slot(params: &RenderGlyphParams) -> usize {
        const K: u64 = 0x9e37_79b9_7f4a_7c15;
        let words = [
            params.glyph_id.0 as u64 | (params.font_id.0 as u64) << 32,
            params.font_size.0.to_bits() as u64
                | (params.subpixel_variant.x as u64) << 32
                | (params.subpixel_variant.y as u64) << 40
                | (params.dilation as u64) << 48
                | (params.subpixel_rendering as u64) << 56
                | (params.is_emoji as u64) << 57,
            params.scale_factor.to_bits() as u64,
        ];
        let hash = words.iter().fold(0u64, |hash, word| {
            (hash.rotate_left(26) ^ word).wrapping_mul(K)
        });
        (hash >> (64 - GLYPH_BOUNDS_SLOT_BITS)) as usize
    }

    /// The glyph's raster bounds, if kept.
    #[cfg(test)]
    pub(crate) fn get(&self, params: &RenderGlyphParams) -> Option<Bounds<DevicePixels>> {
        self.lookup(params).map(|(bounds, _)| bounds)
    }

    /// The glyph's raster bounds, if kept, and its tile, if it was looked up
    /// this frame.
    pub(crate) fn lookup(
        &self,
        params: &RenderGlyphParams,
    ) -> Option<(Bounds<DevicePixels>, Option<AtlasTile>)> {
        match &self.slots[Self::slot(params)] {
            Some(slot) if slot.params == *params => Some((
                slot.raster_bounds,
                slot.tile
                    .and_then(|(frame, tile)| (frame == self.frame).then_some(tile)),
            )),
            _ => None,
        }
    }

    pub(crate) fn insert(&mut self, params: &RenderGlyphParams, bounds: Bounds<DevicePixels>) {
        self.slots[Self::slot(params)] = Some(GlyphSlot {
            params: params.clone(),
            raster_bounds: bounds,
            tile: None,
        });
    }

    /// Keeps the tile the glyph was found at in the sprite atlas this frame,
    /// if the glyph is kept.
    pub(crate) fn insert_tile(&mut self, params: &RenderGlyphParams, tile: AtlasTile) {
        if let Some(slot) = &mut self.slots[Self::slot(params)]
            && slot.params == *params
        {
            slot.tile = Some((self.frame, tile));
        }
    }

    /// Ends the frame the tiles kept were looked up in.
    pub(crate) fn finish_frame(&mut self) {
        self.frame += 1;
    }
}

/// How many fonts' bounding boxes [`bounding_box`] keeps, most recent last.
const BOUNDING_BOXES: usize = 16;

/// [`TextSystem::bounding_box`](crate::TextSystem::bounding_box), which
/// painting a line asks for once a run: it takes a lock and hashes the font to
/// find its metrics, where a window paints its text in a handful of fonts and
/// sizes, whose bounding boxes it keeps.
#[inline]
pub(crate) fn bounding_box(
    window: &mut Window,
    cx: &App,
    font_id: FontId,
    font_size: Pixels,
) -> Bounds<Pixels> {
    let boxes = &mut window.fast_glyph_bounds.bounding_boxes;
    if let Some((_, _, bounds)) = boxes
        .iter()
        .rev()
        .find(|(id, size, _)| *id == font_id && *size == font_size)
    {
        return *bounds;
    }
    let bounds = cx.text_system().bounding_box(font_id, font_size);
    if boxes.len() == BOUNDING_BOXES {
        boxes.remove(0);
    }
    boxes.push((font_id, font_size, bounds));
    bounds
}

/// Whether none of a line's decoration runs has a background, so painting its
/// background paints nothing: it needn't walk its glyphs, nor push a layer to
/// paint nothing in, which costs the scene a bounds tree insertion a line.
#[inline]
pub(crate) fn has_no_background(decoration_runs: &[DecorationRun]) -> bool {
    decoration_runs
        .iter()
        .all(|run| run.background_color.is_none())
}
