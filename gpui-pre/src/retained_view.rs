mod replay;
#[cfg(test)]
mod tests;

use self::replay::{
    CarriedFrom, Replay, ReplayPlan, Slots, record_paint, record_prepaint, relative, replay_paint,
    replay_prepaint,
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
    cache_key: ViewElementCacheKey,
    prepainted: Option<FrameOutput<PrepaintStateIndex>>,
    painted: Option<FrameOutput<PaintIndex>>,
    drew_slots_in: u64,
    accessed_entities: FxHashSet<EntityId>,
    /// What the view read while it rendered and painted, outside the cached views inside it. A replay shows none
    /// of a change to these, so a notified one has the view render.
    own_reads: FxHashSet<EntityId>,
    render_handle: Option<AnyWeakView>,
    retained: RetainedPaint,
    slots: Slots,
}

/// Where a view's output sits in the frame `frame`, counted from the start of the output of the cached view that
/// draws it, or of the frame for a view in none.
///
/// The frame can be older than the one before: a view is not prepainted in a frame that carries it over inside a
/// cached view or a deferred draw, and not painted while it is hidden. A view is carried over only when both its
/// outputs are of the frame it is carried over from. The retained scene is of the frame of the paint output, so a
/// view that rendered while it was hidden never shows the scene of an older render.
struct FrameOutput<I> {
    range: Range<I>,
    frame: u64,
}

#[derive(PartialEq)]
struct ViewElementCacheKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    rem_size: Pixels,
    opacity: OpacityBits,
}

/// An opacity that equals itself when it is NaN, so that a cache key which holds one can match.
#[derive(PartialEq)]
struct OpacityBits(u32);

impl ViewElementCacheKey {
    fn current(bounds: Bounds<Pixels>, window: &Window) -> Self {
        Self {
            bounds,
            content_mask: window.content_mask(),
            text_style: window.text_style(),
            rem_size: window.rem_size(),
            opacity: OpacityBits(window.element_opacity.to_bits()),
        }
    }
}

struct RetainedPaint {
    /// One id for the view's whole life at its place in the element tree.
    layer_id: u64,
    generation: u64,
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
        self.spare = self.scene.replace(Arc::new(scene));
    }

    fn chunk(&self) -> Option<RetainedChunk> {
        let scene = self.scene.as_ref()?;
        Some(RetainedChunk {
            order: 0,
            bounds: scene.covered_bounds()?,
            layer_id: self.layer_id,
            generation: self.generation,
            scene: scene.clone(),
        })
    }
}

enum Plan {
    /// Neither the view nor a view inside it was notified.
    Reuse {
        prepaint: Range<PrepaintStateIndex>,
        paint: Range<PaintIndex>,
    },
    /// Only cached views inside the view were notified.
    Replay(ReplayPlan),
    Render,
}

impl ViewElementState {
    fn new(cache_key: ViewElementCacheKey) -> Self {
        Self {
            cache_key,
            prepainted: None,
            painted: None,
            drew_slots_in: 0,
            accessed_entities: FxHashSet::default(),
            own_reads: FxHashSet::default(),
            render_handle: None,
            retained: RetainedPaint::new(),
            slots: Slots::Unknown,
        }
    }

    fn output_to_carry_over(
        &self,
        cache_key: &ViewElementCacheKey,
        window: &Window,
    ) -> Option<(Range<PrepaintStateIndex>, Range<PaintIndex>)> {
        if self.cache_key != *cache_key || window.refreshing {
            return None;
        }
        let carried_from = CarriedFrom::current(window)?;
        let of_that_frame = |frame: u64| frame == carried_from.drawn_in;
        let prepainted = self.prepainted.as_ref()?;
        let painted = self.painted.as_ref()?;
        if !of_that_frame(prepainted.frame) || !of_that_frame(painted.frame) {
            return None;
        }
        let prepaint = carried_from.prepaint_range(&prepainted.range);
        let paint = carried_from.paint_range(&painted.range);
        (window.rendered_frame_holds_prepaint(&prepaint)
            && window.rendered_frame_holds_paint(&paint))
        .then_some((prepaint, paint))
    }

    fn plan(
        &mut self,
        entity_id: EntityId,
        cache_key: &ViewElementCacheKey,
        window: &Window,
    ) -> Plan {
        let Some((prepaint, paint)) = self.output_to_carry_over(cache_key, window) else {
            return Plan::Render;
        };
        if !window.dirty_views.contains(&entity_id) {
            return Plan::Reuse { prepaint, paint };
        }
        self.replay_plan(entity_id, prepaint, paint, window)
            .map_or(Plan::Render, Plan::Replay)
    }

    /// `None` when the view has to render.
    fn carry_over(
        &mut self,
        entity_id: EntityId,
        cache_key: &ViewElementCacheKey,
        prepaint_start: &PrepaintStateIndex,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Outcome> {
        match self.plan(entity_id, cache_key, window) {
            Plan::Render => None,
            Plan::Reuse { prepaint, paint } => {
                window.reuse_prepaint(prepaint);
                cx.entities.extend_accessed(&self.accessed_entities);
                Some(Outcome::Reused { paint })
            }
            Plan::Replay(plan) => {
                let (replayed, accessed_entities) = cx.detect_accessed_entities(|cx| {
                    // A replay that stops half way has put part of the view on the frame: this takes it off.
                    window.transact(|window| {
                        replay_prepaint(plan, prepaint_start.clone(), window, cx).ok_or(())
                    })
                });
                let (replay, slots) = replayed.ok()?;
                self.accessed_entities.extend(accessed_entities);
                cx.entities.extend_accessed(&self.accessed_entities);
                self.slots = slots;
                self.drew_slots_in = window.frame_number;
                Some(Outcome::Replayed(replay))
            }
        }
    }

    fn paint_into_own_scene<R>(
        &mut self,
        paint_start: PaintIndex,
        window: &mut Window,
        cx: &mut App,
        paint: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        let frame = PaintFrame::new(paint_start, window, cx);
        let outer_scene = mem::replace(&mut window.next_frame.scene, self.retained.empty_scene());
        let (result, paints) = frame.enter(window, |window| paint(window, cx));
        let scene = mem::replace(&mut window.next_frame.scene, outer_scene);
        self.retained.repainted(scene);
        self.slots = mem::replace(&mut self.slots, Slots::Unknown).painted(paints);
        result
    }
}

fn with_refreshing<R>(window: &mut Window, f: impl FnOnce(&mut Window) -> R) -> R {
    let refreshing = mem::replace(&mut window.refreshing, true);
    let result = f(window);
    window.refreshing = refreshing;
    result
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
        let carried_over = state
            .as_mut()
            .and_then(|state| state.carry_over(entity_id, &cache_key, &prepaint_start, window, cx));
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
        let base = record_prepaint(
            entity_id,
            global_id,
            state.render_handle.as_ref(),
            bounds,
            slot_start..prepaint_end.clone(),
            window,
        );
        state.prepainted = relative(prepaint_start..prepaint_end, &base).map(|range| FrameOutput {
            range,
            frame: window.frame_number,
        });
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
    let ((((element, render_handle), own_reads), accessed_entities), slots) =
        frame.enter(window, |window| {
            // Every view inside renders too: a view that renders may change one inside it without a notify.
            with_refreshing(window, |window| {
                cx.detect_accessed_entities(|cx| {
                    cx.with_read_scope(|cx| {
                        let (mut element, render_handle) = render(window, cx);
                        element.layout_as_root(
                            Size::<AvailableSpace>::from(bounds.size),
                            window,
                            cx,
                        );
                        element.prepaint_at(bounds.origin, window, cx);
                        (element, render_handle)
                    })
                })
            })
        });

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
            return ((), paint_uncached(prepaint, window, cx));
        };

        let paint_start = window.paint_index();
        match &mut prepaint.0 {
            Outcome::Rendered(element) => {
                let own_reads =
                    state.paint_into_own_scene(paint_start.clone(), window, cx, |window, cx| {
                        with_refreshing(window, |window| {
                            cx.with_read_scope(|cx| element.paint(window, cx)).1
                        })
                    });
                state.own_reads.extend(own_reads);
            }
            Outcome::Reused { paint } => window.reuse_paint_except_scene(paint.clone()),
            Outcome::Replayed(replay) => {
                state.paint_into_own_scene(paint_start.clone(), window, cx, |window, cx| {
                    replay_paint(replay, window, cx)
                });
            }
        }
        if let Some(chunk) = state.retained.chunk() {
            window.next_frame.scene.insert_retained_chunk(chunk);
        }

        let paint_end = window.paint_index();
        let base = record_paint(entity_id, slot_start..paint_end.clone(), window, cx);
        state.painted = relative(paint_start..paint_end, &base).map(|range| FrameOutput {
            range,
            frame: window.frame_number,
        });
        ((), state)
    })
}

/// Only a prepaint of this frame leaves a state, so a view without one has nothing retained and paints as an
/// uncached view does.
fn paint_uncached(
    prepaint: &mut ViewPrepaint,
    window: &mut Window,
    cx: &mut App,
) -> ViewElementState {
    if let Outcome::Rendered(element) = &mut prepaint.0 {
        element.paint(window, cx);
    }
    ViewElementState::new(ViewElementCacheKey::current(Bounds::default(), window))
}
