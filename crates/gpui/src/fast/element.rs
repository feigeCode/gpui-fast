//! Elements drawn again from what they drew on the last frame.
//!
//! A view that changed renders again and builds every element it holds anew,
//! though most of them are usually built just as they were: a quote board
//! whose price changed in one row builds every other row as it did. Those
//! elements are still built, but once built, each is compared with the
//! element built at its place last frame; one built the same way, in every
//! element nested in it too, is drawn again from what it drew then, as a
//! view whose dependencies did not change is (see [`crate::fast::retained`]):
//! its layout nodes are kept rather than requested, and its hitboxes,
//! dispatch nodes and primitives are copied from last frame's.
//!
//! Only elements whose output is fully decided by what they were built with
//! and where they are drawn take part: a `div` with a style and children but
//! no listener, focus, scroll, hover or other interactive state, and plain
//! text. What they were built with is compared exactly, a `div`'s style
//! refinement with the one kept from last frame, text with its text; where
//! they are drawn — bounds, content mask, opacity, text style and rem size —
//! is compared too. Nothing they draw can depend on anything else, so they
//! read nothing a view would have to depend on.
//!
//! An element is found again by its layout key: its path from the root of
//! the element tree, each step an `ElementId` or a position among siblings,
//! as its layout node is (see [`crate::fast::layout_key`]). An element that
//! differs is built as upstream builds it, but the elements nested in it are
//! each compared on their own, so a row whose price changed builds the row
//! and the price, and draws the other cells of the row from last frame.
//!
//! Each element drawn this way leaves a record. The records of an element
//! nested in no other that does — a row, say — and of those nested in it
//! are frozen, once it is painted, into one [`Subtree`] shared from frame to
//! frame, their ranges relative to its own. Drawing the row again from last
//! frame takes that subtree over as it is, however many elements it holds;
//! only elements drawn again inside one built this frame copy their records.

use crate::fast::layout_key::{KeyPosition, key_position, pop_layout_key, push_layout_key};
use crate::window::{PaintIndex, PrepaintStateIndex};
use crate::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Div, Drawable, Element, ElementId,
    HitboxBehavior, Interactivity, LayoutId, Overflow, Pixels, SharedString, Size, Stateful,
    StyleRefinement, Text, TextStyle, Window,
};
use collections::FxHashMap;
use std::{
    any::{Any, TypeId},
    cell::OnceCell,
    mem,
    ops::Range,
    rc::Rc,
};

impl Window {
    /// Sets whether an element built this frame as it was built on the last
    /// one is drawn again from what it drew then, which is the default. It
    /// takes view retention to be on as well (see
    /// [`Window::set_view_retention`]): with that off, every element is drawn
    /// from scratch each frame, as upstream GPUI draws it.
    ///
    /// The `GPUI_ELEMENT_RETENTION=0` environment variable turns it off for
    /// every window.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_element_retention(&mut self, enabled: bool) {
        if self.retained_state.element_retention != enabled {
            self.retained_state.element_retention = enabled;
            self.refresh();
        }
    }
}

/// The records of the elements drawn in one frame, for the next one to draw
/// them again from. Kept in the frame's
/// [`crate::fast::retained::RetainedSubtrees`].
#[derive(Default)]
pub(crate) struct ElementRecords {
    /// A record per element nested in no other element with one, in the
    /// order they began prepainting.
    roots: Vec<Root>,
    by_key: FxHashMap<u64, u32>,
    /// The records of the root being prepainted, its own first, each
    /// followed by those nested in it, with ranges in this frame.
    building: Vec<ElementRecord>,
    /// The root whose records `building` holds.
    building_root: u32,
    /// The records in `building` whose prepaint is under way, innermost
    /// last.
    open: Vec<u32>,
}

/// An element nested in no other element with a record, and where its
/// subtree of records was drawn.
struct Root {
    key: u64,
    records: RootRecords,
    /// For a root left without records, where it was drawn. See
    /// [`Placement`].
    placement: Option<Placement>,
    prepaint_start: PrepaintStateIndex,
    /// Where its paint started, or, while [`Paint::Pending`], where it
    /// started in the frame it is copied from.
    paint_start: PaintIndex,
    paint: Paint,
    /// Whether it is the only root under its key.
    usable: bool,
}

/// Where a root left without records was drawn, and whether that is where
/// it was drawn the frame before.
///
/// A root starts out drawn as upstream draws it, with everything nested in
/// it, and is recorded only once it is drawn twice in a row where it was:
/// rows under a scroll, or below rows inserted above them, are drawn
/// somewhere else every frame and could never be drawn again from the last
/// one, so they are not compared, kept and built again every frame for
/// nothing. A recorded root found to have moved goes back to that.
#[derive(Clone, Copy)]
struct Placement {
    bounds: Bounds<Pixels>,
    still: bool,
}

/// Whether an element nested in no other with a record is recorded this
/// frame. See [`Placement`].
enum Probation {
    /// It was recorded last frame, or stood still.
    Record,
    /// It moved, or was not drawn, last frame: drawn as upstream draws it,
    /// its placement noted, last frame's bounds if it had one.
    Skip(Option<Bounds<Pixels>>),
}

/// The records of a root and those nested in it.
enum RootRecords {
    /// Being prepainted, in `building`.
    Building,
    /// Prepainted, with prepaint ranges relative to the root's, and paint
    /// ranges in this frame as they are painted.
    Pending(Box<[ElementRecord]>),
    /// Painted.
    Frozen(Rc<Subtree>),
    /// Never painted, or carried along with something drawn again from last
    /// frame without having been painted there, or not recorded while it
    /// skips being compared.
    Lost,
}

/// The records of an element and of the elements nested in it, its own
/// first, each followed by those nested in it, with prepaint and paint
/// ranges relative to its own. Shared by every frame that draws it again.
struct Subtree {
    records: Box<[ElementRecord]>,
    /// The records by key, for an element whose place among its siblings
    /// changed, worked out the first time one is looked for.
    by_key: OnceCell<FxHashMap<u64, u32>>,
}

impl Subtree {
    fn find(&self, key: u64) -> Option<u32> {
        self.by_key
            .get_or_init(|| {
                self.records
                    .iter()
                    .enumerate()
                    .map(|(index, record)| (record.key, index as u32))
                    .collect()
            })
            .get(&key)
            .copied()
    }
}

#[derive(Clone)]
struct ElementRecord {
    key: u64,
    snapshot: Snapshot,
    /// How many of the records following this one are nested inside it.
    nested: u32,
    /// Whether every child of its element left a record, in order, among
    /// the nested ones: none went unprepainted under a `display: none`.
    complete: bool,
    /// Whether it is complete, everything nested in it recorded, and its
    /// layout claimed the nodes of these records and no others, so that it
    /// can be drawn again from them.
    reusable: bool,
    paint: Paint,
    /// For [`Snapshot::Moving`], whether it was drawn where it was the frame
    /// before.
    still: bool,
    layout_id: LayoutId,
    /// While it is being built, how many layout nodes it claimed.
    claimed: u32,
    context: ElementContext,
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
}

#[derive(Clone, Copy, PartialEq)]
enum Paint {
    /// Not painted, so its paint range means nothing.
    Unpainted,
    /// Painted into its paint range.
    Painted,
    /// Drawn again from the frame before and holding the paint range it had
    /// there, until what it is drawn again with is painted.
    Pending,
}

/// Where an element was drawn, and what it inherited there.
#[derive(Clone)]
struct ElementContext {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    opacity: f32,
    text_style: Rc<TextStyle>,
    rem_size: Pixels,
}

impl ElementContext {
    fn matches(&self, other: &Self) -> bool {
        self.bounds == other.bounds
            && self.content_mask == other.content_mask
            && self.opacity == other.opacity
            && self.rem_size == other.rem_size
            && same_text_style(&self.text_style, &other.text_style)
    }
}

fn same_text_style(a: &Rc<TextStyle>, b: &Rc<TextStyle>) -> bool {
    Rc::ptr_eq(a, b) || **a == **b
}

/// What an element was built with, besides its children, which have
/// records of their own.
#[derive(Clone)]
enum Snapshot {
    Div {
        id: Option<ElementId>,
        /// Taken from the element once it is painted, when not lent by the
        /// record of last frame it was built as.
        style: Option<Rc<StyleRefinement>>,
        children: u32,
    },
    Text {
        id: Option<ElementId>,
        text: SharedString,
    },
    /// Not recorded, with nothing nested in it, because it was drawn
    /// somewhere else than the frame before; it is recorded again once it
    /// stands still. See [`Placement`], which does this for roots.
    Moving,
}

/// A record of last frame: the record `index` of the subtree of its root
/// `root`.
#[derive(Clone, Copy, PartialEq)]
struct PrevRef {
    root: u32,
    index: u32,
}

impl ElementRecords {
    /// How many roots this frame holds so far, for what is drawn from here
    /// on to know where its roots start.
    pub(crate) fn len(&self) -> u32 {
        self.roots.len() as u32
    }

    pub(crate) fn clear(&mut self) {
        self.roots.clear();
        self.by_key.clear();
        self.building.clear();
        self.open.clear();
    }

    /// The painted subtree of the root `root`, if it can be drawn again from.
    fn subtree(&self, root: u32) -> Option<&Rc<Subtree>> {
        let root = &self.roots[root as usize];
        match &root.records {
            RootRecords::Frozen(subtree) if root.paint == Paint::Painted && root.usable => {
                Some(subtree)
            }
            _ => None,
        }
    }

    /// The records of the root `root`, pending their paint.
    fn pending(&mut self, root: u32) -> &mut [ElementRecord] {
        match &mut self.roots[root as usize].records {
            RootRecords::Pending(records) => records,
            _ => panic!("a root is painted once, after it is prepainted"),
        }
    }

    fn record(&self, previous: PrevRef) -> &ElementRecord {
        &self
            .subtree(previous.root)
            .expect("a record found is painted")
            .records[previous.index as usize]
    }

    /// Where `previous`'s prepaint and paint went in this frame.
    fn ranges(&self, previous: PrevRef) -> (Range<PrepaintStateIndex>, Range<PaintIndex>) {
        let root = &self.roots[previous.root as usize];
        let record = self.record(previous);
        let zero_prepaint = PrepaintStateIndex::default();
        let zero_paint = PaintIndex::default();
        (
            record
                .prepaint_range
                .start
                .shifted(&zero_prepaint, &root.prepaint_start)
                ..record
                    .prepaint_range
                    .end
                    .shifted(&zero_prepaint, &root.prepaint_start),
            record
                .paint_range
                .start
                .shifted(&zero_paint, &root.paint_start)
                ..record
                    .paint_range
                    .end
                    .shifted(&zero_paint, &root.paint_start),
        )
    }

    /// The record the element with layout key `key` left, if it can be drawn
    /// again from it: the one the element around it pointed it to if that
    /// has its key, or else one under its key where that one was, or else a
    /// root under its key.
    fn find(&self, candidate: Option<PrevRef>, key: u64) -> Option<PrevRef> {
        if let Some(candidate) = candidate
            && let Some(subtree) = self.subtree(candidate.root)
        {
            if subtree.records[candidate.index as usize].key == key {
                return Some(candidate);
            }
            if let Some(index) = subtree.find(key) {
                return Some(PrevRef {
                    root: candidate.root,
                    index,
                });
            }
        }
        let root = *self.by_key.get(&key)?;
        self.subtree(root)?;
        Some(PrevRef { root, index: 0 })
    }

    /// Whether the element under `key`, should it be a root, is recorded.
    fn probation(&self, key: u64) -> Probation {
        let Some(&root) = self.by_key.get(&key) else {
            return Probation::Skip(None);
        };
        let root = &self.roots[root as usize];
        match (&root.records, root.placement) {
            _ if !root.usable => Probation::Skip(None),
            (RootRecords::Frozen(_), _) if root.paint == Paint::Painted => Probation::Record,
            (_, Some(Placement { still: true, .. })) => Probation::Record,
            (_, placement) => Probation::Skip(placement.map(|placement| placement.bounds)),
        }
    }

    fn push_root(&mut self, root: Root) -> u32 {
        let index = self.roots.len() as u32;
        // Two roots under one key can only both be drawn; neither can be
        // found to be drawn again.
        let mut usable = true;
        match self.by_key.entry(root.key) {
            collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(index);
            }
            collections::hash_map::Entry::Occupied(entry) => {
                self.roots[*entry.get() as usize].usable = false;
                usable = false;
            }
        }
        self.roots.push(Root { usable, ..root });
        index
    }

    /// Sets the records of the root being built aside, now that it is
    /// prepainted, their prepaint ranges made relative to its own.
    fn finish_prepaint(&mut self) {
        let start = self.building[0].prepaint_range.start.clone();
        let zero = PrepaintStateIndex::default();
        for record in &mut self.building {
            record.prepaint_range = record.prepaint_range.start.shifted(&start, &zero)
                ..record.prepaint_range.end.shifted(&start, &zero);
        }
        let records = self.building.drain(..).collect();
        let root = &mut self.roots[self.building_root as usize];
        root.records = RootRecords::Pending(records);
        root.prepaint_start = start;
    }

    /// Freezes the records of the root `root`, now painted, into its
    /// subtree, their paint ranges made relative to its own.
    fn freeze(&mut self, root: u32) {
        let root = &mut self.roots[root as usize];
        let RootRecords::Pending(mut records) = mem::replace(&mut root.records, RootRecords::Lost)
        else {
            panic!("a root is painted once, after it is prepainted");
        };
        let start = records[0].paint_range.start.clone();
        let zero = PaintIndex::default();
        for record in records.iter_mut() {
            if record.paint == Paint::Painted {
                record.paint_range = record.paint_range.start.shifted(&start, &zero)
                    ..record.paint_range.end.shifted(&start, &zero);
            } else {
                record.paint = Paint::Unpainted;
            }
        }
        root.records = RootRecords::Frozen(Rc::new(Subtree {
            records,
            by_key: OnceCell::new(),
        }));
        root.paint_start = start;
        root.paint = Paint::Painted;
    }
}

/// Carries last frame's roots `range` into this frame, for what they were
/// drawn in is drawn again from last frame, with the prepaint that started
/// at `from` copied to `to`. Their paint is placed by
/// [`place_carried_paint`]. Returns where they landed.
pub(crate) fn carry_records(
    source: &ElementRecords,
    target: &mut ElementRecords,
    range: Range<u32>,
    from: &PrepaintStateIndex,
    to: &PrepaintStateIndex,
) -> Range<u32> {
    let start = target.len();
    for root in &source.roots[range.start as usize..range.end as usize] {
        let (records, paint) = match (&root.records, root.paint) {
            (RootRecords::Frozen(subtree), Paint::Painted) => {
                (RootRecords::Frozen(subtree.clone()), Paint::Pending)
            }
            _ => (RootRecords::Lost, Paint::Unpainted),
        };
        target.push_root(Root {
            key: root.key,
            records,
            placement: root.placement,
            prepaint_start: root.prepaint_start.shifted(from, to),
            paint_start: root.paint_start.clone(),
            paint,
            usable: true,
        });
    }
    start..target.len()
}

/// Places the paint of the roots `range` [`carry_records`] carried, now
/// that the paint that started at `from` last frame was copied to `to`.
pub(crate) fn place_carried_paint(
    records: &mut ElementRecords,
    range: Range<u32>,
    from: &PaintIndex,
    to: &PaintIndex,
) {
    for root in &mut records.roots[range.start as usize..range.end as usize] {
        if root.paint == Paint::Pending {
            root.paint_start = root.paint_start.shifted(from, to);
            root.paint = Paint::Painted;
        }
    }
}

/// What a [`Drawable`] keeps to be drawn from last frame. See
/// [`crate::fast::element`].
#[derive(Default)]
pub(crate) struct DrawableRetention {
    /// Whether the element, and everything nested in it, is of a kind that
    /// can be drawn again from last frame, once worked out.
    eligible: Option<bool>,
    /// The record of last frame at the element's place among its siblings,
    /// as the element around it found it.
    candidate: Option<PrevRef>,
    /// The record of last frame the element was last compared with, and
    /// whether it, and everything nested in it, was built the same way.
    matched: Option<(PrevRef, bool)>,
    /// The style of the record it was compared with, when it was built with
    /// the same.
    lent_style: Option<Rc<StyleRefinement>>,
    phase: Phase,
}

#[derive(Default)]
enum Phase {
    /// Its layout not requested yet.
    #[default]
    Start,
    /// Drawn as upstream draws it, without a record.
    Plain,
    /// Drawn as upstream draws it, with everything nested in it, while it
    /// is not recorded. See [`Placement`].
    Skipped {
        key: u64,
        layout_id: LayoutId,
        previous: Option<Bounds<Pixels>>,
    },
    /// Built and laid out; it leaves a record once prepainted.
    Built(BuiltLayout),
    /// Laid out at the nodes a record of last frame kept, without being
    /// built.
    Kept(KeptLayout),
    /// Prepainted into the record `index` of the root `root`.
    Recorded { root: u32, index: u32 },
    /// Drawn from last frame as far as prepaint, as the root at this index.
    ReusedRoot(u32),
    /// Drawn from last frame as far as prepaint, into the records starting
    /// at `index` of the root `root`.
    ReusedNested { root: u32, index: u32 },
}

struct BuiltLayout {
    key: u64,
    snapshot: Snapshot,
    layout_id: LayoutId,
    claimed: u32,
    /// Where its record of last frame was drawn, for a moving one to tell
    /// whether it stood still.
    previous_bounds: Option<Bounds<Pixels>>,
}

struct KeptLayout {
    previous: PrevRef,
    key: u64,
    layout_id: LayoutId,
    /// Where it was begun, to request its layout after all.
    position: KeyPosition,
}

/// An element as an element nested in another sees it, for the one around
/// it to compare it with last frame's.
pub(crate) struct ElementParts<'a> {
    element: &'a mut dyn Any,
    state: &'a mut DrawableRetention,
    id: fn(&dyn Any) -> Option<ElementId>,
}

impl<'a> ElementParts<'a> {
    pub(crate) fn new<E: Element>(element: &'a mut E, state: &'a mut DrawableRetention) -> Self {
        ElementParts {
            element,
            state,
            id: |element| element.downcast_ref::<E>().and_then(Element::id),
        }
    }

    /// The element's [`Element::id`].
    pub(crate) fn element_id(&self) -> Option<ElementId> {
        (self.id)(self.element)
    }
}

/// The type of [`Div`]'s listener for its children's bounds, named here so
/// that its field fits a line.
pub(crate) type PrepaintListener =
    Option<Box<dyn Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static>>;

/// The type of [`Div`]'s function ordering its children's prepaint, named
/// here so that its field fits a line.
pub(crate) type PrepaintOrderFn =
    Option<Box<dyn Fn(&mut Window, &mut App) -> smallvec::SmallVec<[usize; 8]>>>;

/// The element types that can be drawn from last frame. A `Drawable` of any
/// other type goes straight to upstream's drawing, decided at compile time.
#[inline(always)]
fn retainable_type<E: 'static>() -> bool {
    let id = TypeId::of::<E>();
    id == TypeId::of::<Div>()
        || id == TypeId::of::<Stateful<Div>>()
        || id == TypeId::of::<SharedString>()
        || id == TypeId::of::<&'static str>()
        || id == TypeId::of::<Text>()
}

/// Whether anything can be drawn from last frame in `window` this frame.
fn active(window: &Window, cx: &App) -> bool {
    #[cfg(any(feature = "inspector", debug_assertions))]
    if window.inspector_enabled() {
        return false;
    }
    let state = &window.retained_state;
    state.element_retention
        && state.view_retention
        && !window.refreshing
        && !cx.has_active_drag()
        && !window.a11y.is_active()
}

/// What an element is, as far as being drawn again goes.
enum Own<'a> {
    Div(&'a mut Div),
    Text {
        id: Option<&'a ElementId>,
        text: TextRef<'a>,
    },
}

enum TextRef<'a> {
    Shared(&'a SharedString),
    Static(&'static str),
}

impl TextRef<'_> {
    fn as_str(&self) -> &str {
        match self {
            TextRef::Shared(text) => text,
            TextRef::Static(text) => text,
        }
    }

    fn to_shared(&self) -> SharedString {
        match self {
            TextRef::Shared(text) => (*text).clone(),
            TextRef::Static(text) => SharedString::new_static(text),
        }
    }
}

fn own(element: &mut dyn Any) -> Option<Own<'_>> {
    if element.is::<Div>() {
        return element.downcast_mut::<Div>().map(Own::Div);
    }
    if element.is::<Stateful<Div>>() {
        return element
            .downcast_mut::<Stateful<Div>>()
            .map(|stateful| Own::Div(&mut stateful.element));
    }
    if element.is::<SharedString>() {
        let text = element.downcast_ref::<SharedString>()?;
        return Some(Own::Text {
            id: None,
            text: TextRef::Shared(text),
        });
    }
    if element.is::<&'static str>() {
        let text = *element.downcast_ref::<&'static str>()?;
        return Some(Own::Text {
            id: None,
            text: TextRef::Static(text),
        });
    }
    let text = element.downcast_ref::<Text>()?;
    Some(Own::Text {
        id: text.id.as_ref(),
        text: TextRef::Shared(&text.text),
    })
}

impl Own<'_> {
    /// What it was built with, for a record, but for a `div`'s style, which
    /// `lent_style` is, when a record of last frame lends it.
    fn snapshot(&self, lent_style: Option<Rc<StyleRefinement>>) -> Snapshot {
        match self {
            Own::Div(div) => Snapshot::Div {
                id: div.interactivity.element_id.clone(),
                style: lent_style,
                children: div.children.len() as u32,
            },
            Own::Text { id, text } => Snapshot::Text {
                id: id.cloned(),
                text: text.to_shared(),
            },
        }
    }
}

/// Whether a `div` draws nothing but its style and its children: no
/// listener, and no interactive state that changes what it draws.
fn plain_div(div: &Div) -> bool {
    let Div {
        interactivity,
        children: _,
        prepaint_listener,
        image_cache,
        prepaint_order_fn,
    } = div;
    prepaint_listener.is_none()
        && image_cache.is_none()
        && prepaint_order_fn.is_none()
        && plain_interactivity(interactivity)
}

fn plain_interactivity(interactivity: &Interactivity) -> bool {
    // Destructured, so that a field upstream adds can't be missed here.
    let Interactivity {
        element_id: _,
        active: _,
        hovered: _,
        tooltip_id: _,
        content_size: _,
        key_context,
        focusable,
        tracked_focus_handle,
        tracked_scroll_handle,
        scroll_anchor,
        scroll_offset,
        ongoing_scroll,
        group,
        base_style,
        focus_style,
        in_focus_style,
        focus_visible_style,
        hover_style,
        group_hover_style,
        active_style,
        group_active_style,
        drag_over_styles,
        group_drag_over_styles,
        mouse_down_listeners,
        mouse_up_listeners,
        mouse_pressure_listeners,
        mouse_move_listeners,
        mouse_exit_listeners,
        file_drop_exit_listeners,
        scroll_wheel_listeners,
        pinch_listeners,
        key_down_listeners,
        key_up_listeners,
        modifiers_changed_listeners,
        action_listeners,
        drop_listeners,
        can_drop_predicate,
        click_listeners,
        aux_click_listeners,
        drag_listener,
        hover_listener,
        hover_listener_mode: _,
        tooltip_builder,
        tooltip_show_delay: _,
        window_control,
        hitbox_behavior,
        tab_index,
        tab_group,
        tab_stop: _,
        a11y_action_listeners,
        a11y_synthetic_children,
        report_active_descendant_focus,
        override_role,
        aria: _,
        #[cfg(any(feature = "inspector", debug_assertions))]
            source_location: _,
        #[cfg(any(test, feature = "test-support"))]
            debug_selector: _,
    } = interactivity;
    key_context.is_none()
        && !focusable
        && tracked_focus_handle.is_none()
        && tracked_scroll_handle.is_none()
        && scroll_anchor.is_none()
        && scroll_offset.is_none()
        && ongoing_scroll.is_none()
        && group.is_none()
        && base_style.overflow.x != Some(Overflow::Scroll)
        && base_style.overflow.y != Some(Overflow::Scroll)
        && base_style.mouse_cursor.is_none()
        && focus_style.is_none()
        && in_focus_style.is_none()
        && focus_visible_style.is_none()
        && hover_style.is_none()
        && group_hover_style.is_none()
        && active_style.is_none()
        && group_active_style.is_none()
        && drag_over_styles.is_empty()
        && group_drag_over_styles.is_empty()
        && mouse_down_listeners.is_empty()
        && mouse_up_listeners.is_empty()
        && mouse_pressure_listeners.is_empty()
        && mouse_move_listeners.is_empty()
        && mouse_exit_listeners.is_empty()
        && file_drop_exit_listeners.is_empty()
        && scroll_wheel_listeners.is_empty()
        && pinch_listeners.is_empty()
        && key_down_listeners.is_empty()
        && key_up_listeners.is_empty()
        && modifiers_changed_listeners.is_empty()
        && action_listeners.is_empty()
        && drop_listeners.is_empty()
        && can_drop_predicate.is_none()
        && click_listeners.is_empty()
        && aux_click_listeners.is_empty()
        && drag_listener.is_none()
        && hover_listener.is_none()
        && tooltip_builder.is_none()
        && window_control.is_none()
        && *hitbox_behavior == HitboxBehavior::Normal
        && tab_index.is_none()
        && !tab_group
        && a11y_action_listeners.is_empty()
        && a11y_synthetic_children.is_none()
        && !report_active_descendant_focus
        && override_role.is_none()
}

fn child_parts(child: &mut AnyElement) -> ElementParts<'_> {
    child.0.fast_retention()
}

/// Whether `parts` and every element nested in it can be drawn again from
/// last frame.
fn subtree_eligible(parts: ElementParts) -> bool {
    if let Some(eligible) = parts.state.eligible {
        return eligible;
    }
    let eligible = match own(parts.element) {
        Some(Own::Div(div)) => {
            plain_div(div)
                && div.children.iter_mut().all(|child| {
                    let child: &mut AnyElement = child;
                    subtree_eligible(child_parts(child))
                })
        }
        Some(Own::Text { .. }) => true,
        None => false,
    };
    parts.state.eligible = Some(eligible);
    eligible
}

/// Whether `parts`, eligible, and every element nested in it were built as
/// the record `previous` of `subtree` and those nested in it were. Points
/// the children of `parts` to the records at their places on the way.
fn subtree_matches(parts: ElementParts, subtree: &Subtree, previous: PrevRef) -> bool {
    if let Some((matched, result)) = parts.state.matched
        && matched == previous
    {
        return result;
    }
    let records = &subtree.records;
    let index = previous.index as usize;
    let record = &records[index];
    let mut own = own(parts.element);
    // Its children pointed to the records at their places, which those with
    // the keys they had there take up, whether it was built as it was or not.
    if let (Some(Own::Div(div)), Snapshot::Div { children, .. }) = (&mut own, &record.snapshot)
        && record.complete
    {
        let mut child_index = index + 1;
        for child in div.children.iter_mut().take(*children as usize) {
            let child: &mut AnyElement = child;
            child_parts(child).state.candidate = Some(PrevRef {
                root: previous.root,
                index: child_index as u32,
            });
            child_index += records[child_index].nested as usize + 1;
        }
    }
    let result = record.reusable
        && record.paint == Paint::Painted
        && match (own, &record.snapshot) {
            (
                Some(Own::Div(div)),
                Snapshot::Div {
                    id,
                    style,
                    children,
                },
            ) => {
                let same_style = style
                    .as_ref()
                    .is_some_and(|style| **style == *div.interactivity.base_style);
                if same_style {
                    parts.state.lent_style = style.clone();
                }
                div.children.len() == *children as usize
                    && div.interactivity.element_id == *id
                    && same_style
                    && {
                        let mut child_index = index + 1;
                        div.children.iter_mut().all(|child| {
                            let child: &mut AnyElement = child;
                            let child_ref = PrevRef {
                                root: previous.root,
                                index: child_index as u32,
                            };
                            child_index += records[child_index].nested as usize + 1;
                            subtree_matches(child_parts(child), subtree, child_ref)
                        })
                    }
            }
            (
                Some(Own::Text { id, text }),
                Snapshot::Text {
                    id: previous_id,
                    text: previous_text,
                },
            ) => id == previous_id.as_ref() && text.as_str() == previous_text.as_ref(),
            _ => false,
        };
    parts.state.matched = Some((previous, result));
    result
}

/// Requests `drawable`'s layout as [`Drawable::request_layout`] does, or,
/// when it was built as it was last frame, keeps the nodes it had then
/// without building it.
#[inline(always)]
pub(crate) fn request_layout<E: Element>(
    drawable: &mut Drawable<E>,
    window: &mut Window,
    cx: &mut App,
) -> LayoutId {
    if !retainable_type::<E>() || !matches!(drawable.fast_retention.phase, Phase::Start) {
        return drawable.request_layout(window, cx);
    }
    request_retained_layout(drawable, window, cx)
}

#[inline(never)]
fn request_retained_layout<E: Element>(
    drawable: &mut Drawable<E>,
    window: &mut Window,
    cx: &mut App,
) -> LayoutId {
    if !active(window, cx) || window.retained_state.skipping_elements > 0 {
        drawable.fast_retention.phase = Phase::Plain;
        return drawable.request_layout(window, cx);
    }
    let id = drawable.element.id();
    let position = key_position(window);
    let key = position.key(id.as_ref());
    if !subtree_eligible(ElementParts::new(
        &mut drawable.element,
        &mut drawable.fast_retention,
    )) {
        drawable.fast_retention.phase = Phase::Plain;
        return drawable.request_layout(window, cx);
    }
    if window.retained_state.recording_elements == 0
        && let Probation::Skip(previous) = window.rendered_frame.retained.elements.probation(key)
    {
        let layout_id = request_skipped_layout(drawable, window, cx);
        drawable.fast_retention.phase = Phase::Skipped {
            key,
            layout_id,
            previous,
        };
        return layout_id;
    }
    let previous = window
        .rendered_frame
        .retained
        .elements
        .find(drawable.fast_retention.candidate, key);

    // Nested in an element being recorded, but moving: recorded as such,
    // with nothing nested in it, until it stands still.
    if window.retained_state.recording_elements > 0
        && let Some(previous) = previous
        && let record = window.rendered_frame.retained.elements.record(previous)
        && matches!(record.snapshot, Snapshot::Moving)
        && !record.still
    {
        let previous_bounds = Some(record.context.bounds);
        let layout_id = request_skipped_layout(drawable, window, cx);
        drawable.fast_retention.phase = Phase::Built(BuiltLayout {
            key,
            snapshot: Snapshot::Moving,
            layout_id,
            claimed: 0,
            previous_bounds,
        });
        return layout_id;
    }

    if let Some(previous) = previous
        && let Some(layout_id) = keep_layout(
            ElementParts::new(&mut drawable.element, &mut drawable.fast_retention),
            previous,
            window,
        )
    {
        // Begun and ended, for its siblings to be keyed as they would be.
        push_layout_key(window, id.as_ref());
        pop_layout_key(window);
        drawable.fast_retention.phase = Phase::Kept(KeptLayout {
            previous,
            key,
            layout_id,
            position,
        });
        return layout_id;
    }

    let lent_style = drawable.fast_retention.lent_style.take();
    let snapshot = own(&mut drawable.element)
        .expect("an eligible element is one of its own kind")
        .snapshot(lent_style);
    window.retained_state.recording_elements += 1;
    let (layout_id, claimed) = count_layout(window, |window| drawable.request_layout(window, cx));
    window.retained_state.recording_elements -= 1;
    drawable.fast_retention.phase = match claimed {
        Some(claimed) => Phase::Built(BuiltLayout {
            key,
            snapshot,
            layout_id,
            claimed,
            previous_bounds: None,
        }),
        None => Phase::Plain,
    };
    layout_id
}

/// Requests `drawable`'s layout as upstream does, and that of everything
/// nested in it, none of it recorded.
fn request_skipped_layout<E: Element>(
    drawable: &mut Drawable<E>,
    window: &mut Window,
    cx: &mut App,
) -> LayoutId {
    window.retained_state.skipping_elements += 1;
    let layout_id = drawable.request_layout(window, cx);
    window.retained_state.skipping_elements -= 1;
    layout_id
}

/// Puts the keys of the layout nodes `previous` and the records nested in it
/// hold in `keys`.
fn gather_keys(records: &ElementRecords, previous: PrevRef, keys: &mut Vec<u64>) {
    let subtree = records
        .subtree(previous.root)
        .expect("a record found is painted");
    let start = previous.index as usize;
    let end = start + subtree.records[start].nested as usize + 1;
    keys.clear();
    keys.extend(subtree.records[start..end].iter().map(|record| record.key));
}

/// Keeps the layout nodes of last frame's record `previous` for `element`,
/// if it was built as that record's element was, inherits what it did, and
/// the nodes are all still there, returning the node it is laid out at.
fn keep_layout(parts: ElementParts, previous: PrevRef, window: &mut Window) -> Option<LayoutId> {
    let records = &window.rendered_frame.retained.elements;
    let subtree = records.subtree(previous.root)?;
    let record = &subtree.records[previous.index as usize];
    if record.context.rem_size != window.rem_size()
        || !same_text_style(
            &record.context.text_style,
            &crate::fast::text_style::text_style(window),
        )
        || !subtree_matches(parts, subtree, previous)
    {
        return None;
    }
    let layout_id = record.layout_id;
    let keys = &mut window.retained_state.element_keys;
    gather_keys(records, previous, keys);
    let engine = window.layout_engine.as_mut().unwrap();
    engine.try_keep_retained(keys).then_some(layout_id)
}

/// Requests a layout with `request`, counting the layout nodes it claims.
/// Returns the count unless it allocated a node that is gone at the end of
/// the frame, which nothing can be drawn again from.
fn count_layout(
    window: &mut Window,
    request: impl FnOnce(&mut Window) -> LayoutId,
) -> (LayoutId, Option<u32>) {
    let engine = window.layout_engine.as_mut().unwrap();
    let recording = engine.record_claimed_keys();
    let transient = engine.transient_count();
    let layout_id = request(window);
    let engine = window.layout_engine.as_mut().unwrap();
    let claimed = engine.finish_counting_claimed_keys(recording);
    let kept = engine.transient_count() == transient;
    (layout_id, kept.then_some(claimed as u32))
}

/// Lays `drawable` out as [`Drawable::layout_as_root`] does, or, when its
/// layout was kept from last frame, computes it at the node it kept.
#[inline(always)]
pub(crate) fn layout_as_root<E: Element>(
    drawable: &mut Drawable<E>,
    available_space: Size<AvailableSpace>,
    window: &mut Window,
    cx: &mut App,
) -> Size<Pixels> {
    if retainable_type::<E>() {
        if matches!(drawable.fast_retention.phase, Phase::Start) {
            request_layout(drawable, window, cx);
        }
        if let Phase::Kept(kept) = &drawable.fast_retention.phase {
            let layout_id = kept.layout_id;
            window.compute_layout(layout_id, available_space, cx);
            return window.layout_bounds(layout_id).size;
        }
    }
    drawable.layout_as_root(available_space, window, cx)
}

/// Prepaints `drawable` as [`Drawable::prepaint`] does, recording what it
/// prepaints, or draws it again from last frame when it was built as it was
/// then and is drawn where it was.
#[inline(always)]
pub(crate) fn prepaint<E: Element>(drawable: &mut Drawable<E>, window: &mut Window, cx: &mut App) {
    if !retainable_type::<E>() {
        return drawable.prepaint(window, cx);
    }
    prepaint_retained(drawable, window, cx)
}

#[inline(never)]
fn prepaint_retained<E: Element>(drawable: &mut Drawable<E>, window: &mut Window, cx: &mut App) {
    let built = match mem::take(&mut drawable.fast_retention.phase) {
        Phase::Built(built) => built,
        Phase::Kept(kept) => {
            if let Some(phase) = reuse_prepaint(&kept, window) {
                drawable.fast_retention.phase = phase;
                return;
            }
            let root = window.next_frame.retained.elements.open.is_empty();
            match build_at_kept_layout(drawable, &kept, root, window, cx) {
                Some(built) => built,
                None => {
                    if root {
                        // Drawn as upstream draws it, and recorded from the
                        // next frame on if it stood where it was.
                        let bounds = window.layout_bounds(kept.layout_id);
                        let still = previous_bounds(&kept, window) == bounds;
                        leave_placement(kept.key, bounds, still, window);
                    }
                    drawable.fast_retention.phase = Phase::Plain;
                    return drawable.prepaint(window, cx);
                }
            }
        }
        Phase::Skipped {
            key,
            layout_id,
            previous,
        } => {
            let bounds = window.layout_bounds(layout_id);
            leave_placement(key, bounds, previous == Some(bounds), window);
            drawable.fast_retention.phase = Phase::Plain;
            return drawable.prepaint(window, cx);
        }
        phase => {
            drawable.fast_retention.phase = phase;
            return drawable.prepaint(window, cx);
        }
    };
    let (root, index) = begin_record(built, window);
    drawable.prepaint(window, cx);
    finish_record(index, window);
    drawable.fast_retention.phase = Phase::Recorded { root, index };
}

/// Where the record `kept` kept its layout from was drawn last frame.
fn previous_bounds(kept: &KeptLayout, window: &Window) -> Bounds<Pixels> {
    window
        .rendered_frame
        .retained
        .elements
        .record(kept.previous)
        .context
        .bounds
}

/// Builds an element whose layout was kept from last frame, but which cannot
/// be drawn again from it because it is drawn somewhere else: it moved, or
/// what it inherits changed, or its text was measured again. Its layout is requested now, as it would have
/// been, which finds the nodes it kept as they were.
#[inline(never)]
fn build_at_kept_layout<E: Element>(
    drawable: &mut Drawable<E>,
    kept: &KeptLayout,
    root: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<BuiltLayout> {
    let bounds = window.layout_bounds(kept.layout_id);
    let previous_bounds = previous_bounds(kept, window);
    {
        let records = &window.rendered_frame.retained.elements;
        let keys = &mut window.retained_state.element_keys;
        gather_keys(records, kept.previous, keys);
        window.layout_engine.as_mut().unwrap().release_kept(keys);
    }
    let changes = window.layout_changes();
    let remeasures = window.layout_remeasures();
    let layout_id = crate::fast::layout_key::with_key_position(window, &kept.position, |window| {
        request_skipped_layout(drawable, window, cx)
    });
    // Built as it was, it asks for the layout it had; should it ask for
    // another after all, it is laid out within the bounds it was given,
    // and on the next frame from scratch.
    let unchanged = layout_id == kept.layout_id && window.layout_changes() == changes;
    if !unchanged || window.layout_remeasures() != remeasures {
        window.relayout_in_place(layout_id, bounds.size.into(), cx);
    }
    if !unchanged {
        window.request_animation_frame();
    }
    // A root is left without records, one nested in an element being
    // recorded is recorded as moving; either is recorded in full from the
    // next frame on if it stood where it was.
    (!root).then_some(BuiltLayout {
        key: kept.key,
        snapshot: Snapshot::Moving,
        layout_id,
        claimed: 0,
        previous_bounds: Some(previous_bounds),
    })
}

/// Leaves a root without records for the element under `key`, drawn at
/// `bounds`, `still` if it was drawn there last frame too. See
/// [`Placement`].
fn leave_placement(key: u64, bounds: Bounds<Pixels>, still: bool, window: &mut Window) {
    if !window.next_frame.retained.elements.open.is_empty() {
        return;
    }
    let start = window.prepaint_index();
    window.next_frame.retained.elements.push_root(Root {
        key,
        records: RootRecords::Lost,
        placement: Some(Placement { bounds, still }),
        prepaint_start: start,
        paint_start: PaintIndex::default(),
        paint: Paint::Unpainted,
        usable: true,
    });
}

fn context(layout_id: LayoutId, window: &mut Window) -> ElementContext {
    ElementContext {
        bounds: window.layout_bounds(layout_id),
        content_mask: window.content_mask(),
        opacity: window.element_opacity,
        text_style: crate::fast::text_style::text_style(window),
        rem_size: window.rem_size(),
    }
}

/// Starts this frame's record of an element being prepainted, returning
/// the root it is in and where it is among its records.
fn begin_record(built: BuiltLayout, window: &mut Window) -> (u32, u32) {
    let context = context(built.layout_id, window);
    let start = window.prepaint_index();
    window
        .layout_engine
        .as_mut()
        .unwrap()
        .retention
        .stats
        .elements_built += 1;
    let elements = &mut window.next_frame.retained.elements;
    if elements.open.is_empty() {
        debug_assert!(elements.building.is_empty());
        elements.building_root = elements.push_root(Root {
            key: built.key,
            records: RootRecords::Building,
            placement: None,
            prepaint_start: start.clone(),
            paint_start: PaintIndex::default(),
            paint: Paint::Unpainted,
            usable: true,
        });
    }
    let index = elements.building.len() as u32;
    let still = built.previous_bounds == Some(context.bounds);
    elements.building.push(ElementRecord {
        key: built.key,
        snapshot: built.snapshot,
        nested: 0,
        complete: false,
        reusable: false,
        paint: Paint::Unpainted,
        still,
        layout_id: built.layout_id,
        claimed: built.claimed,
        context,
        prepaint_range: start.clone()..start,
        paint_range: PaintIndex::default()..PaintIndex::default(),
    });
    elements.open.push(index);
    (elements.building_root, index)
}

/// Ends the record [`begin_record`] started, once its element is
/// prepainted.
fn finish_record(index: u32, window: &mut Window) {
    let end = window.prepaint_index();
    let elements = &mut window.next_frame.retained.elements;
    debug_assert_eq!(elements.open.last(), Some(&index));
    elements.open.pop();
    let index = index as usize;
    let len = elements.building.len();
    let mut children = 0;
    let mut child = index + 1;
    while child < len {
        children += 1;
        child += elements.building[child].nested as usize + 1;
    }
    let record = &mut elements.building[index];
    let nested = (len - index - 1) as u32;
    record.nested = nested;
    record.prepaint_range.end = end;
    record.complete = match &record.snapshot {
        Snapshot::Div { children: all, .. } => children == *all,
        Snapshot::Text { .. } | Snapshot::Moving => children == 0,
    };
    record.reusable = record.complete
        && !matches!(record.snapshot, Snapshot::Moving)
        && record.claimed == nested + 1;
    if elements.open.is_empty() {
        elements.finish_prepaint();
    }
}

/// Draws the element whose layout `kept` kept again from last frame as far
/// as its prepaint goes, if it is drawn where it was, returning what its
/// paint takes over from there.
fn reuse_prepaint(kept: &KeptLayout, window: &mut Window) -> Option<Phase> {
    let context = context(kept.layout_id, window);
    let previous = kept.previous;
    let (prepaint_range, paint_range) = {
        let records = &window.rendered_frame.retained.elements;
        if !records.record(previous).context.matches(&context)
            || remeasured(records, previous, window)
        {
            return None;
        }
        records.ranges(previous)
    };
    let start = window.prepaint_index();
    window.reuse_prepaint(prepaint_range.clone());
    debug_assert!(
        window.prepaint_index() == prepaint_range.end.shifted(&prepaint_range.start, &start),
        "a reused prepaint range changed length"
    );

    let source = &window.rendered_frame.retained.elements;
    let subtree = source
        .subtree(previous.root)
        .expect("a record found is painted");
    let first = previous.index as usize;
    let last = first + subtree.records[first].nested as usize;
    window
        .layout_engine
        .as_mut()
        .unwrap()
        .retention
        .stats
        .elements_reused += (last - first + 1) as u64;
    window.next_frame.retained.reused_any = true;
    let target = &mut window.next_frame.retained.elements;

    if target.open.is_empty() {
        // Drawn again as a root: its subtree is taken over as it is, or,
        // when it was nested in another last frame, cut out of that one's.
        let subtree = if first == 0 {
            subtree.clone()
        } else {
            let from_prepaint = subtree.records[first].prepaint_range.start.clone();
            let from_paint = subtree.records[first].paint_range.start.clone();
            let zero_prepaint = PrepaintStateIndex::default();
            let zero_paint = PaintIndex::default();
            Rc::new(Subtree {
                records: subtree.records[first..=last]
                    .iter()
                    .map(|record| ElementRecord {
                        prepaint_range: record
                            .prepaint_range
                            .start
                            .shifted(&from_prepaint, &zero_prepaint)
                            ..record
                                .prepaint_range
                                .end
                                .shifted(&from_prepaint, &zero_prepaint),
                        paint_range: record.paint_range.start.shifted(&from_paint, &zero_paint)
                            ..record.paint_range.end.shifted(&from_paint, &zero_paint),
                        ..record.clone()
                    })
                    .collect(),
                by_key: OnceCell::new(),
            })
        };
        let root = target.push_root(Root {
            key: kept.key,
            records: RootRecords::Frozen(subtree),
            placement: None,
            prepaint_start: start,
            paint_start: paint_range.start,
            paint: Paint::Pending,
            usable: true,
        });
        return Some(Phase::ReusedRoot(root));
    }

    // Drawn again inside an element built this frame: its records are copied
    // into that one's, placed in this frame's prepaint, and in last frame's
    // paint until it is painted.
    let index = target.building.len() as u32;
    let root = &source.roots[previous.root as usize];
    let zero_paint = PaintIndex::default();
    let from_prepaint = subtree.records[first].prepaint_range.start.clone();
    target
        .building
        .extend(subtree.records[first..=last].iter().map(|record| {
            ElementRecord {
                prepaint_range: record.prepaint_range.start.shifted(&from_prepaint, &start)
                    ..record.prepaint_range.end.shifted(&from_prepaint, &start),
                paint_range: record
                    .paint_range
                    .start
                    .shifted(&zero_paint, &root.paint_start)
                    ..record
                        .paint_range
                        .end
                        .shifted(&zero_paint, &root.paint_start),
                paint: match record.paint {
                    Paint::Painted => Paint::Pending,
                    paint => paint,
                },
                ..record.clone()
            }
        }));
    Some(Phase::ReusedNested {
        root: target.building_root,
        index,
    })
}

/// Whether a layout node `previous` or a record nested in it holds was
/// measured again this frame. A text laid out again may break into other
/// lines though its size comes out the same, so it is drawn again only while
/// its measurement stands as it was.
fn remeasured(records: &ElementRecords, previous: PrevRef, window: &Window) -> bool {
    let measured = &window.layout_engine.as_ref().unwrap().retention.measured;
    if measured.is_empty() {
        return false;
    }
    let subtree = records
        .subtree(previous.root)
        .expect("a record found is painted");
    let first = previous.index as usize;
    let last = first + subtree.records[first].nested as usize;
    subtree.records[first..=last]
        .iter()
        .any(|record| measured.contains(&record.layout_id))
}

/// Paints `drawable` as [`Drawable::paint`] does, recording what it paints,
/// or draws it again from last frame when its prepaint was.
#[inline(always)]
pub(crate) fn paint<E: Element>(drawable: &mut Drawable<E>, window: &mut Window, cx: &mut App) {
    if !retainable_type::<E>() {
        drawable.paint(window, cx);
        return;
    }
    match drawable.fast_retention.phase {
        Phase::Recorded { root, index } => {
            let start = window.paint_index();
            drawable.paint(window, cx);
            let end = window.paint_index();
            let elements = &mut window.next_frame.retained.elements;
            let record = &mut elements.pending(root)[index as usize];
            record.paint_range = start..end;
            record.paint = Paint::Painted;
            // Painted, the element needs its style no more, which its record
            // takes over.
            if let Snapshot::Div {
                style: style @ None,
                ..
            } = &mut record.snapshot
                && let Some(Own::Div(div)) = own(&mut drawable.element)
            {
                *style = Some(Rc::new(mem::take(&mut *div.interactivity.base_style)));
            }
            if index == 0 {
                elements.freeze(root);
            }
        }
        Phase::ReusedRoot(root) => reuse_root_paint(root, window),
        Phase::ReusedNested { root, index } => reuse_nested_paint(root, index, window),
        _ => {
            drawable.paint(window, cx);
        }
    }
}

/// Draws the root whose prepaint [`reuse_prepaint`] drew again as far as its
/// paint goes.
fn reuse_root_paint(root: u32, window: &mut Window) {
    let source = {
        let root = &window.next_frame.retained.elements.roots[root as usize];
        let RootRecords::Frozen(subtree) = &root.records else {
            panic!("a root drawn again has a subtree");
        };
        let range = &subtree.records[0].paint_range;
        let zero = PaintIndex::default();
        range.start.shifted(&zero, &root.paint_start)..range.end.shifted(&zero, &root.paint_start)
    };
    let start = window.paint_index();
    window.reuse_paint(source.clone());
    debug_assert!(
        window.paint_index() == source.end.shifted(&source.start, &start),
        "a reused paint range changed length"
    );
    let root = &mut window.next_frame.retained.elements.roots[root as usize];
    root.paint_start = start;
    root.paint = Paint::Painted;
}

/// Draws the records starting at `anchor` of the root `root`, whose
/// prepaint [`reuse_prepaint`] drew again, as far as their paint goes.
fn reuse_nested_paint(root: u32, anchor: u32, window: &mut Window) {
    let anchor = anchor as usize;
    let (source, nested) = {
        let record = &window.next_frame.retained.elements.pending(root)[anchor];
        (record.paint_range.clone(), record.nested as usize)
    };
    let start = window.paint_index();
    window.reuse_paint(source.clone());
    debug_assert!(
        window.paint_index() == source.end.shifted(&source.start, &start),
        "a reused paint range changed length"
    );
    let records = window.next_frame.retained.elements.pending(root);
    for record in &mut records[anchor..=anchor + nested] {
        if record.paint == Paint::Pending {
            record.paint_range = record.paint_range.start.shifted(&source.start, &start)
                ..record.paint_range.end.shifted(&source.start, &start);
            record.paint = Paint::Painted;
        }
    }
}
