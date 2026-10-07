//! Showing frames the renderer drew on the CPU without the GPU: through
//! `wl_shm` buffers on Wayland and `PutImage` on X11.
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.

#[cfg(feature = "wayland")]
pub(crate) mod wayland;
#[cfg(feature = "x11")]
pub(crate) mod x11;
