//! Showing a frame drawn on the CPU through the GPU ("blit"): the frame's
//! changed rectangles are uploaded to a texture the size of the surface,
//! which a tiny pipeline copies to the swapchain image. It still wakes the
//! GPU, but for a texture upload and one full-screen triangle, without
//! recording the scene; and it needs nothing from the platform, so it works
//! on every window wgpu draws, whatever the compositor or driver.
//!
//! The canvas's `0xAARRGGBB` pixels are, on a little-endian machine, BGRA
//! bytes: the texture is `Bgra8Unorm` whatever the target's format, and the
//! shader writes the loaded values unchanged (no blending), so each byte
//! reaches the target as it is in the canvas.

use std::sync::Arc;

use gpui::{Bounds, DevicePixels};

/// The format CPU frames are uploaded in.
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// The GPU resources of the blit path, made on first use.
#[derive(Default)]
pub(crate) struct Blit {
    device: Option<Arc<wgpu::Device>>,
    layout: Option<wgpu::BindGroupLayout>,
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    texture: Option<BlitTexture>,
}

struct BlitTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl Blit {
    /// Drops the texture; the next frame uploads its canvas whole.
    pub(crate) fn release_texture(this: &mut Self) {
        this.texture = None;
    }

    /// Forgets resources made on another device (after a device loss).
    fn use_device(this: &mut Self, device: &Arc<wgpu::Device>) {
        if this
            .device
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, device))
        {
            *this = Blit {
                device: Some(device.clone()),
                ..Blit::default()
            };
        }
    }

    /// Uploads `rects` of `pixels`, a frame of `width` × `height`, to the
    /// texture: all of it when the texture is new.
    pub(crate) fn upload(
        this: &mut Self,
        device: &Arc<wgpu::Device>,
        queue: &wgpu::Queue,
        pixels: &[u32],
        width: u32,
        height: u32,
        rects: &[Bounds<DevicePixels>],
    ) {
        Self::use_device(this, device);
        let fresh = this
            .texture
            .as_ref()
            .is_none_or(|texture| texture.width != width || texture.height != height);
        if fresh {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("cpu_frame"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEXTURE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cpu_frame"),
                layout: Self::layout(this, device),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            this.texture = Some(BlitTexture {
                texture,
                bind_group,
                width,
                height,
            });
        }
        let Some(texture) = &this.texture else {
            return;
        };
        let bytes: &[u8] = bytemuck::cast_slice(pixels);
        let whole = [Bounds {
            origin: Default::default(),
            size: gpui::size(DevicePixels(width as i32), DevicePixels(height as i32)),
        }];
        let rects = if fresh { &whole[..] } else { rects };
        let stride = width as usize * 4;
        for rect in rects {
            let x = rect.origin.x.0.clamp(0, width as i32) as u32;
            let y = rect.origin.y.0.clamp(0, height as i32) as u32;
            let right = (rect.origin.x.0 + rect.size.width.0).clamp(0, width as i32) as u32;
            let bottom = (rect.origin.y.0 + rect.size.height.0).clamp(0, height as i32) as u32;
            if right <= x || bottom <= y {
                continue;
            }
            let offset = y as usize * stride + x as usize * 4;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes[offset..],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: right - x,
                    height: bottom - y,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Records copying the texture to `target`, of `format`. Does nothing
    /// before the first upload.
    pub(crate) fn draw(
        this: &mut Self,
        device: &Arc<wgpu::Device>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) {
        Self::use_device(this, device);
        if this.texture.is_none() {
            return;
        }
        if this
            .pipeline
            .as_ref()
            .is_none_or(|(pipeline_format, _)| *pipeline_format != format)
        {
            let pipeline = Self::create_pipeline(this, device, format);
            this.pipeline = Some((format, pipeline));
        }
        let (Some((_, pipeline)), Some(texture)) = (&this.pipeline, &this.texture) else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cpu_frame_blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &texture.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn layout<'a>(this: &'a mut Self, device: &wgpu::Device) -> &'a wgpu::BindGroupLayout {
        this.layout.get_or_insert_with(|| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("cpu_frame_blit"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            })
        })
    }

    fn create_pipeline(
        this: &mut Self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cpu_frame_blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cpu_frame_blit"),
            bind_group_layouts: &[Some(Self::layout(this, device))],
            immediate_size: 0,
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cpu_frame_blit"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_blit"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_blit"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Blit;
    use crate::WgpuContext;
    use crate::fast::adaptive::region::rect;

    /// The pixels of `texture`, 4 bytes each, row by row, as stored.
    fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
        let (width, height) = (texture.width(), texture.height());
        let row = width * 4;
        let padded_row = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit_readback"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |result| result.expect("mapped"));
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("polled");
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::new();
        for y in 0..height {
            let start = (y * padded_row) as usize;
            pixels.extend_from_slice(&mapped[start..start + row as usize]);
        }
        pixels
    }

    fn blit_into(
        blit: &mut Blit,
        device: &Arc<wgpu::Device>,
        queue: &wgpu::Queue,
        target: &wgpu::Texture,
    ) -> Vec<u8> {
        let view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        Blit::draw(blit, device, &mut encoder, &view, target.format());
        queue.submit(std::iter::once(encoder.finish()));
        read_back(device, queue, target)
    }

    /// The canvas's pixels as the target stores them.
    fn expected(pixels: &[u32], format: wgpu::TextureFormat) -> Vec<u8> {
        pixels
            .iter()
            .flat_map(|pixel| {
                let [b, g, r, a] = pixel.to_le_bytes();
                match format {
                    wgpu::TextureFormat::Rgba8Unorm => [r, g, b, a],
                    _ => [b, g, r, a],
                }
            })
            .collect()
    }

    #[test]
    fn blit_copies_the_canvas_exactly() {
        let Ok((context, _)) = WgpuContext::new_headless() else {
            eprintln!("skipped: no wgpu adapter");
            return;
        };
        let (device, queue) = (&context.device, &context.queue);
        let (width, height) = (67u32, 45u32);
        // Every byte value in every channel, premultiplied or not.
        let mut pixels: Vec<u32> = (0..width * height)
            .map(|ix| ix.wrapping_mul(2_654_435_761))
            .collect();
        for format in [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ] {
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("blit_target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mut blit = Blit::default();
            // A new texture takes the whole canvas, whatever the rects.
            Blit::upload(&mut blit, device, queue, &pixels, width, height, &[]);
            assert_eq!(
                blit_into(&mut blit, device, queue, &target),
                expected(&pixels, format)
            );

            // Later frames upload only their rects.
            let changed = [rect(3, 4, 10, 7), rect(60, 40, 20, 20)];
            let before = pixels.clone();
            for rect in &changed {
                for y in rect.origin.y.0..(rect.origin.y.0 + rect.size.height.0).min(height as i32)
                {
                    for x in
                        rect.origin.x.0..(rect.origin.x.0 + rect.size.width.0).min(width as i32)
                    {
                        pixels[(y as u32 * width + x as u32) as usize] ^= 0x5a5a_5a5a;
                    }
                }
            }
            Blit::upload(&mut blit, device, queue, &pixels, width, height, &changed);
            assert_eq!(
                blit_into(&mut blit, device, queue, &target),
                expected(&pixels, format)
            );
            // Pixels outside the rects are not uploaded.
            let mut stale = before.clone();
            stale[0] ^= 1;
            Blit::upload(
                &mut blit,
                device,
                queue,
                &stale,
                width,
                height,
                &changed[..1],
            );
            let mut shown = pixels.clone();
            for rect in &changed[..1] {
                for y in rect.origin.y.0..rect.origin.y.0 + rect.size.height.0 {
                    for x in rect.origin.x.0..rect.origin.x.0 + rect.size.width.0 {
                        let ix = (y as u32 * width + x as u32) as usize;
                        shown[ix] = stale[ix];
                    }
                }
            }
            assert_eq!(
                blit_into(&mut blit, device, queue, &target),
                expected(&shown, format)
            );
            pixels = before;
        }
    }
}
