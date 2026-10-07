//! Drawing a frame on the CPU when it changes little, and on the GPU
//! otherwise.
//!
//! STUB: interface only; to be implemented.

use gpui::Scene;

use crate::WgpuRenderer;
use crate::fast::cpu::CpuPresenter;

/// The renderer's choice between drawing on the CPU and on the GPU, and what
/// the CPU path keeps between frames.
#[derive(Default)]
pub(crate) struct Adaptive {
    presenter: Option<Box<dyn CpuPresenter>>,
}

impl Adaptive {
    /// Notes that the GPU drew and is presenting `scene`.
    pub(crate) fn gpu_drew(_this: &mut Self, _scene: &Scene) {}
}

impl WgpuRenderer {
    /// Lets this renderer draw frames that change little on the CPU, and show
    /// them with `presenter`, without the GPU.
    pub fn set_cpu_presenter(&mut self, presenter: Box<dyn CpuPresenter>) {
        self.fast_adaptive.presenter = Some(presenter);
    }
}

/// Draws `scene` on the CPU and presents it, if it should be: returns
/// whether it was presented, or `None` for the GPU to draw it.
pub(crate) fn draw(_renderer: &mut WgpuRenderer, _scene: &Scene) -> Option<bool> {
    None
}
