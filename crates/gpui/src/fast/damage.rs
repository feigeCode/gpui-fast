//! Scene damage: where a finished scene can draw different pixels than the
//! scene the window drew before it, for renderers that redraw only that.
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.

use crate::{Bounds, DevicePixels, Window};

/// Where a finished scene can draw different pixels than an earlier scene of
/// its window, in device pixels.
#[derive(Clone, Debug, Default)]
pub struct SceneDamage {
    /// This scene's number among its window's scenes, counting from 1.
    pub frame: u64,
    /// The number of the scene `rects` compare this one with. Zero when
    /// there is none: every pixel may differ.
    pub since: u64,
    /// Where this scene can draw different pixels than scene `since`. Empty
    /// when the two draw the same pixels.
    pub rects: Vec<Bounds<DevicePixels>>,
    /// Primitives of either scene not drawn the same in the other.
    pub changed_primitives: usize,
}

impl SceneDamage {
    /// Whether every pixel may differ, as nothing is known to compare with.
    pub fn is_full(&self) -> bool {
        self.since == 0
    }

    /// The pixels `rects` cover, counting overlaps once per rectangle.
    pub fn area(&self) -> i64 {
        self.rects
            .iter()
            .map(|rect| rect.size.width.0 as i64 * rect.size.height.0 as i64)
            .sum()
    }
}

/// Fills the damage of the frame just finished (`window.next_frame`) against
/// the frame drawn before it (`window.rendered_frame`).
pub(crate) fn finish_frame(window: &mut Window) {
    let frame = window.rendered_frame.scene.damage.frame + 1;
    let damage = &mut window.next_frame.scene.damage;
    damage.frame = frame;
    damage.since = 0;
    damage.rects.clear();
    damage.changed_primitives = 0;
}
