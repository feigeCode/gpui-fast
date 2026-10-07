//! Presenting CPU frames on X11 with `PutImage`.
//!
//! X11 keeps a window's contents, so a frame uploads only its damage: large
//! rectangles through an MIT-SHM segment the size of the frame, small ones,
//! or all of them without MIT-SHM, in `PutImage` requests split to stay under
//! the connection's maximum request length.
//!
//! When the GPU presents through the Present extension the window shows the
//! presented pixmap, which holds a whole frame; uploading the damage of the
//! next frame on top of it is what the CPU path needs.

use std::{os::fd::AsFd as _, rc::Rc};

use anyhow::Context as _;
use gpui::{Bounds, DevicePixels, point, size};
use gpui_wgpu::{CpuFrame, CpuPresenter, WgpuRenderer};
use x11rb::{
    connection::{Connection as _, DiscardMode, RequestConnection as _, RequestKind},
    protocol::{
        shm::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    xcb_ffi::XCBConnection,
};

use super::memfd::Mapping;
use super::region::Region;

/// The size of a `PutImage` request without its pixels, plus some slack.
const PUT_IMAGE_HEADER_BYTES: usize = 64;
/// Rectangles of more pixels than this go through MIT-SHM, when available.
const SHM_MIN_PIXELS: usize = 64 * 64;

/// Lets `renderer` present the frames it draws on the CPU in `window`, of
/// `depth` bits per pixel.
pub(crate) fn install(
    renderer: &mut WgpuRenderer,
    xcb: &Rc<XCBConnection>,
    window: xproto::Window,
    depth: u8,
) {
    match PutImagePresenter::new(xcb, window, depth) {
        Ok(presenter) => renderer.set_cpu_presenter(Box::new(presenter)),
        Err(err) => log::info!("CPU frames are not presented on this X11 window: {err:#}"),
    }
}

struct PutImagePresenter {
    xcb: Rc<XCBConnection>,
    window: xproto::Window,
    depth: u8,
    /// Whether the server takes pixels most significant byte first.
    big_endian: bool,
    gc: Option<xproto::Gcontext>,
    shm: Shm,
}

enum Shm {
    Unknown,
    Unavailable,
    Available(Option<Segment>),
}

/// An MIT-SHM segment the size of a frame, and the request after the last
/// `ShmPutImage` reading it: once that request's reply arrived, the server
/// read the segment and it can be written again.
struct Segment {
    seg: shm::Seg,
    map: Mapping,
    width: u32,
    height: u32,
    pending: Option<x11rb::connection::SequenceNumber>,
}

impl PutImagePresenter {
    fn new(xcb: &Rc<XCBConnection>, window: xproto::Window, depth: u8) -> anyhow::Result<Self> {
        anyhow::ensure!(depth == 24 || depth == 32, "depth {depth}");
        let setup = xcb.setup();
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .context("no pixmap format for the window's depth")?;
        anyhow::ensure!(
            format.bits_per_pixel == 32,
            "{} bits per pixel",
            format.bits_per_pixel
        );

        let visual = xcb
            .get_window_attributes(window)?
            .reply()
            .context("GetWindowAttributes")?
            .visual;
        let visual_type = setup
            .roots
            .iter()
            .flat_map(|screen| &screen.allowed_depths)
            .flat_map(|depth| &depth.visuals)
            .find(|visual_type| visual_type.visual_id == visual)
            .context("the window's visual is not on any screen")?;
        anyhow::ensure!(
            visual_type.class == xproto::VisualClass::TRUE_COLOR
                && visual_type.red_mask == 0xff0000
                && visual_type.green_mask == 0xff00
                && visual_type.blue_mask == 0xff,
            "visual of class {:?} and masks {:#x} {:#x} {:#x}",
            visual_type.class,
            visual_type.red_mask,
            visual_type.green_mask,
            visual_type.blue_mask,
        );

        Ok(Self {
            xcb: xcb.clone(),
            window,
            depth,
            big_endian: setup.image_byte_order == xproto::ImageOrder::MSB_FIRST,
            gc: None,
            shm: Shm::Unknown,
        })
    }

    fn gc(&mut self) -> anyhow::Result<xproto::Gcontext> {
        if let Some(gc) = self.gc {
            return Ok(gc);
        }
        let gc = self.xcb.generate_id()?;
        self.xcb
            .create_gc(
                gc,
                self.window,
                &xproto::CreateGCAux::new().graphics_exposures(0),
            )?
            .check()
            .context("CreateGC")?;
        self.gc = Some(gc);
        Ok(gc)
    }

    /// Whether the server has MIT-SHM 1.2 or later, which takes a file
    /// descriptor for a segment.
    fn has_shm(&mut self) -> bool {
        if let Shm::Unknown = self.shm {
            let available = self
                .xcb
                .extension_information(shm::X11_EXTENSION_NAME)
                .ok()
                .flatten()
                .is_some()
                && self
                    .xcb
                    .shm_query_version()
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some_and(|version| {
                        (version.major_version, version.minor_version) >= (1, 2)
                    });
            self.shm = if available {
                Shm::Available(None)
            } else {
                Shm::Unavailable
            };
        }
        matches!(self.shm, Shm::Available(_))
    }

    /// The segment for a frame of `width` × `height`, once the server has
    /// read what the last frame wrote into it.
    fn segment(&mut self, width: u32, height: u32) -> anyhow::Result<&mut Segment> {
        let Shm::Available(segment) = &mut self.shm else {
            anyhow::bail!("no MIT-SHM");
        };
        if segment
            .as_ref()
            .is_some_and(|segment| segment.width != width || segment.height != height)
        {
            if let Some(old) = segment.take() {
                old.free(&self.xcb);
            }
        }
        if segment.is_none() {
            let map = Mapping::new(width as usize * height as usize * 4)?;
            let seg = self.xcb.generate_id()?;
            let fd = map.fd().as_fd().try_clone_to_owned()?;
            self.xcb
                .shm_attach_fd(seg, fd, true)?
                .check()
                .context("ShmAttachFd")?;
            *segment = Some(Segment {
                seg,
                map,
                width,
                height,
                pending: None,
            });
        }
        let segment = segment.as_mut().unwrap();
        if let Some(pending) = segment.pending.take() {
            self.xcb
                .wait_for_reply_or_error(pending)
                .context("waiting for the server to read the MIT-SHM segment")?;
        }
        Ok(segment)
    }

    fn put_image(
        &self,
        gc: xproto::Gcontext,
        pixels: &[u32],
        width: u32,
        rect: Bounds<DevicePixels>,
        alpha: u32,
    ) -> anyhow::Result<()> {
        let max_pixels = self
            .xcb
            .maximum_request_bytes()
            .saturating_sub(PUT_IMAGE_HEADER_BYTES)
            / 4;
        let mut data = Vec::new();
        for part in split_rect(rect, max_pixels.max(1)) {
            data.clear();
            data.reserve(part.size.width.0 as usize * part.size.height.0 as usize * 4);
            for y in part.origin.y.0..part.origin.y.0 + part.size.height.0 {
                let start = y as usize * width as usize + part.origin.x.0 as usize;
                for &pixel in &pixels[start..start + part.size.width.0 as usize] {
                    let pixel = pixel | alpha;
                    data.extend_from_slice(&if self.big_endian {
                        pixel.to_be_bytes()
                    } else {
                        pixel.to_le_bytes()
                    });
                }
            }
            self.xcb
                .put_image(
                    xproto::ImageFormat::Z_PIXMAP,
                    self.window,
                    gc,
                    part.size.width.0 as u16,
                    part.size.height.0 as u16,
                    part.origin.x.0 as i16,
                    part.origin.y.0 as i16,
                    0,
                    self.depth,
                    &data,
                )?
                .ignore_error();
        }
        Ok(())
    }

    fn shm_put_images(
        &mut self,
        gc: xproto::Gcontext,
        frame: &CpuFrame<'_>,
        rects: &[Bounds<DevicePixels>],
        alpha: u32,
    ) -> anyhow::Result<()> {
        let (width, height) = (frame.width, frame.height);
        let (window, depth, big_endian) = (self.window, self.depth, self.big_endian);
        let segment = self.segment(width, height)?;
        let seg = segment.seg;
        let dst = segment.map.pixels_mut();
        for rect in rects {
            for y in rect.origin.y.0..rect.origin.y.0 + rect.size.height.0 {
                let start = y as usize * width as usize + rect.origin.x.0 as usize;
                let end = start + rect.size.width.0 as usize;
                for (dst, &src) in dst[start..end].iter_mut().zip(&frame.pixels[start..end]) {
                    let pixel = src | alpha;
                    *dst = if big_endian {
                        pixel.to_be()
                    } else {
                        pixel.to_le()
                    };
                }
            }
        }
        for rect in rects {
            self.xcb
                .shm_put_image(
                    window,
                    gc,
                    width as u16,
                    height as u16,
                    rect.origin.x.0 as u16,
                    rect.origin.y.0 as u16,
                    rect.size.width.0 as u16,
                    rect.size.height.0 as u16,
                    rect.origin.x.0 as i16,
                    rect.origin.y.0 as i16,
                    depth,
                    xproto::ImageFormat::Z_PIXMAP.into(),
                    false,
                    seg,
                    0,
                )?
                .ignore_error();
        }
        let cookie = self.xcb.get_input_focus()?;
        let pending = cookie.sequence_number();
        // The reply is waited for before the segment is written again.
        std::mem::forget(cookie);
        if let Shm::Available(Some(segment)) = &mut self.shm {
            segment.pending = Some(pending);
        }
        Ok(())
    }
}

impl CpuPresenter for PutImagePresenter {
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let (width, height) = (frame.width, frame.height);
        anyhow::ensure!(
            width > 0 && height > 0 && width <= u16::MAX as u32 && height <= u16::MAX as u32,
            "CPU frame of {width}×{height}"
        );
        anyhow::ensure!(
            frame.pixels.len() >= width as usize * height as usize,
            "CPU frame has fewer pixels than its size"
        );
        // Without the GPU's opaque composite alpha, a 32-bit window shows
        // the alpha channel: make it opaque.
        let alpha = if frame.opaque && self.depth == 32 {
            0xff00_0000
        } else {
            0
        };
        let gc = self.gc()?;
        let damage = Region::from_rects(frame.damage, width, height);
        let (large, small): (Vec<_>, Vec<_>) = damage.rects().iter().partition(|rect| {
            rect.size.width.0 as usize * rect.size.height.0 as usize >= SHM_MIN_PIXELS
        });

        let mut put = small;
        if !large.is_empty() {
            if self.has_shm() {
                if let Err(err) = self.shm_put_images(gc, &frame, &large, alpha) {
                    log::warn!("MIT-SHM upload failed, using PutImage: {err:#}");
                    if let Shm::Available(segment) = &mut self.shm {
                        if let Some(segment) = segment.take() {
                            segment.free(&self.xcb);
                        }
                    }
                    self.shm = Shm::Unavailable;
                    put.extend(large);
                }
            } else {
                put.extend(large);
            }
        }
        for rect in put {
            self.put_image(gc, frame.pixels, width, rect, alpha)?;
        }
        self.xcb.flush().context("X11 flush")?;
        Ok(())
    }

    fn release(&mut self) {
        if let Shm::Available(segment) = &mut self.shm {
            if let Some(segment) = segment.take() {
                segment.free(&self.xcb);
            }
        }
        if let Some(gc) = self.gc.take() {
            if let Ok(cookie) = self.xcb.free_gc(gc) {
                cookie.ignore_error();
            }
        }
        self.xcb.flush().ok();
    }
}

impl Drop for PutImagePresenter {
    fn drop(&mut self) {
        CpuPresenter::release(self);
    }
}

impl Segment {
    fn free(self, xcb: &XCBConnection) {
        if let Some(pending) = self.pending {
            xcb.discard_reply(
                pending,
                RequestKind::HasResponse,
                DiscardMode::DiscardReplyAndError,
            );
        }
        if let Ok(cookie) = xcb.shm_detach(self.seg) {
            cookie.ignore_error();
        }
    }
}

/// Splits `rect` into rectangles of at most `max_pixels` pixels: bands of
/// whole rows, or pieces of a row when one row is longer.
fn split_rect(
    rect: Bounds<DevicePixels>,
    max_pixels: usize,
) -> impl Iterator<Item = Bounds<DevicePixels>> {
    let (x, y) = (rect.origin.x.0, rect.origin.y.0);
    let (width, height) = (rect.size.width.0.max(0), rect.size.height.0.max(0));
    let max_pixels = max_pixels.clamp(1, i32::MAX as usize) as i32;
    let (band_width, band_height) = if width <= max_pixels {
        (width, (max_pixels / width.max(1)).max(1))
    } else {
        (max_pixels, 1)
    };
    (y..y + height)
        .step_by(band_height as usize)
        .flat_map(move |top| {
            let bottom = (top + band_height).min(y + height);
            (x..x + width)
                .step_by(band_width.max(1) as usize)
                .map(move |left| {
                    let right = (left + band_width).min(x + width);
                    Bounds {
                        origin: point(DevicePixels(left), DevicePixels(top)),
                        size: size(DevicePixels(right - left), DevicePixels(bottom - top)),
                    }
                })
        })
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, DevicePixels, point, size};

    use super::split_rect;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(x), DevicePixels(y)),
            size: size(DevicePixels(w), DevicePixels(h)),
        }
    }

    fn area(rects: &[Bounds<DevicePixels>]) -> i32 {
        rects
            .iter()
            .map(|rect| rect.size.width.0 * rect.size.height.0)
            .sum()
    }

    #[test]
    fn a_rect_under_the_limit_is_one_request() {
        let parts = split_rect(rect(3, 4, 10, 10), 100).collect::<Vec<_>>();
        assert_eq!(parts, vec![rect(3, 4, 10, 10)]);
    }

    #[test]
    fn a_tall_rect_splits_into_bands_of_rows() {
        let parts = split_rect(rect(5, 10, 100, 25), 1000).collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![
                rect(5, 10, 100, 10),
                rect(5, 20, 100, 10),
                rect(5, 30, 100, 5)
            ]
        );
    }

    #[test]
    fn a_row_longer_than_the_limit_splits_into_pieces() {
        let parts = split_rect(rect(0, 0, 250, 2), 100).collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![
                rect(0, 0, 100, 1),
                rect(100, 0, 100, 1),
                rect(200, 0, 50, 1),
                rect(0, 1, 100, 1),
                rect(100, 1, 100, 1),
                rect(200, 1, 50, 1),
            ]
        );
    }

    #[test]
    fn parts_stay_under_the_limit_and_cover_the_rect() {
        for (width, height, max) in [(1920, 1080, 65_535), (7, 13, 5), (4096, 3, 1000)] {
            let whole = rect(1, 2, width, height);
            let parts = split_rect(whole, max as usize).collect::<Vec<_>>();
            assert!(
                parts
                    .iter()
                    .all(|part| part.size.width.0 * part.size.height.0 <= max)
            );
            assert!(parts.iter().all(|part| whole.intersect(part) == *part));
            assert_eq!(area(&parts), width * height);
        }
    }

    #[test]
    fn an_empty_rect_is_no_request() {
        assert_eq!(split_rect(rect(0, 0, 0, 10), 100).count(), 0);
        assert_eq!(split_rect(rect(0, 0, 10, 0), 100).count(), 0);
    }
}
