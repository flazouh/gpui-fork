use super::{ViewElementState, ViewPrepaint};
use crate::{
    AnyView, AnyWeakView, App, Bounds, ContentMask, EntityId, FrameIndex, GlobalElementId,
    GroupHitboxes, PaintIndex, Pixels, PrepaintStateIndex, Scene, TextStyleRefinement, Window,
    key_dispatch::DispatchNodeId, view,
};
use std::{mem, ops::Range, sync::Arc};

/// The cached views a cached view drew, in the order it drew them. A replay carries the view's own output over in
/// the runs between them and draws each of them in place.
pub(super) enum Slots {
    /// The view drew a view that a replay could not draw again in place, or its paint did not draw the views its
    /// prepaint did.
    Unknown,
    Prepainted(Vec<SlotPrepaint>),
    Painted(Vec<Slot>),
}

impl Slots {
    pub(super) fn painted(self, paints: Option<Vec<SlotPaint>>) -> Self {
        let (Slots::Prepainted(prepaints), Some(paints)) = (self, paints) else {
            return Slots::Unknown;
        };
        let same_views = prepaints.len() == paints.len()
            && prepaints
                .iter()
                .zip(&paints)
                .all(|(prepaint, paint)| prepaint.entity_id == paint.entity_id);
        if !same_views {
            return Slots::Unknown;
        }
        Slots::Painted(
            prepaints
                .into_iter()
                .zip(paints)
                .map(|(prepaint, paint)| Slot { prepaint, paint })
                .collect(),
        )
    }
}

pub(super) struct Slot {
    prepaint: SlotPrepaint,
    paint: SlotPaint,
}

pub(super) struct SlotPrepaint {
    entity_id: EntityId,
    global_id: GlobalElementId,
    render_handle: AnyWeakView,
    bounds: Bounds<Pixels>,
    context: SlotContext,
    /// Counted from the start of the outer view's output, and from before the view takes its own state: the outer
    /// view, carrying its output over around the slot, leaves out the state access that the view makes again.
    range: Range<PrepaintStateIndex>,
}

pub(super) struct SlotPaint {
    entity_id: EntityId,
    context: SlotContext,
    range: Range<PaintIndex>,
}

/// What the elements around a cached view set for it, which a replay sets again in their place.
struct SlotContext {
    content_mask: ContentMask<Pixels>,
    text_style_stack: Vec<TextStyleRefinement>,
    rem_size: Pixels,
    opacity: f32,
}

impl SlotContext {
    fn capture(window: &Window) -> Self {
        Self {
            content_mask: window.content_mask(),
            text_style_stack: window.text_style_stack.clone(),
            rem_size: window.rem_size(),
            opacity: window.element_opacity,
        }
    }

    fn enter<R>(
        &self,
        global_id: &GlobalElementId,
        window: &mut Window,
        f: impl FnOnce(&mut Window) -> R,
    ) -> R {
        let element_ids = mem::replace(
            &mut window.element_id_stack,
            global_id.0.iter().cloned().collect(),
        );
        let text_styles = mem::replace(&mut window.text_style_stack, self.text_style_stack.clone());
        let opacity = mem::replace(&mut window.element_opacity, self.opacity);
        window.content_mask_stack.push(self.content_mask);
        let result = window.with_rem_size(Some(self.rem_size), f);
        window.content_mask_stack.pop();
        window.element_opacity = opacity;
        window.text_style_stack = text_styles;
        window.element_id_stack = element_ids;
        result
    }
}

/// A cached view that is rendering or replaying its prepaint. The cached views it draws count their ranges from its
/// start and are recorded as its slots.
pub(crate) struct PrepaintFrame {
    /// `None` under a view that renders, where nothing is carried over.
    carried_from: Option<CarriedFrom>,
    new_start: PrepaintStateIndex,
    image_caches: usize,
    /// `None` once a view was drawn that a replay could not draw again in place.
    slots: Option<Vec<SlotPrepaint>>,
}

impl PrepaintFrame {
    pub(super) fn rendering(new_start: PrepaintStateIndex, window: &Window) -> Self {
        Self {
            carried_from: None,
            new_start,
            image_caches: window.image_cache_stack.len(),
            slots: Some(Vec::new()),
        }
    }

    fn replaying(plan: &ReplayPlan, new_start: PrepaintStateIndex, window: &Window) -> Self {
        Self {
            carried_from: Some(CarriedFrom {
                prepaint_start: plan.prepaint.start.clone(),
                paint_start: plan.paint.start.clone(),
                drawn_in: plan.slots_drawn_in,
            }),
            ..Self::rendering(new_start, window)
        }
    }

    /// Runs `prepaint` with this view as the innermost one, and returns the slots it recorded.
    pub(super) fn enter<R>(
        self,
        window: &mut Window,
        prepaint: impl FnOnce(&mut Window) -> R,
    ) -> (R, Slots) {
        window.cached_view_prepaints.push(self);
        let result = prepaint(window);
        let slots = window
            .cached_view_prepaints
            .pop()
            .and_then(|frame| frame.slots)
            .map_or(Slots::Unknown, Slots::Prepainted);
        (result, slots)
    }

    pub(crate) fn recorded_views(&self) -> usize {
        self.slots.as_ref().map_or(0, Vec::len)
    }

    pub(crate) fn forget_views_after(&mut self, count: usize) {
        if let Some(slots) = &mut self.slots {
            slots.truncate(count);
        }
    }
}

/// Where the output a view is carried over from sits in the frame before, and how old it is.
#[derive(Clone)]
pub(super) struct CarriedFrom {
    prepaint_start: PrepaintStateIndex,
    paint_start: PaintIndex,
    /// The frame before, or for a slot the frame its view last drew its slots in, which has been carried over
    /// whole since.
    pub(super) drawn_in: u64,
}

impl CarriedFrom {
    /// `None` under a cached view that renders.
    pub(super) fn current(window: &Window) -> Option<Self> {
        match window.cached_view_prepaints.last() {
            None => Some(Self {
                prepaint_start: PrepaintStateIndex::default(),
                paint_start: PaintIndex::default(),
                drawn_in: window.frame_number.saturating_sub(1),
            }),
            Some(outer) => outer.carried_from.clone(),
        }
    }

    pub(super) fn prepaint_range(
        &self,
        range: &Range<PrepaintStateIndex>,
    ) -> Range<PrepaintStateIndex> {
        offset(range, &self.prepaint_start)
    }

    pub(super) fn paint_range(&self, range: &Range<PaintIndex>) -> Range<PaintIndex> {
        offset(range, &self.paint_start)
    }
}

pub(crate) struct PaintFrame {
    new_start: PaintIndex,
    image_caches: usize,
    group_depth: usize,
    hover_groups: usize,
    /// `None` once a view was drawn that a replay could not draw again in place.
    paints: Option<Vec<SlotPaint>>,
}

impl PaintFrame {
    pub(super) fn new(new_start: PaintIndex, window: &Window, cx: &mut App) -> Self {
        Self {
            new_start,
            image_caches: window.image_cache_stack.len(),
            group_depth: window.group_depth,
            hover_groups: GroupHitboxes::active(cx),
            paints: Some(Vec::new()),
        }
    }

    /// Runs `paint` with this view as the innermost one, and returns what its slots painted.
    pub(super) fn enter<R>(
        self,
        window: &mut Window,
        paint: impl FnOnce(&mut Window) -> R,
    ) -> (R, Option<Vec<SlotPaint>>) {
        window.cached_view_paints.push(self);
        let result = paint(window);
        let paints = window
            .cached_view_paints
            .pop()
            .and_then(|frame| frame.paints);
        (result, paints)
    }

    /// Whether a view painted now sits in nothing a replay would not set again: an image cache, a group painted
    /// into a scene of its own, or a hover group whose hitbox the view may read.
    fn restores(&self, window: &Window, cx: &mut App) -> bool {
        self.image_caches == window.image_cache_stack.len()
            && self.group_depth == window.group_depth
            && self.hover_groups == GroupHitboxes::active(cx)
    }
}

/// Records the view that prepainted `range` as a slot of the cached view that draws it, if one does. Returns the
/// start the view's ranges count from.
pub(super) fn record_prepaint(
    entity_id: EntityId,
    global_id: &GlobalElementId,
    render_handle: Option<&AnyWeakView>,
    bounds: Bounds<Pixels>,
    range: Range<PrepaintStateIndex>,
    window: &mut Window,
) -> PrepaintStateIndex {
    let Some(outer) = window.cached_view_prepaints.last() else {
        return PrepaintStateIndex::default();
    };
    let base = outer.new_start.clone();
    let slot = render_handle
        .filter(|_| outer.image_caches == window.image_cache_stack.len())
        .zip(relative(range, &base))
        .map(|(render_handle, range)| SlotPrepaint {
            entity_id,
            global_id: global_id.clone(),
            render_handle: render_handle.clone(),
            bounds,
            context: SlotContext::capture(window),
            range,
        });
    if let Some(outer) = window.cached_view_prepaints.last_mut() {
        match (&mut outer.slots, slot) {
            (Some(slots), Some(slot)) => slots.push(slot),
            _ => outer.slots = None,
        }
    }
    base
}

pub(super) fn record_paint(
    entity_id: EntityId,
    range: Range<PaintIndex>,
    window: &mut Window,
    cx: &mut App,
) -> PaintIndex {
    let Some(outer) = window.cached_view_paints.last() else {
        return PaintIndex::default();
    };
    let base = outer.new_start.clone();
    let paint = outer
        .restores(window, cx)
        .then(|| relative(range, &base))
        .flatten()
        .map(|range| SlotPaint {
            entity_id,
            context: SlotContext::capture(window),
            range,
        });
    if let Some(outer) = window.cached_view_paints.last_mut() {
        match (&mut outer.paints, paint) {
            (Some(paints), Some(paint)) => paints.push(paint),
            _ => outer.paints = None,
        }
    }
    base
}

fn offset<I: FrameIndex>(range: &Range<I>, base: &I) -> Range<I> {
    range.start.offset_by(base)..range.end.offset_by(base)
}

pub(super) fn relative<I: FrameIndex>(range: Range<I>, base: &I) -> Option<Range<I>> {
    Some(range.start.relative_to(base)?..range.end.relative_to(base)?)
}

/// A replay that is known to work, before it changes the frame.
pub(super) struct ReplayPlan {
    prepaint: Range<PrepaintStateIndex>,
    paint: Range<PaintIndex>,
    slots_drawn_in: u64,
    scene: Arc<Scene>,
    /// Each slot with its view, held so that it lives until the slot is drawn.
    children: Vec<(Slot, AnyView)>,
}

/// A replay whose prepaint is done, for paint to finish.
pub(super) struct Replay {
    paint: Range<PaintIndex>,
    /// The view's scene of the frame before. Its operations between the slots go on the new one.
    scene: Arc<Scene>,
    children: Vec<ReplayedChild>,
}

struct ReplayedChild {
    slot: Slot,
    prepaint: ViewPrepaint,
    /// The dispatch node the view prepainted under, and paints under.
    node: DispatchNodeId,
}

impl ViewElementState {
    /// The replay of a view that a view inside it made dirty, if the frame before has all a replay takes from it
    /// and no notified entity would change what the view itself drew.
    pub(super) fn replay_plan(
        &mut self,
        entity_id: EntityId,
        prepaint: Range<PrepaintStateIndex>,
        paint: Range<PaintIndex>,
        window: &Window,
    ) -> Option<ReplayPlan> {
        let Slots::Painted(slots) = &self.slots else {
            return None;
        };
        let scene = self.retained.scene.clone()?;
        let notified = &window.self_dirty_views;
        if notified.contains(&entity_id)
            || notified
                .iter()
                .any(|entity| self.own_reads.contains(entity))
            || window.a11y.is_active()
            || window.prepaint_range_has_deferred_draws(&prepaint)
            || !notified_views_are_in_slots(entity_id, slots, window)
            || !slots_are_in_order(slots, &prepaint, &paint, &scene, window)
        {
            return None;
        }
        let views = slots
            .iter()
            .map(|slot| slot.prepaint.render_handle.upgrade())
            .collect::<Option<Vec<_>>>()?;
        let Slots::Painted(slots) = mem::replace(&mut self.slots, Slots::Unknown) else {
            return None;
        };
        Some(ReplayPlan {
            prepaint,
            paint,
            slots_drawn_in: self.drew_slots_in,
            scene,
            children: slots.into_iter().zip(views).collect(),
        })
    }
}

/// Whether every notified view inside the view `entity_id` is inside one of its slots, which renders it. A notified
/// view between the two belongs to the output the view itself carries over, which would not show the change.
fn notified_views_are_in_slots(entity_id: EntityId, slots: &[Slot], window: &Window) -> bool {
    window.self_dirty_views.iter().all(|&notified| {
        let mut below = None;
        for view in window
            .rendered_frame
            .dispatch_tree
            .view_path_reversed(notified)
        {
            if view == entity_id {
                return below.is_some_and(|below| {
                    slots.iter().any(|slot| slot.prepaint.entity_id == below)
                });
            }
            below = Some(view);
        }
        true
    })
}

/// Whether the frame before holds the view's output as the slots describe it: each slot after the one before, all
/// inside the view's ranges and its scene. A replay reads these ranges without another check.
fn slots_are_in_order(
    slots: &[Slot],
    prepaint: &Range<PrepaintStateIndex>,
    paint: &Range<PaintIndex>,
    scene: &Scene,
    window: &Window,
) -> bool {
    let mut prepaint_run_start = prepaint.start.clone();
    let mut paint_run_start = paint.start.clone();
    let mut scene_run_start = 0;
    for slot in slots {
        let slot_prepaint = offset(&slot.prepaint.range, &prepaint.start);
        let slot_paint = offset(&slot.paint.range, &paint.start);
        let scene_range = slot_paint.start.scene_index()..slot_paint.end.scene_index();
        if !window.rendered_frame_holds_prepaint(&(prepaint_run_start..slot_prepaint.start.clone()))
            || !window.rendered_frame_holds_prepaint(&slot_prepaint)
            || !window.rendered_frame_holds_paint(&(paint_run_start..slot_paint.start.clone()))
            || !window.rendered_frame_holds_paint(&slot_paint)
            || scene_run_start > scene_range.start
            || scene_range.start > scene_range.end
        {
            return false;
        }
        prepaint_run_start = slot_prepaint.end;
        paint_run_start = slot_paint.end;
        scene_run_start = scene_range.end;
    }
    window.rendered_frame_holds_prepaint(&(prepaint_run_start..prepaint.end.clone()))
        && window.rendered_frame_holds_paint(&(paint_run_start..paint.end.clone()))
        && scene_run_start <= scene.len()
}

/// Prepaints a cached view again from what it prepainted in the frame before, but for its slots: each of those
/// prepaints as it would in place, in the dispatch node it went under and the context it had. `None` when the frame
/// before does not match the slots, with the frame left for [`Window::transact`] to take back.
pub(super) fn replay_prepaint(
    plan: ReplayPlan,
    new_start: PrepaintStateIndex,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Replay, Slots)> {
    let frame = PrepaintFrame::replaying(&plan, new_start, window);
    let (children, slots) = frame.enter(window, |window| {
        let mut open_nodes = Vec::new();
        let mut children = Vec::with_capacity(plan.children.len());
        // `None` once a run did not end as the slots say.
        let mut next_run_start = Some(plan.prepaint.start.clone());
        for (slot, view) in plan.children {
            let range = offset(&slot.prepaint.range, &plan.prepaint.start);
            // The run before a cached view ends with the dispatch node of the view's own element.
            let node = next_run_start.take().and_then(|run_start| {
                window.reuse_prepaint_open(run_start..range.start, &mut open_nodes)
            });
            let Some(node) = node else {
                break;
            };
            let prepaint =
                slot.prepaint
                    .context
                    .enter(&slot.prepaint.global_id, window, |window| {
                        view::prepaint_view(
                            slot.prepaint.entity_id,
                            Some(&slot.prepaint.global_id),
                            slot.prepaint.bounds,
                            &mut None,
                            window,
                            cx,
                            &mut |window, cx| {
                                (view.render_element(window, cx), Some(view.downgrade()))
                            },
                        )
                    });
            children.push(ReplayedChild {
                slot,
                prepaint,
                node,
            });
            next_run_start = Some(range.end);
        }
        let complete = next_run_start.is_some();
        if let Some(run_start) = next_run_start {
            window.reuse_prepaint_open(run_start..plan.prepaint.end, &mut open_nodes);
        }
        window.close_reused_nodes(&mut open_nodes);
        complete.then_some(children)
    });
    let replay = Replay {
        paint: plan.paint,
        scene: plan.scene,
        children: children?,
    };
    Some((replay, slots))
}

/// Paints a cached view again from what it painted in the frame before, but for its slots, which paint in place.
/// The view's scene is built again from the operations of the one before, with the chunk each slot puts on it.
pub(super) fn replay_paint(replay: &mut Replay, window: &mut Window, cx: &mut App) {
    let mut next_run_start = replay.paint.start.clone();
    let mut next_scene_run_start = 0;
    for child in &mut replay.children {
        let range = offset(&child.slot.paint.range, &replay.paint.start);
        let scene_range = range.start.scene_index()..range.end.scene_index();
        window.reuse_paint_except_scene(next_run_start..range.start);
        window
            .next_frame
            .scene
            .replay(next_scene_run_start..scene_range.start, &replay.scene);
        window.next_frame.dispatch_tree.set_active_node(child.node);
        let slot = &child.slot;
        slot.paint
            .context
            .enter(&slot.prepaint.global_id, window, |window| {
                view::paint_view(
                    slot.prepaint.entity_id,
                    true,
                    Some(&slot.prepaint.global_id),
                    &mut child.prepaint,
                    window,
                    cx,
                );
            });
        next_run_start = range.end;
        next_scene_run_start = scene_range.end;
    }
    window.reuse_paint_except_scene(next_run_start..replay.paint.end.clone());
    window
        .next_frame
        .scene
        .replay(next_scene_run_start..replay.scene.len(), &replay.scene);
}
