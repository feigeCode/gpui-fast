//! Tests of elements drawn again from what they drew on the last frame.

use crate::{
    Context, IntoElement, Render, SharedString, TestAppContext, Window, WindowHandle, div, hsla,
    prelude::*, px,
};

struct Rows {
    texts: Vec<SharedString>,
}

impl Render for Rows {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().children(
            self.texts
                .iter()
                .cloned()
                .map(|text| div().h(px(20.)).bg(hsla(0.5, 0.5, 0.5, 1.)).child(text)),
        )
    }
}

#[gpui::test]
fn unchanged_elements_of_a_rendered_view_are_reused(cx: &mut TestAppContext) {
    let window: WindowHandle<Rows> = cx.add_window(|_, _| Rows {
        texts: vec!["a".into(), "b".into(), "c".into()],
    });
    // Rendered again as it was, until its elements are recorded: an element
    // nested in no other is recorded once it stands still.
    for _ in 0..3 {
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
    }
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.reset_layout_stats();
    })
    .unwrap();
    window
        .update(cx, |rows, _, cx| {
            rows.texts[1] = "B".into();
            cx.notify();
        })
        .unwrap();
    let stats = cx
        .update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.layout_stats()
        })
        .unwrap();
    // The two rows that did not change, a div and its text each, and the
    // changed row's div, whose text changed.
    assert_eq!(stats.elements_reused, 4, "{stats:?}");
}

struct Narrowing {
    width: f32,
}

impl Render for Narrowing {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_start()
            .w(px(self.width))
            .child(div().child("x"))
    }
}

#[gpui::test]
fn text_measured_again_is_not_drawn_again(cx: &mut TestAppContext) {
    let window: WindowHandle<Narrowing> = cx.add_window(|_, _| Narrowing { width: 200. });
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.reset_layout_stats();
    })
    .unwrap();
    window
        .update(cx, |narrowing, _, cx| {
            narrowing.width = 150.;
            cx.notify();
        })
        .unwrap();
    let stats = cx
        .update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.layout_stats()
        })
        .unwrap();
    // The text keeps its size in a narrower column, but was measured again
    // under the narrower width, and could have broken into other lines.
    assert!(stats.measure_calls > 0, "{stats:?}");
    assert_eq!(stats.elements_reused, 0, "{stats:?}");
}

struct Outer {
    tick: usize,
    middle: crate::Entity<Middle>,
}

struct Middle {
    tick: usize,
    inner: crate::Entity<Inner>,
}

struct Inner {
    inner: crate::Entity<Leaf>,
}

struct Leaf {
    tick: usize,
}

impl Render for Outer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(10.))
                    .child(SharedString::from(self.tick.to_string())),
            )
            .child(self.middle.clone())
    }
}

impl Render for Middle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(10.))
                    .child(SharedString::from(self.tick.to_string())),
            )
            .child(self.inner.clone())
    }
}

impl Render for Inner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(div().h(px(10.)).child("inner"))
            .child(self.inner.clone())
    }
}

impl Render for Leaf {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(SharedString::from(self.tick.to_string()))
    }
}

/// A view drawn from last frame carries the element records drawn inside it,
/// and those of the views nested in it; one of them drawn around a view
/// nested in it, which carries none, must not throw that off.
#[gpui::test]
fn a_view_drawn_again_carries_what_a_spliced_view_in_it_did_not(cx: &mut TestAppContext) {
    let window: WindowHandle<Outer> = cx.add_window(|_, cx| {
        let leaf = cx.new(|_| Leaf { tick: 0 });
        let inner = cx.new(|_| Inner { inner: leaf });
        Outer {
            tick: 0,
            middle: cx.new(|_| Middle { tick: 0, inner }),
        }
    });
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap()
    };
    draw(cx);
    let (middle, leaf) = window
        .read_with(cx, |outer, cx| {
            let middle = outer.middle.clone();
            let leaf = middle.read(cx).inner.read(cx).inner.clone();
            (middle, leaf)
        })
        .unwrap();
    for _ in 0..3 {
        // The middle view is built, and the inner one drawn around the leaf.
        cx.update(|cx| {
            middle.update(cx, |middle, cx| {
                middle.tick += 1;
                cx.notify();
            });
            leaf.update(cx, |leaf, cx| {
                leaf.tick += 1;
                cx.notify();
            });
        });
        draw(cx);
        // The outer view is built, and the middle one drawn from last frame.
        window
            .update(cx, |outer, _, cx| {
                outer.tick += 1;
                cx.notify();
            })
            .unwrap();
        draw(cx);
    }
}

/// Draws `window` again after notifying its view, returning the frame's
/// statistics.
fn draw_notified<V: Render>(
    window: WindowHandle<V>,
    cx: &mut TestAppContext,
) -> crate::LayoutStats {
    cx.update_window(window.into(), |_, window, _| window.reset_layout_stats())
        .unwrap();
    window.update(cx, |_, _, cx| cx.notify()).unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.layout_stats()
    })
    .unwrap()
}

/// An element built again only because its text was measured again did not
/// move, so it is recorded on the very next frame and drawn again on the one
/// after, rather than waiting a frame to be found standing still.
#[gpui::test]
fn text_measured_again_in_place_is_drawn_again_two_frames_later(cx: &mut TestAppContext) {
    let window: WindowHandle<Narrowing> = cx.add_window(|_, _| Narrowing { width: 200. });
    for _ in 0..3 {
        draw_notified(window, cx);
    }
    assert!(draw_notified(window, cx).elements_reused > 0);

    window
        .update(cx, |narrowing, _, _| narrowing.width = 150.)
        .unwrap();
    let stats = draw_notified(window, cx);
    assert!(stats.measure_calls > 0, "{stats:?}");
    assert_eq!(stats.elements_reused, 0, "{stats:?}");

    let stats = draw_notified(window, cx);
    assert_eq!(stats.elements_reused, 0, "recorded again: {stats:?}");
    let stats = draw_notified(window, cx);
    assert!(stats.elements_reused > 0, "drawn again: {stats:?}");
}

struct Lists {
    list: crate::ListState,
}

fn list_row(ix: usize) -> impl IntoElement {
    div()
        .h(px(20.))
        .child(SharedString::from(format!("row {ix}")))
}

impl Render for Lists {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .child(
                crate::list(self.list.clone(), |ix, _, _| {
                    list_row(ix).into_any_element()
                })
                .w(px(100.))
                .h(px(200.)),
            )
            .child(
                crate::uniform_list("rows", 20, |range, _, _| {
                    range.map(list_row).collect::<Vec<_>>()
                })
                .w(px(100.))
                .h(px(200.)),
            )
    }
}

/// The items of a `list` and a `uniform_list` are laid out apart from the
/// view they are drawn in, and a list lays out the item it measures once
/// more; neither may keep an unchanged item from being drawn again.
#[gpui::test]
fn unchanged_list_items_are_drawn_again(cx: &mut TestAppContext) {
    let window: WindowHandle<Lists> = cx.add_window(|_, _| Lists {
        list: crate::ListState::new(20, crate::ListAlignment::Top, px(0.)),
    });
    for _ in 0..3 {
        draw_notified(window, cx);
    }
    let stats = draw_notified(window, cx);
    // Ten visible rows of each list, a div and its text each.
    assert_eq!(stats.elements_reused, 40, "{stats:?}");
    assert_eq!(stats.elements_built, 0, "{stats:?}");
}
