//! A frame's batches drawn with state set only when it changes.
//!
//! Upstream's `DirectXRenderer::render` draws each batch through its
//! `PipelineState`, which binds the whole pipeline, the sampler and the
//! texture, and maps the batch constant buffer, for every batch; each path
//! batch also collects its vertices and sprites into new `Vec`s. This draws
//! the same batches in the same order with the same draws, but binds through
//! [`DrawState`], which skips what is already bound, and collects path
//! vertices and sprites into buffers kept from frame to frame.
//!
//! Upstream's loop still runs while a graphics debugger such as PIX captures
//! the frame, so that each batch keeps its labeled event.

use std::slice;

use anyhow::{Context as _, Result};
use gpui::{Path, PrimitiveBatch, ScaledPixels, Scene};
use windows::Win32::Graphics::{
    Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP},
    Direct3D11::{ID3D11Buffer, ID3D11DeviceContext, ID3D11SamplerState, ID3D11ShaderResourceView},
};

use crate::DirectXRenderer;
use crate::directx_renderer::{
    DirectXResources, PathRasterizationSprite, PathSprite, PipelineState, RENDER_TARGET_FORMAT,
};
use crate::fast::draw_state::DrawState;
use crate::fast::globals::UploadedGlobals;

/// The renderer's gpui-fast state, kept from frame to frame.
#[derive(Default)]
pub(crate) struct FrameState {
    pub(crate) globals: UploadedGlobals,
    state: DrawState,
    path_vertices: Vec<PathRasterizationSprite>,
    path_sprites: Vec<PathSprite>,
}

/// Draws `scene`'s batches in place of `DirectXRenderer::render`'s loop, once
/// the scene's instances are uploaded. Returns `false`, having drawn nothing,
/// when a graphics debugger is capturing and upstream's labeled loop should
/// run instead.
pub(crate) fn draw_batches(renderer: &mut DirectXRenderer, scene: &Scene) -> Result<bool> {
    let devices = renderer.devices.as_ref().context("devices missing")?;
    let capturing = devices
        .annotation
        .as_ref()
        .is_some_and(|annotation| unsafe { annotation.GetStatus().as_bool() });
    if capturing {
        return Ok(false);
    }
    let resources = renderer.resources.as_ref().context("resources missing")?;
    let pipelines = &mut renderer.pipelines;
    let globals = &renderer.globals;
    let batch_params = globals
        .batch_params_buffer
        .as_ref()
        .context("batch params buffer missing")?;
    let frame = &mut renderer.fast_frame;
    let device_context = &devices.device_context;
    frame.state.forget();

    let mut draw = Draw {
        device_context,
        state: &mut frame.state,
        batch_params,
        sampler: &globals.sampler,
    };
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Shadows(range) => {
                draw.instances(&pipelines.shadow_pipeline, range.start, range.len(), None)
            }
            PrimitiveBatch::Quads(range) => {
                draw.instances(&pipelines.quad_pipeline, range.start, range.len(), None)
            }
            PrimitiveBatch::Paths(range) => {
                let paths = &scene.paths[range];
                if paths.is_empty() {
                    continue;
                }
                rasterize_paths(
                    &mut draw,
                    &devices.device,
                    resources,
                    &mut pipelines.path_rasterization_pipeline,
                    &mut frame.path_vertices,
                    paths,
                )
                .and_then(|()| {
                    composite_paths(
                        &mut draw,
                        &devices.device,
                        resources,
                        &mut pipelines.path_sprite_pipeline,
                        &mut frame.path_sprites,
                        paths,
                    )
                })
            }
            PrimitiveBatch::Underlines(range) => draw.instances(
                &pipelines.underline_pipeline,
                range.start,
                range.len(),
                None,
            ),
            PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                let [view] = renderer.atlas.get_texture_view(texture_id);
                let pipeline = &pipelines.mono_sprites;
                draw.instances(pipeline, range.start, range.len(), Some(&view))
            }
            PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                let [view] = renderer.atlas.get_texture_view(texture_id);
                let pipeline = &pipelines.subpixel_sprites;
                draw.instances(pipeline, range.start, range.len(), Some(&view))
            }
            PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                let [view] = renderer.atlas.get_texture_view(texture_id);
                let pipeline = &pipelines.poly_sprites;
                draw.instances(pipeline, range.start, range.len(), Some(&view))
            }
            // Upstream draws nothing for surfaces on Windows either.
            PrimitiveBatch::Surfaces(_) => Ok(()),
        }
        .with_context(|| scene_too_large(scene))?;
    }
    Ok(true)
}

/// What every batch's draw needs.
struct Draw<'a> {
    device_context: &'a ID3D11DeviceContext,
    state: &'a mut DrawState,
    batch_params: &'a ID3D11Buffer,
    sampler: &'a Option<ID3D11SamplerState>,
}

impl Draw<'_> {
    /// Upstream's `PipelineState::draw_range` (and `draw_range_with_texture`
    /// when `texture` is given).
    fn instances<T>(
        &mut self,
        pipeline: &PipelineState<T>,
        first_instance: usize,
        instance_count: usize,
        texture: Option<&Option<ID3D11ShaderResourceView>>,
    ) -> Result<()> {
        if instance_count == 0 {
            return Ok(());
        }
        anyhow::ensure!(
            first_instance + instance_count <= pipeline.buffer_size,
            "DirectX instance range exceeds the {} buffer",
            pipeline.label
        );
        let context = self.device_context;
        self.state
            .set_batch_start(context, self.batch_params, first_instance as u32)?;
        self.state
            .set_pipeline(context, pipeline, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
        if let Some(texture) = texture {
            self.state.set_texture(context, texture, self.sampler);
        }
        unsafe { context.DrawInstanced(4, instance_count as u32, 0, 0) };
        Ok(())
    }
}

/// Upstream's `DirectXRenderer::draw_paths_to_intermediate`.
fn rasterize_paths(
    draw: &mut Draw<'_>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    resources: &DirectXResources,
    pipeline: &mut PipelineState<PathRasterizationSprite>,
    vertices: &mut Vec<PathRasterizationSprite>,
    paths: &[Path<ScaledPixels>],
) -> Result<()> {
    let context = draw.device_context;
    unsafe {
        context.ClearRenderTargetView(
            resources.path_intermediate_msaa_view.as_ref().unwrap(),
            &[0.0; 4],
        );
        context.OMSetRenderTargets(
            Some(slice::from_ref(&resources.path_intermediate_msaa_view)),
            None,
        );
    }
    draw.state.forget_views();

    vertices.clear();
    for path in paths {
        let bounds = path.clipped_bounds();
        vertices.extend(path.vertices.iter().map(|v| PathRasterizationSprite {
            xy_position: v.xy_position,
            st_position: v.st_position,
            color: path.color,
            bounds,
        }));
    }
    pipeline.update_buffer(device, context, vertices)?;
    draw.state
        .set_pipeline(context, pipeline, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    unsafe {
        context.DrawInstanced(vertices.len() as u32, 1, 0, 0);
        context.ResolveSubresource(
            &resources.path_intermediate_texture,
            0,
            &resources.path_intermediate_msaa_texture,
            0,
            RENDER_TARGET_FORMAT,
        );
        context.OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
    }
    draw.state.forget_views();
    Ok(())
}

/// Upstream's `DirectXRenderer::draw_paths_from_intermediate`.
fn composite_paths(
    draw: &mut Draw<'_>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    resources: &DirectXResources,
    pipeline: &mut PipelineState<PathSprite>,
    sprites: &mut Vec<PathSprite>,
    paths: &[Path<ScaledPixels>],
) -> Result<()> {
    let (Some(first_path), Some(last_path)) = (paths.first(), paths.last()) else {
        return Ok(());
    };

    // Each pixel must be copied only once, in case of transparent paths.
    // Paths of one draw order are disjoint and copied one by one; a batch
    // mixing draw orders is copied as one rect spanning all of them.
    sprites.clear();
    if last_path.order == first_path.order {
        sprites.extend(paths.iter().map(|path| PathSprite {
            bounds: path.clipped_bounds(),
        }));
    } else {
        let mut bounds = first_path.clipped_bounds();
        for path in &paths[1..] {
            bounds = bounds.union(&path.clipped_bounds());
        }
        sprites.push(PathSprite { bounds });
    }

    let context = draw.device_context;
    pipeline.update_buffer(device, context, sprites)?;
    draw.state
        .set_pipeline(context, pipeline, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    draw.state
        .set_texture(context, &resources.path_intermediate_srv, draw.sampler);
    unsafe { context.DrawInstanced(4, sprites.len() as u32, 0, 0) };
    Ok(())
}

fn scene_too_large(scene: &Scene) -> String {
    format!(
        "scene too large:\
        {} paths, {} shadows, {} quads, {} underlines, {} mono, {} subpixel, {} poly, {} surfaces",
        scene.paths.len(),
        scene.shadows.len(),
        scene.quads.len(),
        scene.underlines.len(),
        scene.monochrome_sprites.len(),
        scene.subpixel_sprites.len(),
        scene.polychrome_sprites.len(),
        scene.surfaces.len(),
    )
}
