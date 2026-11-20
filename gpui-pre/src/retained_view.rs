//! Drawing of cached views ([`crate::Entity::cached`], [`crate::AnyView::cached`]).
//!
//! A cached view paints into a scene of its own, which goes on the scene of whatever draws the view as one
//! [`RetainedChunk`]. Each frame the view is drawn in one of three ways:
//!
//! - Reused: neither the view nor a view inside it was notified. Its prepaint and paint output is carried over from
//!   the frame before, and its chunk goes on the scene again.
//! - Replayed: only cached views inside it were notified. The view does not render. Its own output is carried over
//!   in runs between the cached views it drew (its slots), and each of those is drawn in place. See [`replay`].
//! - Rendered: anything else. Every view inside it renders too, as under [`Window::refresh`], because a view that
//!   renders may change a view inside it without notifying it.

mod replay;
#[cfg(test)]
mod tests;

use self::replay::{
    CarriedFrom, Replay, ReplayPlan, Slots, record_paint, record_prepaint, relative_paint,
    relative_prepaint, replay_paint, replay_prepaint,
};
pub(crate) use self::replay::{PaintFrame, PrepaintFrame};
use crate::{
    AnyElement, AnyWeakView, App, AvailableSpace, Bounds, ContentMask, EntityId, GlobalElementId,
    PaintIndex, Pixels, PrepaintStateIndex, RetainedChunk, Scene, Size, TextStyle, Window,
};
use collections::FxHashSet;
use std::{
    mem,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// What prepainting a view left for its paint.
#[doc(hidden)]
pub struct ViewPrepaint(Outcome);

enum Outcome {
    Rendered(AnyElement),
    Reused { paint: Range<PaintIndex> },
    Replayed(Replay),
}

impl ViewPrepaint {
    pub(crate) fn rendered(element: AnyElement) -> Self {
        Self(Outcome::Rendered(element))
    }

    pub(crate) fn rendered_element(&mut self) -> Option<&mut AnyElement> {
        match &mut self.0 {
            Outcome::Rendered(element) => Some(element),
            Outcome::Reused { .. } | Outcome::Replayed(_) => None,
        }
    }
}

struct ViewElementState {
    /// Where the view's prepaint output sits, counted from the start of the output of the cached view that draws
    /// it (from the start of the frame for a view in none). Counted so, it stays right while that view is carried
    /// over to another place in the frame.
    prepaint_range: Range<PrepaintStateIndex>,
    /// Like `prepaint_range`, for paint. The scene index counts in the scene of the cached view that draws it.
    paint_range: Range<PaintIndex>,
    /// The frame `prepaint_range` is of. The view is not prepainted in a frame that carries it over inside a
    /// cached view, or inside a deferred draw: the range can then be older than the frame before. It is of the
    /// frame being drawn in a second prepaint, after [`Window::transact`] threw the first away.
    prepainted_in: u64,
    /// The frame `paint_range` and the retained scene are of. A view is prepainted without being painted while it
    /// is hidden: nothing is then carried over, because the scene may be older than what the view rendered last.
    painted_in: u64,
    /// The frame the view last rendered or replayed in, which is the frame it last drew its slots in.
    drew_slots_in: u64,
    cache_key: ViewElementCacheKey,
    accessed_entities: FxHashSet<EntityId>,
    /// What the view read while it rendered and painted, outside the cached views inside it. A replay shows none
    /// of a change to these, so a notified one has the view render.
    own_reads: FxHashSet<EntityId>,
    /// Renders the view again, for a cached view that draws it and replays around it.
    render_handle: Option<AnyWeakView>,
    retained: RetainedPaint,
    slots: Slots,
}

#[derive(PartialEq)]
struct ViewElementCacheKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    rem_size: Pixels,
    /// The bits of the opacity the elements around the view give it: a NaN then matches itself.
    opacity: u32,
}

impl ViewElementCacheKey {
    fn current(bounds: Bounds<Pixels>, window: &Window) -> Self {
        Self {
            bounds,
            content_mask: window.content_mask(),
            text_style: window.text_style(),
            rem_size: window.rem_size(),
            opacity: window.element_opacity.to_bits(),
        }
    }
}

/// What a cached view last painted, put on the scene again in each frame the view is clean.
struct RetainedPaint {
    /// One id for the view's whole life at its place in the element tree.
    layer_id: u64,
    generation: u64,
    clean_frames: u32,
    scene: Option<Arc<Scene>>,
    /// The scene before `scene`. The next paint takes over its allocations once no frame holds it.
    spare: Option<Arc<Scene>>,
}

impl RetainedPaint {
    fn new() -> Self {
        static NEXT_LAYER_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            layer_id: NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed),
            generation: 0,
            clean_frames: 0,
            scene: None,
            spare: None,
        }
    }

    fn empty_scene(&mut self) -> Scene {
        match self.spare.take().map(Arc::try_unwrap) {
            Some(Ok(mut scene)) => {
                scene.clear();
                scene
            }
            _ => Scene::default(),
        }
    }

    fn repainted(&mut self, mut scene: Scene) {
        scene.finish();
        self.generation += 1;
        self.clean_frames = 0;
        self.spare = self.scene.replace(Arc::new(scene));
    }

    fn kept_clean(&mut self) {
        self.clean_frames = self.clean_frames.saturating_add(1);
    }

    fn chunk(&self) -> Option<RetainedChunk> {
        let scene = self.scene.as_ref()?;
        Some(RetainedChunk {
            order: 0,
            bounds: scene.covered_bounds()?,
            layer_id: self.layer_id,
            generation: self.generation,
            clean_frames: self.clean_frames,
            scene: scene.clone(),
        })
    }
}

enum Plan {
    Reuse {
        prepaint: Range<PrepaintStateIndex>,
        paint: Range<PaintIndex>,
    },
    Replay(ReplayPlan),
    Render,
}

impl ViewElementState {
    fn new(cache_key: ViewElementCacheKey) -> Self {
        Self {
            prepaint_range: PrepaintStateIndex::default()..PrepaintStateIndex::default(),
            paint_range: PaintIndex::default()..PaintIndex::default(),
            prepainted_in: 0,
            painted_in: 0,
            drew_slots_in: 0,
            cache_key,
            accessed_entities: FxHashSet::default(),
            own_reads: FxHashSet::default(),
            render_handle: None,
            retained: RetainedPaint::new(),
            slots: Slots::Prepainted(None),
        }
    }

    fn plan(
        &mut self,
        entity_id: EntityId,
        cache_key: &ViewElementCacheKey,
        window: &Window,
    ) -> Plan {
        if self.cache_key != *cache_key || window.refreshing {
            return Plan::Render;
        }
        let Some(carried_from) = CarriedFrom::current(window) else {
            return Plan::Render;
        };
        if self.prepainted_in != carried_from.drawn_in || self.painted_in != carried_from.drawn_in {
            return Plan::Render;
        }
        let prepaint = carried_from.prepaint_range(&self.prepaint_range);
        let paint = carried_from.paint_range(&self.paint_range);
        if !window.rendered_frame_holds_prepaint(&prepaint)
            || !window.rendered_frame_holds_paint(&paint)
        {
            return Plan::Render;
        }
        if !window.dirty_views.contains(&entity_id) {
            return Plan::Reuse { prepaint, paint };
        }
        match self.replay_plan(entity_id, prepaint, paint, window) {
            Some(plan) => Plan::Replay(plan),
            None => Plan::Render,
        }
    }

    /// Ends a paint into the view's own scene: keeps that scene, puts back the one the view is drawn on, and pairs
    /// the slots with what the cached views painted in.
    fn finish_paint(&mut self, outer_scene: Scene, window: &mut Window) {
        let scene = mem::replace(&mut window.next_frame.scene, outer_scene);
        self.retained.repainted(scene);
        let paints = window
            .cached_view_paints
            .pop()
            .and_then(PaintFrame::into_paints);
        let slots = mem::replace(&mut self.slots, Slots::Painted(None));
        self.slots = slots.painted(paints);
    }
}

pub(crate) fn prepaint(
    entity_id: EntityId,
    global_id: &GlobalElementId,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
    render: &mut dyn FnMut(&mut Window, &mut App) -> (AnyElement, Option<AnyWeakView>),
) -> ViewPrepaint {
    let slot_start = window.prepaint_index();
    window.with_element_state::<ViewElementState, _>(global_id, |mut state, window| {
        let cache_key = ViewElementCacheKey::current(bounds, window);
        let prepaint_start = window.prepaint_index();
        let plan = state.as_mut().map_or(Plan::Render, |state| {
            state.plan(entity_id, &cache_key, window)
        });
        let carried_over = match (plan, state.as_mut()) {
            (Plan::Reuse { prepaint, paint }, Some(state)) => {
                window.reuse_prepaint(prepaint);
                cx.entities.extend_accessed(&state.accessed_entities);
                Some(Outcome::Reused { paint })
            }
            (Plan::Replay(plan), Some(state)) => {
                let (replayed, accessed_entities) = cx.detect_accessed_entities(|cx| {
                    // A replay that stops half way has put part of the view on the frame: this takes it off.
                    window.transact(|window| {
                        replay_prepaint(plan, prepaint_start.clone(), window, cx).ok_or(())
                    })
                });
                replayed.ok().map(|(replay, slots)| {
                    state.accessed_entities.extend(accessed_entities);
                    cx.entities.extend_accessed(&state.accessed_entities);
                    state.slots = slots;
                    state.drew_slots_in = window.frame_number;
                    Outcome::Replayed(replay)
                })
            }
            _ => None,
        };
        let (outcome, mut state) = match (carried_over, state) {
            (Some(outcome), Some(state)) => (outcome, state),
            (_, previous) => render_view(
                previous,
                cache_key,
                bounds,
                prepaint_start.clone(),
                window,
                cx,
                render,
            ),
        };

        let prepaint_end = window.prepaint_index();
        state.prepainted_in = window.frame_number;
        let base = record_prepaint(
            entity_id,
            global_id,
            state.render_handle.as_ref(),
            bounds,
            slot_start..prepaint_end.clone(),
            window,
        );
        state.prepaint_range = relative_prepaint(prepaint_start..prepaint_end, &base);
        (ViewPrepaint(outcome), state)
    })
}

fn render_view(
    previous: Option<ViewElementState>,
    cache_key: ViewElementCacheKey,
    bounds: Bounds<Pixels>,
    prepaint_start: PrepaintStateIndex,
    window: &mut Window,
    cx: &mut App,
    render: &mut dyn FnMut(&mut Window, &mut App) -> (AnyElement, Option<AnyWeakView>),
) -> (Outcome, ViewElementState) {
    let frame = PrepaintFrame::rendering(prepaint_start, window);
    window.cached_view_prepaints.push(frame);
    let refreshing = mem::replace(&mut window.refreshing, true);
    let (((element, render_handle), own_reads), accessed_entities) =
        cx.detect_accessed_entities(|cx| {
            cx.with_read_scope(|cx| {
                let (mut element, render_handle) = render(window, cx);
                element.layout_as_root(Size::<AvailableSpace>::from(bounds.size), window, cx);
                element.prepaint_at(bounds.origin, window, cx);
                (element, render_handle)
            })
        });
    window.refreshing = refreshing;
    let slots = window
        .cached_view_prepaints
        .pop()
        .map_or(Slots::Prepainted(None), PrepaintFrame::into_slots);

    let mut state = match previous {
        Some(mut state) => {
            state.cache_key = cache_key;
            state
        }
        None => ViewElementState::new(cache_key),
    };
    state.accessed_entities = accessed_entities;
    state.own_reads = own_reads;
    state.render_handle = render_handle;
    state.slots = slots;
    state.drew_slots_in = window.frame_number;
    (Outcome::Rendered(element), state)
}

pub(crate) fn paint(
    entity_id: EntityId,
    global_id: &GlobalElementId,
    prepaint: &mut ViewPrepaint,
    window: &mut Window,
    cx: &mut App,
) {
    let slot_start = window.paint_index();
    window.with_element_state::<ViewElementState, _>(global_id, |state, window| {
        let Some(mut state) = state else {
            // Only a prepaint of this frame leaves a state, so nothing here is retained: the view paints as an
            // uncached one does.
            if let Outcome::Rendered(element) = &mut prepaint.0 {
                element.paint(window, cx);
            }
            let cache_key = ViewElementCacheKey::current(Bounds::default(), window);
            return ((), ViewElementState::new(cache_key));
        };

        let paint_start = window.paint_index();
        match &mut prepaint.0 {
            Outcome::Rendered(element) => {
                let frame = PaintFrame::new(paint_start.clone(), window, cx);
                window.cached_view_paints.push(frame);
                let outer_scene =
                    mem::replace(&mut window.next_frame.scene, state.retained.empty_scene());
                let refreshing = mem::replace(&mut window.refreshing, true);
                let ((), own_reads) = cx.with_read_scope(|cx| element.paint(window, cx));
                window.refreshing = refreshing;
                state.own_reads.extend(own_reads);
                state.finish_paint(outer_scene, window);
            }
            Outcome::Reused { paint } => {
                window.reuse_paint_except_scene(paint.clone());
                state.retained.kept_clean();
            }
            Outcome::Replayed(replay) => {
                let frame = PaintFrame::new(paint_start.clone(), window, cx);
                window.cached_view_paints.push(frame);
                let outer_scene =
                    mem::replace(&mut window.next_frame.scene, state.retained.empty_scene());
                replay_paint(replay, window, cx);
                state.finish_paint(outer_scene, window);
            }
        }
        if let Some(chunk) = state.retained.chunk() {
            window.next_frame.scene.insert_retained_chunk(chunk);
        }

        let paint_end = window.paint_index();
        state.painted_in = window.frame_number;
        let base = record_paint(entity_id, slot_start..paint_end.clone(), window, cx);
        state.paint_range = relative_paint(paint_start..paint_end, &base);
        ((), state)
    })
}
