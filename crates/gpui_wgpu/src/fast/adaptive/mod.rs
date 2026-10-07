//! Drawing a frame on the CPU when it changes little, and on the GPU
//! otherwise.
//!
//! `WgpuRenderer::draw` first calls [`draw`]. A window draws on the CPU only
//! once its platform installed a presenter ([`WgpuRenderer::set_cpu_presenter`]);
//! then, per frame, [`policy::Policy`] chooses the path from the scene's
//! damage (`Scene::damage`), the atlas tiles written since the last frame,
//! what the CPU's frame in memory (the canvas) still shows, and how much the
//! CPU frames of the current burst cost. A CPU frame redraws the canvas only
//! inside its region (damage, stale pixels the GPU drew since, sprites over
//! written atlas tiles) with `cpu::raster` and hands it to the presenter
//! with that region as its damage. A GPU frame is drawn as before; its damage
//! becomes stale for the canvas ([`Adaptive::gpu_drew`]).
//!
//! Environment:
//! - `GPUI_CPU_RENDER=0` never draws on the CPU (the presenter is not kept);
//!   `GPUI_CPU_RENDER=always` draws every frame it can on the CPU, without
//!   the size, burst and cost limits, for measurements.
//! - `GPUI_RENDER_STATS=1` logs (and prints to stderr) each window's frames
//!   every second, checked when a frame is drawn: frames per path, pixels the
//!   CPU drew, CPU frame times, the CPU-side time of GPU frames
//!   (`WgpuRenderer::draw` from here to `frame.present()`), and why frames
//!   went to the GPU.
//!
//! Composition: a window that composes its content draws each surface from a
//! scene replayed out of the window's scene (`draw_composed` on Linux). Such
//! scenes are not numbered (`SceneDamage::frame` is 0) and always draw on the
//! GPU, and the next numbered scene finds its `since` different from the
//! last scene drawn, so the canvas is drawn whole.
//!
//! CPU time: the raster draws large regions on several threads. Their CPU
//! time is not measured; it is counted as the wall time of the drawing times
//! the threads used, an upper bound, plus the wall time of presenting.
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.

pub(crate) mod policy;
pub(crate) mod region;
pub(crate) mod stats;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;
use std::time::Instant;

use collections::FxHashMap;
use gpui::{AtlasTextureId, Bounds, DevicePixels, Scene};

use crate::WgpuRenderer;
use crate::fast::adaptive::policy::{AtlasDamage, Decision, Frame, GpuReason, Policy, Target};
use crate::fast::adaptive::region::Region;
use crate::fast::adaptive::stats::WindowStats;
use crate::fast::cpu::atlas::AtlasMirror;
use crate::fast::cpu::raster::{self, Canvas, RasterParams};
use crate::fast::cpu::{CpuFrame, CpuPresenter};
use crate::wgpu_renderer::RendererState;

/// Regions smaller than this, in pixels, are drawn on one thread.
pub(crate) const SINGLE_THREAD_PIXELS: i64 = 64 * 1024;

/// The most threads a region is drawn on.
pub(crate) const MAX_THREADS: usize = 8;

/// `GPUI_CPU_RENDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Auto,
    Always,
}

pub(crate) fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GPUI_CPU_RENDER").as_deref() {
        Ok("0") => Mode::Off,
        Ok("always") => Mode::Always,
        _ => Mode::Auto,
    })
}

fn threads_for(pixels: i64) -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    if pixels < SINGLE_THREAD_PIXELS {
        return 1;
    }
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |threads| threads.get())
            .min(MAX_THREADS)
    })
}

/// The renderer's choice between drawing on the CPU and on the GPU, and what
/// the CPU path keeps between frames.
#[derive(Default)]
pub(crate) struct Adaptive {
    presenter: Option<Box<dyn CpuPresenter>>,
    canvas: Option<Canvas>,
    policy: Policy,
    stats: WindowStats,
    /// The frame last handed to the GPU, until it is presented.
    pending: Option<PendingGpu>,
    /// The extents of the sprites of the frame being drawn over atlas
    /// rectangles written since the frame before.
    atlas_region: Region,
    writes: Vec<(AtlasTextureId, Bounds<DevicePixels>)>,
    writes_by_texture: FxHashMap<AtlasTextureId, Vec<Bounds<DevicePixels>>>,
    /// Overrides [`mode`], for tests.
    #[cfg(test)]
    mode: Option<Mode>,
}

struct PendingGpu {
    number: u64,
    atlas_everything: bool,
    start: Instant,
}

impl Adaptive {
    fn mode(this: &Self) -> Mode {
        #[cfg(test)]
        if let Some(mode) = this.mode {
            return mode;
        }
        let _ = this;
        mode()
    }

    /// Starts drawing frames on the CPU when they should be, with
    /// `presenter`, sampling sprites from `mirror`.
    fn install(this: &mut Self, presenter: Box<dyn CpuPresenter>, mirror: &mut AtlasMirror) {
        AtlasMirror::enable(mirror);
        *this = Adaptive {
            presenter: Some(presenter),
            #[cfg(test)]
            mode: this.mode,
            ..Adaptive::default()
        };
    }

    /// Draws `scene` into `target` on the CPU and presents it, if it should
    /// be, and returns whether it was presented, or `None` for the GPU to
    /// draw it. `now` is when the frame began.
    fn frame(
        this: &mut Self,
        scene: &Scene,
        mirror: &mut AtlasMirror,
        target: Target,
        params: &RasterParams,
        now: Instant,
    ) -> Option<bool> {
        let real_start = Instant::now();
        let always = Self::mode(this) == Mode::Always;
        let presenter = this.presenter.as_mut()?;
        let damage = &scene.damage;

        let everything = AtlasMirror::take_writes(mirror, &mut this.writes);
        this.atlas_region.clear();
        if !everything && this.policy.has_canvas() {
            region::add_written_sprites(
                &mut this.atlas_region,
                scene,
                &this.writes,
                target.bounds(),
                &mut this.writes_by_texture,
            );
        }
        let atlas = if everything {
            AtlasDamage::Everything
        } else {
            AtlasDamage::Rects(this.atlas_region.rects())
        };
        let needs_gpu = (AtlasMirror::has_missing(mirror)
            && region::samples_any(scene, |id| AtlasMirror::is_missing(mirror, id)))
        .then_some(GpuReason::MissingAtlas);
        let decision = this.policy.decide(&Frame {
            now,
            target,
            number: damage.frame,
            since: damage.since,
            damage: &damage.rects,
            atlas,
            needs_gpu,
            always,
        });
        let plan = match decision {
            Decision::Cpu(plan) => match raster::can_draw(scene, plan.region.rects()) {
                Ok(()) => plan,
                Err(raster::NeedsGpu::Surfaces) => {
                    return Self::to_gpu(this, scene, everything, GpuReason::Surfaces, now);
                }
            },
            Decision::Gpu(reason) => return Self::to_gpu(this, scene, everything, reason, now),
        };

        let canvas = match &mut this.canvas {
            Some(canvas) if canvas.width() == target.width && canvas.height() == target.height => {
                canvas
            }
            slot => {
                debug_assert!(plan.whole, "a new canvas is drawn whole");
                slot.insert(Canvas::new(target.width, target.height))
            }
        };
        let pixels = plan.region.area();
        let threads = threads_for(pixels);
        let drawing = Instant::now();
        raster::draw(
            canvas,
            scene,
            plan.region.rects(),
            &*mirror,
            params,
            threads,
        );
        let drawn = drawing.elapsed();

        if this.policy.take_gpu_presented() {
            presenter.gpu_presented();
        }
        let presented = presenter.present(CpuFrame {
            pixels: canvas.pixels(),
            width: canvas.width(),
            height: canvas.height(),
            damage: plan.region.rects(),
            opaque: target.opaque,
        });
        let took = real_start.elapsed();
        if let Err(error) = presented {
            log::error!(
                "presenting a frame drawn on the CPU, drawing on the GPU from now: {error:#}"
            );
            this.presenter = None;
            this.canvas = None;
            this.policy = Policy::default();
            this.pending = None;
            AtlasMirror::disable(mirror);
            this.stats.gpu_chosen(GpuReason::PresentFailed);
            return None;
        }
        // Wall time, plus the other threads' share of the drawing.
        let cpu = took + drawn * (threads as u32 - 1);
        let end = now + took;
        this.policy
            .cpu_drew(damage.frame, target, now, end, cpu, always);
        this.pending = None;
        this.stats.cpu_frame(pixels, took);
        this.stats.tick(end);
        Some(true)
    }

    fn to_gpu(
        this: &mut Self,
        scene: &Scene,
        atlas_everything: bool,
        reason: GpuReason,
        now: Instant,
    ) -> Option<bool> {
        this.stats.gpu_chosen(reason);
        this.stats.tick(now);
        this.pending = Some(PendingGpu {
            number: scene.damage.frame,
            atlas_everything,
            start: Instant::now(),
        });
        None
    }

    /// Notes that the GPU drew and is presenting `scene`.
    pub(crate) fn gpu_drew(this: &mut Self, scene: &Scene) {
        if this.presenter.is_none() {
            return;
        }
        Self::gpu_drew_at(this, scene, Instant::now());
    }

    fn gpu_drew_at(this: &mut Self, scene: &Scene, now: Instant) {
        let pending = this
            .pending
            .take()
            .filter(|pending| pending.number == scene.damage.frame);
        let atlas = match &pending {
            Some(pending) if !pending.atlas_everything => {
                AtlasDamage::Rects(this.atlas_region.rects())
            }
            _ => AtlasDamage::Everything,
        };
        let damage = &scene.damage;
        if this
            .policy
            .gpu_drew(now, damage.frame, damage.since, &damage.rects, atlas)
        {
            this.canvas = None;
            if let Some(presenter) = &mut this.presenter {
                presenter.release();
            }
        }
        this.stats
            .gpu_frame(pending.map(|pending| pending.start.elapsed()));
        this.stats.tick(now);
    }
}

impl WgpuRenderer {
    /// Lets this renderer draw frames that change little on the CPU, and show
    /// them with `presenter`, without the GPU. Does nothing with
    /// `GPUI_CPU_RENDER=0`, or on the web.
    pub fn set_cpu_presenter(&mut self, presenter: Box<dyn CpuPresenter>) {
        if mode() == Mode::Off || cfg!(target_family = "wasm") {
            return;
        }
        Adaptive::install(
            &mut self.fast_adaptive,
            presenter,
            &mut self.atlas.cpu_mirror(),
        );
    }
}

/// Draws `scene` on the CPU and presents it, if it should be: returns
/// whether it was presented, or `None` for the GPU to draw it.
pub(crate) fn draw(renderer: &mut WgpuRenderer, scene: &Scene) -> Option<bool> {
    if renderer.fast_adaptive.presenter.is_none() {
        return None;
    }
    let WgpuRenderer {
        state,
        surface_config,
        atlas,
        fast_adaptive,
        ..
    } = renderer;
    let RendererState::Ready { core, .. } = state else {
        fast_adaptive.pending = None;
        return None;
    };
    let target = Target {
        width: surface_config.width,
        height: surface_config.height,
        opaque: surface_config.alpha_mode == wgpu::CompositeAlphaMode::Opaque,
    };
    // As `WgpuRenderer::draw` passes them to the GPU.
    let params = RasterParams {
        gamma_ratios: core.rendering_params.gamma_ratios,
        grayscale_enhanced_contrast: core.rendering_params.grayscale_enhanced_contrast,
        subpixel_enhanced_contrast: core.rendering_params.subpixel_enhanced_contrast,
        is_bgr: core.is_bgr,
        premultiplied_alpha: surface_config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied,
        dual_source_blending: core.dual_source_blending,
    };
    let mut mirror = atlas.cpu_mirror();
    Adaptive::frame(
        fast_adaptive,
        scene,
        &mut mirror,
        target,
        &params,
        Instant::now(),
    )
}
