use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask, DevicePixels,
    MonochromeSprite, Point, ScaledPixels, Scene, Size, TransformationMatrix,
};

use crate::fast::adaptive::policy::{
    AtlasDamage, CANVAS_RELEASE_AFTER, CpuPlan, Decision, Frame, GpuReason, Policy, Target,
};
use crate::fast::adaptive::region::{Region, rect, whole};
use crate::fast::adaptive::{Adaptive, Mode, stats};
use crate::fast::cpu::atlas::AtlasMirror;
use crate::fast::cpu::raster::RasterParams;
use crate::fast::cpu::{CpuFrame, CpuPresenter};

const TARGET: Target = Target {
    width: 1000,
    height: 800,
    opaque: true,
};

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn frame<'a>(
    now: Instant,
    number: u64,
    since: u64,
    damage: &'a [Bounds<DevicePixels>],
) -> Frame<'a> {
    Frame {
        now,
        target: TARGET,
        number,
        since,
        damage,
        atlas: AtlasDamage::Rects(&[]),
        needs_gpu: None,
        always: false,
    }
}

fn region(rects: &[Bounds<DevicePixels>]) -> Vec<Bounds<DevicePixels>> {
    let mut region = Region::default();
    region.add_all(rects, TARGET.bounds());
    sorted(region.rects())
}

fn sorted(rects: &[Bounds<DevicePixels>]) -> Vec<Bounds<DevicePixels>> {
    let mut rects = rects.to_vec();
    rects.sort_by_key(|rect| (rect.origin.y.0, rect.origin.x.0));
    rects
}

fn cpu(decision: Decision) -> CpuPlan {
    match decision {
        Decision::Cpu(plan) => plan,
        Decision::Gpu(reason) => panic!("drawn on the GPU: {reason:?}"),
    }
}

fn gpu(decision: Decision) -> GpuReason {
    match decision {
        Decision::Gpu(reason) => reason,
        Decision::Cpu(plan) => panic!("drawn on the CPU: {plan:?}"),
    }
}

/// A policy whose canvas shows scene 1, drawn whole at `t0`, done 5 ms later.
fn with_canvas(t0: Instant) -> Policy {
    let mut policy = Policy::default();
    let plan = cpu(policy.decide(&frame(t0, 1, 0, &[])));
    assert!(plan.whole);
    assert_eq!(plan.region.rects(), &[TARGET.bounds()]);
    policy.cpu_drew(1, TARGET, t0, t0 + ms(5), ms(5), false);
    policy
}

#[test]
fn tiny_damage_when_idle_draws_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(10, 10, 20, 20)];
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 2, 1, &damage)));
    assert!(!plan.whole);
    assert_eq!(plan.region.rects(), &damage);
    assert_eq!(plan.changed, 400);
}

#[test]
fn large_damage_in_burst_draws_on_gpu_and_small_damage_after_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    // 300 x 300 > 1000 x 800 / 16.
    let large = [rect(0, 0, 300, 300)];
    let now = t0 + ms(5 + 10);
    assert_eq!(
        gpu(policy.decide(&frame(now, 2, 1, &large))),
        GpuReason::LargeChange
    );
    policy.gpu_drew(now + ms(2), 2, 1, &large, AtlasDamage::Rects(&[]));
    // Out of a burst, the same change draws on the CPU.
    let mut idle = with_canvas(t0);
    assert!(!cpu(idle.decide(&frame(t0 + ms(500), 2, 1, &large))).whole);

    // Still in the burst, a small change draws on the CPU with what the GPU
    // drew meanwhile.
    let small = [rect(500, 500, 10, 10)];
    let plan = cpu(policy.decide(&frame(now + ms(10), 3, 2, &small)));
    assert_eq!(sorted(plan.region.rects()), region(&[large[0], small[0]]));
    assert_eq!(plan.changed, 100);
    assert!(policy.take_gpu_presented());
}

#[test]
fn costly_cpu_burst_moves_to_gpu_until_a_pause() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 50, 50)];
    // Frames every 16 ms costing 8 ms of CPU each: half the burst's time.
    let mut now = t0 + ms(20);
    let mut number = 2;
    let mut switched_at = None;
    while now < t0 + ms(1000) {
        match policy.decide(&frame(now, number, number - 1, &damage)) {
            Decision::Cpu(_) => {
                assert!(switched_at.is_none(), "back on the CPU within the burst");
                policy.cpu_drew(number, TARGET, now, now + ms(8), ms(8), false);
            }
            Decision::Gpu(reason) => {
                assert_eq!(reason, GpuReason::CpuHeavy);
                switched_at.get_or_insert(now);
                policy.gpu_drew(
                    now + ms(2),
                    number,
                    number - 1,
                    &damage,
                    AtlasDamage::Rects(&[]),
                );
            }
        }
        number += 1;
        now += ms(16);
    }
    let switched_at = switched_at.expect("the burst moved to the GPU");
    // The load counts from the canvas's frame, at t0, in the same burst.
    let lasted = switched_at - t0;
    assert!(
        lasted >= ms(250) && lasted < ms(300),
        "switched after {lasted:?}"
    );

    // A pause of 50 ms ends the burst: the CPU draws again.
    let last_end = now - ms(16) + ms(2);
    let plan = cpu(policy.decide(&frame(last_end + ms(60), number, number - 1, &damage)));
    assert!(!plan.whole);
}

#[test]
fn cheap_cpu_burst_stays_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 50, 50)];
    let mut now = t0 + ms(20);
    for number in 2..100 {
        cpu(policy.decide(&frame(now, number, number - 1, &damage)));
        policy.cpu_drew(number, TARGET, now, now + ms(1), ms(2), false);
        now += ms(16);
    }
}

#[test]
fn gpu_frames_accumulate_stale_rects() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let a = rect(0, 0, 10, 10);
    let b = rect(400, 400, 10, 10);
    let c = rect(800, 100, 10, 10);
    let atlas = [rect(100, 700, 5, 5)];
    policy.gpu_drew(t0 + ms(100), 2, 1, &[a], AtlasDamage::Rects(&[]));
    policy.gpu_drew(t0 + ms(200), 3, 2, &[b], AtlasDamage::Rects(&atlas));
    assert_eq!(sorted(policy.stale().rects()), region(&[a, b, atlas[0]]));
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 4, 3, &[c])));
    assert_eq!(sorted(plan.region.rects()), region(&[c, a, b, atlas[0]]));
    assert_eq!(plan.changed, 100);
    policy.cpu_drew(4, TARGET, t0 + ms(500), t0 + ms(501), ms(1), false);
    assert!(policy.stale().is_empty());

    // A GPU frame not comparable with the canvas's scene invalidates it.
    policy.gpu_drew(t0 + ms(600), 6, 5, &[a], AtlasDamage::Rects(&[]));
    assert!(!policy.has_canvas());
    assert!(cpu(policy.decide(&frame(t0 + ms(900), 7, 6, &[a]))).whole);
}

#[test]
fn canvas_released_after_a_second_of_gpu_frames() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    let start = t0 + ms(100);
    let mut released = false;
    for (ix, number) in (2..).take(80).enumerate() {
        let now = start + ms(16 * ix as u64);
        let release = policy.gpu_drew(now, number, number - 1, &damage, AtlasDamage::Rects(&[]));
        if release {
            assert!(!released, "released once");
            assert!(now - start >= CANVAS_RELEASE_AFTER);
            released = true;
        } else {
            assert!(released || now - start < CANVAS_RELEASE_AFTER);
        }
    }
    assert!(released);
    assert!(!policy.has_canvas());
    // After a pause, the next CPU frame draws the canvas whole.
    let now = start + ms(16 * 80 + 100);
    assert!(cpu(policy.decide(&frame(now, 82, 81, &damage))).whole);
    // In a burst it would have drawn on the GPU.
    let mut policy = with_canvas(t0);
    policy.release_canvas();
    assert_eq!(
        gpu(policy.decide(&frame(t0 + ms(10), 2, 1, &damage))),
        GpuReason::WholeInBurst
    );
}

#[test]
fn since_mismatch_draws_whole() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    // Scene 2 was never presented: scene 3's damage is relative to it.
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 3, 2, &damage)));
    assert!(plan.whole);
    assert_eq!(plan.region.rects(), &[TARGET.bounds()]);
    // A resize draws whole too.
    let mut policy = with_canvas(t0);
    let mut resized = frame(t0 + ms(500), 2, 1, &damage);
    resized.target.width = 900;
    assert!(cpu(policy.decide(&resized)).whole);
    // As does damage that is not relative to anything.
    let mut policy = with_canvas(t0);
    assert!(cpu(policy.decide(&frame(t0 + ms(500), 2, 0, &damage))).whole);
    // The same scene presented again draws nothing new.
    let mut policy = with_canvas(t0);
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 1, 0, &damage)));
    assert!(!plan.whole);
    assert!(plan.region.is_empty());
}

#[test]
fn atlas_writes_add_sprite_extents() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    let sprites = [rect(300, 300, 20, 20)];
    let mut input = frame(t0 + ms(500), 2, 1, &damage);
    input.atlas = AtlasDamage::Rects(&sprites);
    let plan = cpu(policy.decide(&input));
    assert_eq!(
        sorted(plan.region.rects()),
        region(&[damage[0], sprites[0]])
    );
    assert_eq!(plan.changed, 500);
    input.atlas = AtlasDamage::Everything;
    assert!(cpu(policy.decide(&input)).whole);
}

#[test]
fn limits_and_always() {
    let t0 = Instant::now();
    let big = Target {
        width: 4000,
        height: 3000,
        opaque: false,
    };
    let mut policy = Policy::default();
    let mut input = frame(t0, 1, 0, &[]);
    input.target = big;
    assert_eq!(gpu(policy.decide(&input)), GpuReason::TooLarge);
    input.always = true;
    assert!(cpu(policy.decide(&input)).whole);
    input.needs_gpu = Some(GpuReason::Surfaces);
    assert_eq!(gpu(policy.decide(&input)), GpuReason::Surfaces);
    input.needs_gpu = None;
    input.number = 0;
    assert_eq!(gpu(policy.decide(&input)), GpuReason::Composition);
}

#[test]
fn half_window_region_draws_whole() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 1000, 500)];
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 2, 1, &damage)));
    assert!(plan.whole);
}

#[derive(Default)]
struct Log {
    frames: Vec<(u32, u32, Vec<Bounds<DevicePixels>>, bool)>,
    gpu_presented: usize,
    released: usize,
    fail: bool,
}

struct FakePresenter(Rc<RefCell<Log>>);

impl CpuPresenter for FakePresenter {
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let mut log = self.0.borrow_mut();
        anyhow::ensure!(!log.fail, "presenter failed");
        assert_eq!(frame.pixels.len(), (frame.width * frame.height) as usize);
        log.frames.push((
            frame.width,
            frame.height,
            frame.damage.to_vec(),
            frame.opaque,
        ));
        Ok(())
    }

    fn gpu_presented(&mut self) {
        self.0.borrow_mut().gpu_presented += 1;
    }

    fn release(&mut self) {
        self.0.borrow_mut().released += 1;
    }
}

const PARAMS: RasterParams = RasterParams {
    gamma_ratios: [0.; 4],
    grayscale_enhanced_contrast: 1.,
    subpixel_enhanced_contrast: 1.,
    is_bgr: false,
    premultiplied_alpha: false,
    dual_source_blending: true,
};

fn scene(number: u64, since: u64, damage: &[Bounds<DevicePixels>]) -> Scene {
    let mut scene = Scene::default();
    scene.damage.frame = number;
    scene.damage.since = since;
    scene.damage.rects = damage.to_vec();
    scene
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

fn installed(mirror: &mut AtlasMirror) -> (Adaptive, Rc<RefCell<Log>>) {
    let log = Rc::new(RefCell::new(Log::default()));
    let mut adaptive = Adaptive {
        mode: Some(Mode::Auto),
        ..Adaptive::default()
    };
    Adaptive::install(&mut adaptive, Box::new(FakePresenter(log.clone())), mirror);
    (adaptive, log)
}

#[test]
fn presenter_receives_cpu_frames_with_their_region() {
    let mut mirror = AtlasMirror::default();
    let (mut adaptive, log) = installed(&mut mirror);
    assert!(AtlasMirror::is_enabled(&mirror));
    let totals = stats::totals();
    let t0 = Instant::now();

    // The first frame draws whole.
    let first = scene(1, 0, &[]);
    assert_eq!(
        Adaptive::frame(&mut adaptive, &first, &mut mirror, TARGET, &PARAMS, t0),
        Some(true)
    );
    assert_eq!(
        log.borrow().frames,
        vec![(1000, 800, vec![whole(1000, 800)], true)]
    );

    // A glyph rasterized into a tile the scene's sprite samples is redrawn
    // with the damage.
    let texture = AtlasTextureId {
        index: 0,
        kind: AtlasTextureKind::Monochrome,
    };
    AtlasMirror::upload_raw(
        &mut mirror,
        texture,
        1024,
        1024,
        1,
        rect(0, 0, 4, 4),
        &[255; 16],
    );
    let damage = [rect(0, 0, 10, 10)];
    let mut second = scene(2, 1, &damage);
    second.monochrome_sprites.push(MonochromeSprite {
        order: 0,
        pad: 0,
        bounds: scaled(100., 100., 4., 4.),
        content_mask: ContentMask {
            bounds: scaled(0., 0., 1000., 800.),
        },
        color: Default::default(),
        tile: AtlasTile {
            texture_id: texture,
            tile_id: gpui::TileId(1),
            padding: 0,
            bounds: rect(0, 0, 4, 4),
        },
        transformation: TransformationMatrix::unit(),
    });
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &second,
            &mut mirror,
            TARGET,
            &PARAMS,
            t0 + ms(500)
        ),
        Some(true)
    );
    assert_eq!(
        sorted(&log.borrow().frames[1].2),
        vec![rect(0, 0, 10, 10), rect(99, 99, 6, 6)]
    );

    // A large change in a burst goes to the GPU; its damage is redrawn by
    // the next CPU frame, which tells the presenter the GPU presented.
    let large = [rect(0, 0, 400, 400)];
    let third = scene(3, 2, &large);
    let now = t0 + ms(510);
    assert_eq!(
        Adaptive::frame(&mut adaptive, &third, &mut mirror, TARGET, &PARAMS, now),
        None
    );
    Adaptive::gpu_drew_at(&mut adaptive, &third, now + ms(3));
    assert_eq!(log.borrow().gpu_presented, 0);
    let small = [rect(600, 600, 4, 4)];
    let fourth = scene(4, 3, &small);
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &fourth,
            &mut mirror,
            TARGET,
            &PARAMS,
            now + ms(10)
        ),
        Some(true)
    );
    assert_eq!(log.borrow().gpu_presented, 1);
    assert_eq!(sorted(&log.borrow().frames[2].2), vec![large[0], small[0]]);
    let (cpu_frames, gpu_frames, reasons) = adaptive.stats.counts();
    assert_eq!((cpu_frames, gpu_frames), (3, 1));
    assert_eq!(reasons[GpuReason::LargeChange.index()], 1);
    let after = stats::totals();
    assert!(after.cpu_frames >= totals.cpu_frames + 3);
    assert!(
        after.gpu_reasons[GpuReason::LargeChange.index()]
            > totals.gpu_reasons[GpuReason::LargeChange.index()]
    );

    // A composed window's replayed scene is not numbered: GPU, and the next
    // numbered scene draws whole.
    let replayed = scene(0, 0, &[]);
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &replayed,
            &mut mirror,
            TARGET,
            &PARAMS,
            now + ms(500)
        ),
        None
    );
    Adaptive::gpu_drew_at(&mut adaptive, &replayed, now + ms(501));
    let sixth = scene(6, 5, &small);
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &sixth,
            &mut mirror,
            TARGET,
            &PARAMS,
            now + ms(1000)
        ),
        Some(true)
    );
    assert_eq!(log.borrow().frames[3].2, vec![whole(1000, 800)]);
}

#[test]
fn present_failure_falls_back_to_gpu_for_good() {
    let mut mirror = AtlasMirror::default();
    let (mut adaptive, log) = installed(&mut mirror);
    let t0 = Instant::now();
    log.borrow_mut().fail = true;
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &scene(1, 0, &[]),
            &mut mirror,
            TARGET,
            &PARAMS,
            t0
        ),
        None
    );
    assert!(!AtlasMirror::is_enabled(&mirror));
    log.borrow_mut().fail = false;
    assert_eq!(
        Adaptive::frame(
            &mut adaptive,
            &scene(2, 1, &[]),
            &mut mirror,
            TARGET,
            &PARAMS,
            t0 + ms(500)
        ),
        None
    );
    Adaptive::gpu_drew(&mut adaptive, &scene(2, 1, &[]));
    assert!(log.borrow().frames.is_empty());
}

#[test]
fn canvas_and_presenter_buffers_released_after_gpu_second() {
    let mut mirror = AtlasMirror::default();
    let (mut adaptive, log) = installed(&mut mirror);
    let t0 = Instant::now();
    Adaptive::frame(
        &mut adaptive,
        &scene(1, 0, &[]),
        &mut mirror,
        TARGET,
        &PARAMS,
        t0,
    )
    .unwrap();
    assert!(adaptive.canvas.is_some());
    // Animating most of the window, on the GPU.
    let large = [rect(0, 0, 500, 500)];
    for ix in 0..70u64 {
        let now = t0 + ms(10 + 16 * ix);
        let number = ix + 2;
        let frame = scene(number, number - 1, &large);
        assert_eq!(
            Adaptive::frame(&mut adaptive, &frame, &mut mirror, TARGET, &PARAMS, now),
            None
        );
        Adaptive::gpu_drew_at(&mut adaptive, &frame, now + ms(2));
    }
    assert!(adaptive.canvas.is_none());
    assert_eq!(log.borrow().released, 1);
}

#[test]
fn missing_atlas_textures_need_gpu() {
    let mut mirror = AtlasMirror::default();
    let texture = AtlasTextureId {
        index: 2,
        kind: AtlasTextureKind::Polychrome,
    };
    // Uploaded before a presenter was installed.
    AtlasMirror::upload_raw(
        &mut mirror,
        texture,
        1024,
        1024,
        4,
        rect(0, 0, 1, 1),
        &[0; 4],
    );
    let (mut adaptive, _log) = installed(&mut mirror);
    let mut first = scene(1, 0, &[]);
    first.polychrome_sprites.push(gpui::PolychromeSprite {
        order: 0,
        pad: 0,
        grayscale: Default::default(),
        opacity: 1.,
        bounds: scaled(0., 0., 1., 1.),
        content_mask: ContentMask {
            bounds: scaled(0., 0., 10., 10.),
        },
        corner_radii: Default::default(),
        tile: AtlasTile {
            texture_id: texture,
            tile_id: gpui::TileId(1),
            padding: 0,
            bounds: rect(0, 0, 1, 1),
        },
    });
    let t0 = Instant::now();
    assert_eq!(
        Adaptive::frame(&mut adaptive, &first, &mut mirror, TARGET, &PARAMS, t0),
        None
    );
    assert_eq!(
        adaptive.stats.counts().2[GpuReason::MissingAtlas.index()],
        1
    );
}
