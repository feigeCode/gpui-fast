# Scene damage and adaptive CPU rendering

Retained views, retained layout and scroll layers make the *logical* work of
a frame proportional to what changed. The *physical* work was not: every
frame, however small its change, acquired a swapchain image, recorded a
render pass over the whole window and presented it on the GPU. A blinking
caret, a hover, a ticking clock or one quote changing in a table woke the GPU
for a whole-window frame.

Two mechanisms change that:

1. **Scene damage** (`crates/gpui/src/fast/damage.rs`, every platform). Each
   finished scene carries the rectangles, in device pixels, where it can draw
   different pixels than the scene before it (`Scene::damage`).
2. **Adaptive CPU rendering** (`crates/gpui_wgpu/src/fast/adaptive/`, the
   wgpu renderer: Linux on Wayland and X11). A frame whose damage is small is
   drawn on the CPU into a frame kept in memory, only inside its damage, and
   shown without drawing the scene on the GPU. Frames that change much of the
   window, and bursts of frames whose CPU drawing costs too much, are drawn on
   the GPU as before.

The design is in
[`superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`](superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md).

## Scene damage

`Window::draw` diffs the scene it just finished against the scene drawn
before it (`fast::damage::finish_frame`) and fills `Scene::damage`:

- `frame`: the scene's number in its window, from 1;
- `since`: the number of the scene `rects` compare with, or 0 when every
  pixel may differ (the window's first scene, or a resize);
- `rects`: where this scene can draw different pixels than scene `since`;
- `changed_primitives`.

**It is exact.** A pixel's colour is a function of the primitives that cover
it, in drawing order: by `(order, kind)`, then by position within the kind.
The diff compares each kind's two vectors one draw order at a time. An order
whose primitives are byte-for-byte equal in both scenes damages nothing;
otherwise its primitives are matched by value, and the unmatched ones, and
the matched ones whose relative position changed (inside a text line's
layer, where primitives share an order and may overlap), are damaged. A pixel
outside the damage is drawn by the same primitives, in the same order, with
the same inputs, in both scenes. Also damaged:

- surfaces (video frames), always;
- scroll layer tiles that their layer lists as dirty in a new generation;
- overlapping paths whose batch split or joined: a batch's paths are
  rasterized together and each is composited from the result, so a
  primitive of another kind drawn between two paths changes their overlap
  without either path changing.

What a primitive can touch is its geometry clipped by its content mask,
rounded out to whole pixels: shadows reach three blur radii beyond their
bounds, transformed sprites cover their transformed bounds, paths a pixel
more. Rectangles within 8 px of each other merge; past 16, the two whose union
adds the fewest pixels merge, so changes spread over the window stay apart.
A damage over half the window is reported as the whole window.

Atlas content is outside the scene (an atlas tile freed and allocated again to
another image keeps its id), so the renderer adds the sprites over atlas
tiles written since its last frame to the damage.

`LayoutStats` (test-support) reports the damage: `damage_frames`,
`full_damage_frames`, `damaged_pixels`, `window_pixels`,
`small_damage_frames` (at most a sixteenth of the window),
`changed_primitives` and `damage_time`; `gpui_perf --headless` prints them.

## Adaptive CPU rendering

### A frame's path

`WgpuRenderer::draw` asks `fast::adaptive::draw` first. Per frame, the policy
(`fast::adaptive::policy`) takes:

```text
region  = damage
        ∪ stale (what GPU frames changed since the canvas was last drawn)
        ∪ sprites over atlas rectangles written since the last frame
          (the whole window when the canvas is missing, resized, or the damage
           is not relative to the scene drawn last)
burst   = this frame began within 50 ms of the last one ending
changed = area(damage ∪ atlas sprites)
```

and draws on the **GPU** when:

- the scene has surfaces, samples an atlas texture the CPU has no copy of,
  or has paths and the GPU rasterizes paths with other than 4 samples;
- the scene is not numbered (a composed window's replayed scene);
- it is the renderer's first frame;
- the frame is in a burst and changes more than a sixteenth of the window,
  or would redraw the canvas whole;
- the region is larger than 8 Mpx;
- the CPU frames of the burst have cost more than a quarter of its time,
  once it lasted 250 ms: the rest of the burst draws on the GPU.

Otherwise it draws on the **CPU**: `fast::cpu::raster` redraws the canvas
inside the region, and the frame is shown with the region as its damage.
After a GPU frame, its damage becomes stale for the canvas. After a second of
GPU frames only, the canvas is released; the next CPU frame draws it whole.

### The CPU rasterizer

`fast::cpu::raster` draws a finished scene the way the wgpu renderer records
it: the batches of `Scene::batches` in order, each primitive's fragments the
pixels whose centers its geometry covers (1/256-pixel snapping, top-left
rule), shaded by a port of its fragment shader in `f32`, blended into the
8-bit frame as the pipeline's blend state blends it. Every shader branch is
covered: quads (all backgrounds, rounded corners, borders, dashed borders),
shadows, underlines, monochrome sprites with contrast and gamma correction
and transformations, subpixel sprites (dual-source blending), polychrome
sprites, paths (4× multisampling into an intermediate, as the GPU), and scroll
layer tiles (their content drawn over the layer background).

Two GPU behaviours are reproduced to stay within a level of the GPU: blending
into the 8-bit target is done in fixed point after rounding the fragment to
8 bits, and texture filtering rounds coordinates and weights to 1/256. Large
regions are drawn in bands on up to 8 threads; the pixels do not depend on
how a region is split.

Sprites are sampled from the atlas's CPU copy (`fast::cpu::atlas`), which the
atlas keeps as it uploads (8-bit coverage, or BGRA before the swizzle).

### Showing a CPU frame

Two ways (`GPUI_CPU_PRESENT`):

- **native**: the platform's presenter (`WgpuRenderer::set_cpu_presenter`),
  without the GPU:
  - **Wayland** (`gpui_linux/src/fast/cpu_present/wayland.rs`): `wl_shm`
    buffers on a synchronized subsurface over the window. Not on the window
    surface itself: the Vulkan WSI gives it explicit synchronization
    (`wp_linux_drm_syncobj_surface_v1`, NVIDIA and recent Mesa), and a
    `wl_shm` buffer on such a surface is a protocol error. A CPU frame
    attaches its buffer to the subsurface and commits the window surface
    without a buffer, carrying the frame callback. The next GPU frame hides
    the subsurface in the same commit as its own buffer.
  - **X11** (`x11.rs`): the damage uploaded into the window with
    `PutImage`, through MIT-SHM for large rectangles.
- **blit**: the region is uploaded to a texture that a tiny pipeline copies to
  the swapchain image (`fast::adaptive::blit`). It still presents through the
  GPU, but records no scene: one upload of the damage and one full-screen
  triangle.

A presenter may refuse a frame (Wayland's before the GPU presented the
window's first frame, or while the compositor holds all its buffers): the
frame is then drawn on the GPU; after 64 refusals in a row the CPU path is
off for the window.

### Caveat: window opacity on Wayland

The native Wayland presenter shows CPU frames on a subsurface over the
window surface, which still holds the last GPU frame. Where the compositor
makes the window translucent (a window-opacity rule), the last GPU frame shows
through faintly where CPU frames changed since. The blit presenter has no
such effect.

## Controls

- `GPUI_CPU_RENDER=0`: never draw on the CPU (no presenter kept, no atlas
  copy). `GPUI_CPU_RENDER=always`: draw every frame the CPU can on the CPU,
  without the size, burst and cost limits, for measurements.
- `GPUI_CPU_PRESENT=native|blit`: how CPU frames are shown; unset,
  `fast::adaptive::default_present_mode` decides.
- `GPUI_RENDER_STATS=1`: every second, each window prints a line of
  `key=value` pairs to stderr: `cpu_frames`, `gpu_frames`, `cpu_px`, mean CPU
  and GPU frame times, per presentation mode, and `why_<reason>` counts of
  GPU frames.

## Verifying

- `cargo test -p gpui --features test-support --lib fast::tests::damage`:
  unit tests of every damage rule, and property tests over 1300 random scenes
  and edits checking that every pixel outside the damage is covered by the
  same primitives in the same order.
- `cargo test -p gpui_wgpu fast::cpu`: random and hand-built scenes of every
  primitive drawn by the CPU and by the GPU (`WgpuHeadlessRenderer`),
  compared pixel by pixel; region drawing against whole drawing; threaded
  against single-threaded.
- `cargo test -p gpui_wgpu fast::adaptive`: the policy with a fake clock,
  presenter call order, refusals, the blit path read back exactly.
- `cargo test -p gpui_linux cpu_present`: the presenters' bookkeeping.

## Measuring

`gpui_perf --idle` runs small-update scenarios in a real window;
`script/measure-adaptive` runs each with the GPU only and adaptively,
sampling process CPU, the GPU's utilization, power and performance state
(`nvidia-smi`) and the renderer's statistics. See `script/measure-adaptive
--help`.
