// Modified by Longbridge for gpui-fast.
use anyhow::{Context as _, Result, ensure};
use etagere::BucketedAtlasAllocator;
use gpui::{
    AtlasBackend, AtlasKey, AtlasState, AtlasTextureId, AtlasTextureKind, AtlasTextureList,
    AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Size,
};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLResourceOptions, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use parking_lot::Mutex;
use std::{borrow::Cow, ptr::NonNull};

const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
    width: DevicePixels(1024),
    height: DevicePixels(1024),
};

// Color textures start smaller: image tiles are sparse and a 1024px first page
// wastes memory for icon-heavy but text-light surfaces.
const DEFAULT_COLOR_ATLAS_SIZE: Size<DevicePixels> = Size {
    width: DevicePixels(512),
    height: DevicePixels(512),
};

// Max texture size on all modern Apple GPUs. Anything bigger than that crashes in validateWithDevice.
const MAX_ATLAS_SIZE: Size<DevicePixels> = Size {
    width: DevicePixels(16384),
    height: DevicePixels(16384),
};

// Small images go to their own texture pages so a large render does not evict
// a whole shared page of icons.
const SMALL_IMAGE_TILE_MAX: i32 = 256;

const MAX_PENDING_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

pub struct MetalAtlas(Mutex<AtlasState<MetalAtlasTextures>>);

impl MetalAtlas {
    pub(crate) fn new(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        supports_shared_storage: bool,
    ) -> Self {
        MetalAtlas(Mutex::new(AtlasState::new(MetalAtlasTextures {
            device,
            command_queue,
            supports_shared_storage,
            monochrome_textures: AtlasTextureList::default(),
            polychrome_textures: AtlasTextureList::default(),
            image_textures: AtlasTextureList::default(),
            image_small_textures: AtlasTextureList::default(),
            pending_uploads: Vec::new(),
            pending_upload_bytes: 0,
        })))
    }

    /// Returns the GPU texture backing `id`, or `None` once every tile in it
    /// has been removed. A scene can still reference such a texture when a
    /// cached view replays a paint from before the image was dropped, so
    /// callers must skip those sprites rather than assume the texture exists.
    pub(crate) fn metal_texture(
        &self,
        id: AtlasTextureId,
    ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        Some(self.0.lock().backend.texture(id)?.metal_texture.clone())
    }

    /// Encodes and commits any queued uploads on their own command buffer.
    ///
    /// Uploads are consumed into a command buffer that is committed independently
    /// of the frame render pass, so a later drawing error that drops the render
    /// command buffer cannot lose already-accepted updates. Both command buffers
    /// share the renderer's queue, so the uploads still execute before the frame
    /// that samples the texture.
    pub(crate) fn commit_pending_uploads(&self) {
        let mut lock = self.0.lock();
        if lock.backend.pending_uploads.is_empty() {
            return;
        }
        let command_queue = lock.backend.command_queue.clone();
        let Some(command_buffer) = command_queue.commandBuffer() else {
            return;
        };
        lock.backend.encode_pending_locked(&command_buffer);
        drop(lock);
        command_buffer.commit();
    }

    #[cfg(test)]
    pub(crate) fn flush_pending_uploads(&self) {
        let mut lock = self.0.lock();
        if lock.backend.pending_uploads.is_empty() {
            return;
        }
        let command_queue = lock.backend.command_queue.clone();
        let Some(command_buffer) = command_queue.commandBuffer() else {
            return;
        };
        lock.backend.encode_pending_locked(&command_buffer);
        drop(lock);
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
    }
}

struct PendingUpload {
    texture_id: AtlasTextureId,
    /// Device-pixel destination origin of the upload within the atlas texture.
    origin: MTLOrigin,
    size: MTLSize,
    bytes_per_row: usize,
    data: Vec<u8>,
}

struct MetalAtlasTextures {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    supports_shared_storage: bool,
    monochrome_textures: AtlasTextureList<MetalAtlasTexture>,
    polychrome_textures: AtlasTextureList<MetalAtlasTexture>,
    image_textures: AtlasTextureList<MetalAtlasTexture>,
    image_small_textures: AtlasTextureList<MetalAtlasTexture>,
    pending_uploads: Vec<PendingUpload>,
    pending_upload_bytes: usize,
}

impl PlatformAtlas for MetalAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        self.0.lock().get_or_insert_with(key, build)
    }

    fn update(&self, key: &AtlasKey, bounds: Bounds<DevicePixels>, bytes: &[u8]) -> Result<()> {
        let mut lock = self.0.lock();
        let Some(tile) = lock.tile(key) else {
            return Ok(());
        };
        let Some(texture) = lock.backend.texture(tile.texture_id) else {
            return Ok(());
        };
        let bytes_per_pixel = texture.bytes_per_pixel();
        validate_upload(tile, bounds, bytes, bytes_per_pixel)?;
        let upload_bounds = Bounds {
            origin: Point {
                x: DevicePixels(
                    tile.bounds
                        .origin
                        .x
                        .0
                        .checked_add(bounds.origin.x.0)
                        .context("texture update horizontal origin overflow")?,
                ),
                y: DevicePixels(
                    tile.bounds
                        .origin
                        .y
                        .0
                        .checked_add(bounds.origin.y.0)
                        .context("texture update vertical origin overflow")?,
                ),
            },
            size: bounds.size,
        };
        lock.backend
            .queue_upload(tile, upload_bounds, bytes, bytes_per_pixel);
        Ok(())
    }

    fn resource_generation(&self) -> u64 {
        0
    }

    fn max_texture_size(&self) -> Option<Size<DevicePixels>> {
        Some(MAX_ATLAS_SIZE)
    }

    fn remove(&self, key: &AtlasKey) {
        self.0.lock().remove(key);
    }
}

impl AtlasBackend for MetalAtlasTextures {
    fn insert(
        &mut self,
        kind: AtlasTextureKind,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<AtlasTile> {
        let upload = AtlasUpload::new(kind, size, bytes)?;
        let tile = match kind {
            AtlasTextureKind::DynamicTexture => self
                .allocate_dedicated(size, kind)
                .context("allocating dedicated dynamic-texture")?,
            _ => self.allocate(size, kind).context("allocating atlas tile")?,
        };
        let texture = self
            .texture(tile.texture_id)
            .context("looking up allocated atlas texture")?;
        texture.upload(tile.bounds.origin, &upload)?;
        Ok(tile)
    }

    fn remove(&mut self, tile: AtlasTile) {
        let id = tile.texture_id;

        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome | AtlasTextureKind::DynamicTexture => {
                &mut self.polychrome_textures
            }
            AtlasTextureKind::Image => &mut self.image_textures,
            AtlasTextureKind::ImageSmall => &mut self.image_small_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        };

        let Some(texture_slot) = textures
            .textures
            .iter_mut()
            .find(|texture| texture.as_ref().is_some_and(|v| v.id == id))
        else {
            return;
        };

        let mut freed = false;
        if let Some(mut texture) = texture_slot.take() {
            texture.allocator.deallocate(tile.tile_id.into());
            texture.decrement_ref_count();
            if texture.is_unreferenced() {
                textures.free_list.push(id.index as usize);
                freed = true;
            } else {
                *texture_slot = Some(texture);
            }
        }

        if freed {
            self.pending_uploads
                .retain(|upload| upload.texture_id != id);
            self.pending_upload_bytes = self
                .pending_uploads
                .iter()
                .map(|upload| upload.data.len())
                .sum();
        }
    }

    fn update(
        &mut self,
        tile: AtlasTile,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let bytes_per_pixel = self
            .texture(tile.texture_id)
            .context("updated tile refers to a missing texture")?
            .bytes_per_pixel();
        self.queue_upload(tile, bounds, bytes, bytes_per_pixel);
        Ok(())
    }
}

impl MetalAtlasTextures {
    fn textures(&self, kind: AtlasTextureKind) -> &AtlasTextureList<MetalAtlasTexture> {
        match kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures,
            AtlasTextureKind::Polychrome | AtlasTextureKind::DynamicTexture => {
                &self.polychrome_textures
            }
            AtlasTextureKind::Image => &self.image_textures,
            AtlasTextureKind::ImageSmall => &self.image_small_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        }
    }

    fn textures_mut(&mut self, kind: AtlasTextureKind) -> &mut AtlasTextureList<MetalAtlasTexture> {
        match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome | AtlasTextureKind::DynamicTexture => {
                &mut self.polychrome_textures
            }
            AtlasTextureKind::Image => &mut self.image_textures,
            AtlasTextureKind::ImageSmall => &mut self.image_small_textures,
            AtlasTextureKind::Subpixel => unreachable!(),
        }
    }

    /// Inserts a dynamic texture as a dedicated texture sized exactly to it.
    fn allocate_dedicated(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Result<AtlasTile> {
        ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "dynamic texture dimensions must be positive"
        );
        ensure!(
            size.width <= MAX_ATLAS_SIZE.width && size.height <= MAX_ATLAS_SIZE.height,
            "dynamic texture exceeds maximum atlas size"
        );

        let texture = self.push_texture_with_size(size, texture_kind)?;
        texture
            .allocate(size)
            .context("allocating dedicated dynamic-texture tile")
    }

    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Result<AtlasTile> {
        let texture_kind = if texture_kind == AtlasTextureKind::Image
            && size.width.0 <= SMALL_IMAGE_TILE_MAX
            && size.height.0 <= SMALL_IMAGE_TILE_MAX
        {
            AtlasTextureKind::ImageSmall
        } else {
            texture_kind
        };

        if let Some(tile) = self
            .textures_mut(texture_kind)
            .iter_mut()
            .rev()
            .find_map(|texture| texture.allocate(size))
        {
            return Ok(tile);
        }

        let texture = self.push_texture(size, texture_kind)?;
        texture.allocate(size).context("allocating atlas tile")
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> Result<&mut MetalAtlasTexture> {
        let default_size = match kind {
            AtlasTextureKind::Monochrome => DEFAULT_ATLAS_SIZE,
            AtlasTextureKind::Polychrome
            | AtlasTextureKind::DynamicTexture
            | AtlasTextureKind::Image
            | AtlasTextureKind::ImageSmall => DEFAULT_COLOR_ATLAS_SIZE,
            AtlasTextureKind::Subpixel => unreachable!(),
        };
        let size = min_size.max(&default_size).min(&MAX_ATLAS_SIZE);
        self.push_texture_with_size(size, kind)
    }

    fn push_texture_with_size(
        &mut self,
        size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> Result<&mut MetalAtlasTexture> {
        let size = size.min(&MAX_ATLAS_SIZE);

        let texture_descriptor = MTLTextureDescriptor::new();
        // SAFETY: Both dimensions are clamped to the existing 1..=16384 atlas size limits.
        unsafe {
            texture_descriptor.setWidth(size.width.0 as usize);
            texture_descriptor.setHeight(size.height.0 as usize);
        }
        let pixel_format;
        let usage;
        match kind {
            AtlasTextureKind::Monochrome => {
                pixel_format = MTLPixelFormat::A8Unorm;
                usage = MTLTextureUsage::ShaderRead;
            }
            AtlasTextureKind::Polychrome
            | AtlasTextureKind::DynamicTexture
            | AtlasTextureKind::Image
            | AtlasTextureKind::ImageSmall => {
                pixel_format = MTLPixelFormat::BGRA8Unorm;
                usage = MTLTextureUsage::ShaderRead;
            }
            AtlasTextureKind::Subpixel => unreachable!(),
        }
        texture_descriptor.setPixelFormat(pixel_format);
        texture_descriptor.setUsage(usage);
        // Shared memory mode can be used only on Apple GPU families
        // https://developer.apple.com/documentation/metal/mtlresourceoptions/storagemodeshared
        texture_descriptor.setStorageMode(if self.supports_shared_storage {
            MTLStorageMode::Shared
        } else {
            MTLStorageMode::Managed
        });
        let metal_texture = self
            .device
            .newTextureWithDescriptor(&texture_descriptor)
            .context("allocating atlas texture")?;

        let textures = self.textures_mut(kind);
        let index = textures.free_list.pop().unwrap_or_else(|| {
            let index = textures.textures.len();
            textures.textures.push(None);
            index
        });

        let atlas_texture = MetalAtlasTexture {
            id: AtlasTextureId {
                index: index as u32,
                kind,
            },
            allocator: BucketedAtlasAllocator::new(size_to_etagere(size)),
            metal_texture,
            live_atlas_keys: 0,
        };

        textures.textures[index] = Some(atlas_texture);
        Ok(textures.textures[index]
            .as_mut()
            .expect("atlas texture was just inserted"))
    }

    fn texture(&self, id: AtlasTextureId) -> Option<&MetalAtlasTexture> {
        self.textures(id.kind)
            .textures
            .get(id.index as usize)?
            .as_ref()
    }

    /// Queues a dynamic-texture upload to be applied on the next frame's command buffer.
    fn queue_upload(
        &mut self,
        tile: AtlasTile,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
        bytes_per_pixel: u8,
    ) {
        let is_full_update = bounds.origin == tile.bounds.origin && bounds.size == tile.bounds.size;
        if is_full_update {
            // A full-texture upload supersedes every earlier upload for this texture.
            self.pending_uploads
                .retain(|upload| upload.texture_id != tile.texture_id);
            self.pending_upload_bytes = self
                .pending_uploads
                .iter()
                .map(|upload| upload.data.len())
                .sum();
        }

        let bytes_per_row = (bounds.size.width.0 as usize).saturating_mul(bytes_per_pixel as usize);
        self.pending_upload_bytes = self.pending_upload_bytes.saturating_add(bytes.len());
        self.pending_uploads.push(PendingUpload {
            texture_id: tile.texture_id,
            origin: MTLOrigin {
                x: bounds.origin.x.0 as usize,
                y: bounds.origin.y.0 as usize,
                z: 0,
            },
            size: MTLSize {
                width: bounds.size.width.0 as usize,
                height: bounds.size.height.0 as usize,
                depth: 1,
            },
            bytes_per_row,
            data: bytes.to_vec(),
        });

        if self.pending_upload_bytes > MAX_PENDING_UPLOAD_BYTES {
            let command_queue = self.command_queue.clone();
            if let Some(command_buffer) = command_queue.commandBuffer() {
                self.encode_pending_locked(&command_buffer);
                command_buffer.commit();
            }
        }
    }

    /// Encodes and clears queued uploads onto `command_buffer`.
    fn encode_pending_locked(&mut self, command_buffer: &ProtocolObject<dyn MTLCommandBuffer>) {
        let pending_uploads = std::mem::take(&mut self.pending_uploads);
        self.pending_upload_bytes = 0;
        if pending_uploads.is_empty() {
            return;
        }

        let Some(blit) = command_buffer.blitCommandEncoder() else {
            return;
        };
        let mut staging_buffers: Vec<Retained<ProtocolObject<dyn MTLBuffer>>> =
            Vec::with_capacity(pending_uploads.len());
        for upload in pending_uploads {
            let Some(texture) = self.texture(upload.texture_id) else {
                // The texture was removed before this frame; its queued pixels are
                // no longer needed.
                continue;
            };
            if upload.data.is_empty() {
                continue;
            }
            // SAFETY: The staging buffer copies `upload.data`, which stays alive for the
            // duration of the call and is retained by the Metal buffer.
            let Some(buffer) = (unsafe {
                self.device.newBufferWithBytes_length_options(
                    NonNull::from(upload.data.as_slice()).cast(),
                    upload.data.len(),
                    MTLResourceOptions::StorageModeShared,
                )
            }) else {
                continue;
            };
            // SAFETY: Dimensions, row stride and destination region were validated when the
            // upload was accepted, and the destination texture is live for the whole copy.
            unsafe {
                blit.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &buffer,
                    0,
                    upload.bytes_per_row,
                    0,
                    upload.size,
                    &texture.metal_texture,
                    0,
                    0,
                    upload.origin,
                );
            }
            staging_buffers.push(buffer);
        }
        blit.endEncoding();
    }
}

fn validate_upload(
    tile: AtlasTile,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
    bytes_per_pixel: u8,
) -> Result<()> {
    ensure!(
        bounds.origin.x.0 >= 0 && bounds.origin.y.0 >= 0,
        "texture update origin must be non-negative"
    );
    ensure!(
        bounds.size.width.0 > 0 && bounds.size.height.0 > 0,
        "texture update size must be positive"
    );
    let right = bounds
        .origin
        .x
        .0
        .checked_add(bounds.size.width.0)
        .context("texture update horizontal bounds overflow")?;
    let bottom = bounds
        .origin
        .y
        .0
        .checked_add(bounds.size.height.0)
        .context("texture update vertical bounds overflow")?;
    ensure!(
        right <= tile.bounds.size.width.0 && bottom <= tile.bounds.size.height.0,
        "texture update exceeds the allocated tile bounds"
    );
    let expected_len = usize::try_from(bounds.size.width.0)
        .ok()
        .and_then(|width| {
            usize::try_from(bounds.size.height.0)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel as usize))
        .context("texture update byte size overflow")?;
    ensure!(
        bytes.len() == expected_len,
        "texture update contains {} bytes, expected {expected_len}",
        bytes.len()
    );
    Ok(())
}

struct MetalAtlasTexture {
    id: AtlasTextureId,
    allocator: BucketedAtlasAllocator,
    metal_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    live_atlas_keys: u32,
}

impl MetalAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(size_to_etagere(size))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            bounds: Bounds {
                origin: point_from_etagere(allocation.rectangle.min),
                size,
            },
            padding: 0,
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn upload(&self, origin: Point<DevicePixels>, upload: &AtlasUpload<'_>) -> Result<()> {
        ensure!(
            origin.x.0 >= 0
                && origin.y.0 >= 0
                && (origin.x.0 as usize)
                    .checked_add(upload.size.width.0 as usize)
                    .is_some_and(|end| end <= self.metal_texture.width())
                && (origin.y.0 as usize)
                    .checked_add(upload.size.height.0 as usize)
                    .is_some_and(|end| end <= self.metal_texture.height()),
            "atlas upload is outside texture bounds"
        );
        let region = MTLRegion {
            origin: MTLOrigin {
                x: origin.x.0 as usize,
                y: origin.y.0 as usize,
                z: 0,
            },
            size: MTLSize {
                width: upload.size.width.0 as usize,
                height: upload.size.height.0 as usize,
                depth: 1,
            },
        };
        // SAFETY: Validated bytes cover every row of the checked region in this CPU-accessible texture.
        // The borrowed bytes remain live for the copy; GPU ordering is a separate caller requirement.
        unsafe {
            self.metal_texture
                .replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    NonNull::from(upload.bytes).cast(),
                    upload.bytes_per_row,
                );
        }
        Ok(())
    }

    fn bytes_per_pixel(&self) -> u8 {
        match self.metal_texture.pixelFormat() {
            MTLPixelFormat::A8Unorm | MTLPixelFormat::R8Unorm => 1,
            MTLPixelFormat::RGBA8Unorm | MTLPixelFormat::BGRA8Unorm => 4,
            _ => unreachable!(),
        }
    }

    fn decrement_ref_count(&mut self) {
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&mut self) -> bool {
        self.live_atlas_keys == 0
    }
}

struct AtlasUpload<'a> {
    size: Size<DevicePixels>,
    bytes: &'a [u8],
    bytes_per_row: usize,
}

impl<'a> AtlasUpload<'a> {
    fn new(kind: AtlasTextureKind, size: Size<DevicePixels>, bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "atlas dimensions must be positive"
        );
        let bytes_per_pixel = match kind {
            AtlasTextureKind::Monochrome => 1,
            AtlasTextureKind::Polychrome
            | AtlasTextureKind::DynamicTexture
            | AtlasTextureKind::Image
            | AtlasTextureKind::ImageSmall => 4,
            AtlasTextureKind::Subpixel => unreachable!(),
        };
        let bytes_per_row = (size.width.0 as usize)
            .checked_mul(bytes_per_pixel)
            .context("calculating atlas row byte count")?;
        let byte_count = bytes_per_row
            .checked_mul(size.height.0 as usize)
            .context("calculating atlas byte count")?;
        ensure!(
            bytes.len() >= byte_count,
            "atlas upload byte length is {}, expected at least {byte_count}",
            bytes.len()
        );
        Ok(Self {
            size,
            bytes,
            bytes_per_row,
        })
    }
}

fn size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
    etagere::Size::new(size.width.into(), size.height.into())
}

fn point_from_etagere(value: etagere::Point) -> Point<DevicePixels> {
    Point {
        x: DevicePixels::from(value.x),
        y: DevicePixels::from(value.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{DynamicTextureId, DynamicTextureParams, PlatformAtlas, point, size};
    use objc2_metal::{MTLCreateSystemDefaultDevice, MTLGPUFamily};
    use std::borrow::Cow;

    #[test]
    fn test_remove_clears_stale_keys_from_tiles_by_key() {
        let Some(atlas) = create_atlas() else {
            return;
        };

        let small = Size {
            width: DevicePixels(64),
            height: DevicePixels(64),
        };

        let key_a = make_image_key(1, 0);
        let key_b = make_image_key(2, 0);
        let key_c = make_image_key(3, 0);

        let tile_a = insert_tile(&atlas, key_a.clone(), small);
        let tile_b = insert_tile(&atlas, key_b.clone(), small);
        let tile_c = insert_tile(&atlas, key_c.clone(), small);

        assert_eq!(tile_a.texture_id, tile_b.texture_id);
        assert_eq!(tile_b.texture_id, tile_c.texture_id);

        // Remove A: texture still has B and C, so it stays.
        // The key for A must be removed from tiles_by_key.
        atlas.remove(&key_a);

        // Remove B: texture still has C.
        atlas.remove(&key_b);

        // Remove C: texture becomes unreferenced and is deleted.
        atlas.remove(&key_c);

        // Re-inserting A must allocate a fresh tile on a new texture,
        // NOT return a stale tile referencing the deleted texture.
        let tile_a2 = insert_tile(&atlas, key_a, small);
        assert_eq!(tile_a2.texture_id, tile_a.texture_id);

        // The texture must actually exist — this would panic before the fix.
        assert!(atlas.metal_texture(tile_a2.texture_id).is_some());
    }

    #[test]
    fn test_metal_texture_is_none_after_last_tile_removed() {
        let Some(atlas) = create_atlas() else {
            return;
        };

        let key = make_image_key(1, 0);
        let tile = insert_tile(
            &atlas,
            key.clone(),
            Size {
                width: DevicePixels(64),
                height: DevicePixels(64),
            },
        );
        assert!(atlas.metal_texture(tile.texture_id).is_some());

        // A scene built before the removal may still carry `tile`; looking its
        // texture up must report the gap instead of panicking.
        atlas.remove(&key);
        assert!(atlas.metal_texture(tile.texture_id).is_none());
    }

    #[test]
    fn test_remove_deallocates_tile_space_for_reuse() {
        let Some(atlas) = create_atlas() else {
            return;
        };

        let big = Size {
            width: DevicePixels(700),
            height: DevicePixels(700),
        };

        let big_key_a = make_image_key(2, 0);
        let big_key_b = make_image_key(3, 0);

        let tile_a = insert_tile(&atlas, big_key_a.clone(), big);
        assert_eq!(tile_a.texture_id.kind, AtlasTextureKind::Image);

        atlas.remove(&big_key_a);
        let tile_b = insert_tile(&atlas, big_key_b, big);
        assert_eq!(tile_b.texture_id, tile_a.texture_id);
    }

    #[test]
    fn test_remove_nonexistent_key_is_noop() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let key = make_image_key(999, 0);
        atlas.remove(&key);
    }

    #[test]
    fn test_invalid_upload_does_not_allocate() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let mut state = atlas.0.lock();
        for (kind, short_bytes, short_error) in [
            (
                AtlasTextureKind::Monochrome,
                &[][..],
                "atlas upload byte length is 0, expected at least 1",
            ),
            (
                AtlasTextureKind::Polychrome,
                &[0][..],
                "atlas upload byte length is 1, expected at least 4",
            ),
        ] {
            for (width, height, bytes, expected_error) in [
                (1, 1, short_bytes, short_error),
                (0, 1, &[][..], "atlas dimensions must be positive"),
                (1, 0, &[][..], "atlas dimensions must be positive"),
                (i32::MIN, 1, &[][..], "atlas dimensions must be positive"),
                (1, i32::MIN, &[][..], "atlas dimensions must be positive"),
            ] {
                assert_eq!(
                    state
                        .backend
                        .insert(kind, size(DevicePixels(width), DevicePixels(height)), bytes)
                        .err()
                        .map(|error| error.to_string()),
                    Some(expected_error.to_owned())
                );
                for textures in [
                    &state.backend.monochrome_textures,
                    &state.backend.polychrome_textures,
                ] {
                    assert_eq!((textures.textures.len(), textures.free_list.len()), (0, 0));
                }
            }
        }
    }

    #[test]
    fn test_texture_upload_preserves_pixel_layout() -> Result<()> {
        let Some(atlas) = create_atlas() else {
            return Ok(());
        };
        let mut state = atlas.0.lock();
        for (kind, bytes_per_pixel) in [
            (AtlasTextureKind::Monochrome, 1),
            (AtlasTextureKind::Polychrome, 4),
        ] {
            let texture = state
                .backend
                .push_texture(size(DevicePixels(8), DevicePixels(8)), kind)?;
            texture.upload(
                Point::default(),
                &AtlasUpload::new(
                    kind,
                    size(DevicePixels(8), DevicePixels(8)),
                    &vec![0; 8 * 8 * bytes_per_pixel],
                )?,
            )?;
            let pixels = (1..=3 * 2 * bytes_per_pixel)
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            texture.upload(
                point(DevicePixels(2), DevicePixels(3)),
                &AtlasUpload::new(kind, size(DevicePixels(3), DevicePixels(2)), &pixels)?,
            )?;
            let mut actual = vec![0; 8 * 8 * bytes_per_pixel];
            unsafe {
                texture
                    .metal_texture
                    .getBytes_bytesPerRow_fromRegion_mipmapLevel(
                        NonNull::from(actual.as_mut_slice()).cast(),
                        8 * bytes_per_pixel,
                        MTLRegion {
                            origin: MTLOrigin { x: 0, y: 0, z: 0 },
                            size: MTLSize {
                                width: 8,
                                height: 8,
                                depth: 1,
                            },
                        },
                        0,
                    );
            }
            let mut expected = vec![0; actual.len()];
            for row in 0..2 {
                let start = ((row + 3) * 8 + 2) * bytes_per_pixel;
                let source = row * 3 * bytes_per_pixel;
                expected[start..start + 3 * bytes_per_pixel]
                    .copy_from_slice(&pixels[source..source + 3 * bytes_per_pixel]);
            }
            assert_eq!(actual, expected);
        }
        Ok(())
    }

    #[test]
    fn dynamic_textures_use_exact_sized_dedicated_metal_textures() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let size = Size {
            width: DevicePixels(64),
            height: DevicePixels(32),
        };

        let first = insert_tile(&atlas, make_dynamic_texture_key(1), size);
        let second = insert_tile(&atlas, make_dynamic_texture_key(2), size);

        assert_ne!(first.texture_id, second.texture_id);
        let first_texture = atlas
            .metal_texture(first.texture_id)
            .expect("texture should exist while tiles reference it");
        assert_eq!(first_texture.width(), 64);
        assert_eq!(first_texture.height(), 32);
        assert_eq!(atlas.resource_generation(), 0);
    }

    #[test]
    fn dynamic_texture_updates_upload_only_the_requested_region() {
        let Some(atlas) = create_atlas() else {
            return;
        };
        let size = Size {
            width: DevicePixels(2),
            height: DevicePixels(2),
        };
        let key = make_dynamic_texture_key(3);
        let tile = insert_tile(&atlas, key.clone(), size);
        let dirty_pixel = [1, 2, 3, 4];

        atlas
            .update(
                &key,
                Bounds {
                    origin: Point::new(DevicePixels(1), DevicePixels(1)),
                    size: Size {
                        width: DevicePixels(1),
                        height: DevicePixels(1),
                    },
                },
                &dirty_pixel,
            )
            .expect("partial upload should succeed");

        // Uploads are applied by the next frame's command buffer.
        atlas.flush_pending_uploads();

        let texture = atlas
            .metal_texture(tile.texture_id)
            .expect("texture should exist while tiles reference it");
        let mut uploaded = [0u8; 16];
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::from(uploaded.as_mut_slice()).cast(),
                8,
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width: 2,
                        height: 2,
                        depth: 1,
                    },
                },
                0,
            );
        }
        assert_eq!(&uploaded[..12], &[0; 12]);
        assert_eq!(&uploaded[12..], &dirty_pixel);
    }

    #[test]
    fn dynamic_texture_updates_reject_invalid_regions_and_payloads() {
        let tile = AtlasTile {
            texture_id: AtlasTextureId {
                index: 0,
                kind: AtlasTextureKind::Polychrome,
            },
            tile_id: gpui::TileId(0),
            bounds: Bounds {
                origin: Point::new(DevicePixels(0), DevicePixels(0)),
                size: Size {
                    width: DevicePixels(2),
                    height: DevicePixels(2),
                },
            },
            padding: 0,
        };

        assert!(
            validate_upload(
                tile,
                Bounds {
                    origin: Point::new(DevicePixels(-1), DevicePixels(0)),
                    size: Size {
                        width: DevicePixels(1),
                        height: DevicePixels(1),
                    },
                },
                &[0; 4],
                4,
            )
            .is_err()
        );
        assert!(
            validate_upload(
                tile,
                Bounds {
                    origin: Point::new(DevicePixels(1), DevicePixels(1)),
                    size: Size {
                        width: DevicePixels(2),
                        height: DevicePixels(1),
                    },
                },
                &[0; 8],
                4,
            )
            .is_err()
        );
        assert!(
            validate_upload(
                tile,
                Bounds {
                    origin: Point::new(DevicePixels(0), DevicePixels(0)),
                    size: Size {
                        width: DevicePixels(1),
                        height: DevicePixels(1),
                    },
                },
                &[0; 3],
                4,
            )
            .is_err()
        );
    }

    fn create_atlas() -> Option<MetalAtlas> {
        // SAFETY: Metal is available on every supported macOS/iOS target and
        // the returned device is retained for the atlas lifetime.
        let device = unsafe { MTLCreateSystemDefaultDevice() }?;
        let command_queue = device.newCommandQueue()?;
        let supports_shared_storage =
            cfg!(target_os = "ios") || device.supportsFamily(MTLGPUFamily::Apple1);
        Some(MetalAtlas::new(device, command_queue, supports_shared_storage))
    }

    fn make_dynamic_texture_key(texture_id: usize) -> AtlasKey {
        AtlasKey::DynamicTexture(DynamicTextureParams {
            texture_id: DynamicTextureId(texture_id),
        })
    }

    fn make_image_key(image_id: usize, frame_index: usize) -> AtlasKey {
        AtlasKey::Image(gpui::RenderImageParams {
            image_id: gpui::ImageId(image_id),
            frame_index,
        })
    }

    fn insert_tile(atlas: &MetalAtlas, key: AtlasKey, size: Size<DevicePixels>) -> AtlasTile {
        atlas
            .get_or_insert_with(key, &mut || {
                let byte_count = (size.width.0 as usize) * (size.height.0 as usize) * 4;
                Ok(Some((size, Cow::Owned(vec![0u8; byte_count]))))
            })
            .expect("inserting atlas tile")
            .expect("retrieving inserted atlas tile")
    }
}
