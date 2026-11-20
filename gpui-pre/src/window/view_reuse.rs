use super::{PaintIndex, PrepaintStateIndex, Window};
use crate::key_dispatch::DispatchNodeId;
use std::ops::Range;

/// An index into what a frame holds. A cached view counts its indices from the start of the cached view that draws
/// it, so that they hold while that view is carried over to another place in the frame.
pub(crate) trait FrameIndex: Sized {
    fn offset_by(&self, base: &Self) -> Self;

    /// `None` when `base` is past this index: the two are then not of one frame as it stands, and nothing can be
    /// carried over from the range.
    fn relative_to(&self, base: &Self) -> Option<Self>;
}

/// Implements [`FrameIndex`] for a struct: `counts` are its `usize` fields, `indices` its fields that are a
/// [`FrameIndex`], and `kept` its fields that do not count from the start of the frame.
macro_rules! impl_frame_index {
    ($index:ty {
        counts: [$($(#[$attribute:meta])? $count:ident),* $(,)?],
        indices: [$($nested:ident),* $(,)?],
        kept: [$($kept:ident),* $(,)?] $(,)?
    }) => {
        impl $crate::window::FrameIndex for $index {
            fn offset_by(&self, base: &Self) -> Self {
                Self {
                    $($(#[$attribute])? $count: base.$count + self.$count,)*
                    $($nested: $crate::window::FrameIndex::offset_by(&self.$nested, &base.$nested),)*
                    $($kept: self.$kept,)*
                }
            }

            fn relative_to(&self, base: &Self) -> Option<Self> {
                Some(Self {
                    $($(#[$attribute])? $count: self.$count.checked_sub(base.$count)?,)*
                    $($nested: $crate::window::FrameIndex::relative_to(&self.$nested, &base.$nested)?,)*
                    $($kept: self.$kept,)*
                })
            }
        }
    };
}
pub(crate) use impl_frame_index;

impl_frame_index!(PrepaintStateIndex {
    counts: [
        hitboxes_index,
        tooltips_index,
        deferred_draws_index,
        dispatch_tree_index,
        accessed_element_states_index,
    ],
    indices: [line_layout_index],
    kept: [],
});

// The scene index counts in the scene of the cached view that draws the index's owner, which does not move with
// that view.
impl_frame_index!(PaintIndex {
    counts: [
        #[cfg(any(test, feature = "test-support"))]
        debug_bounds_index,
        mouse_listeners_index,
        input_handlers_index,
        cursor_styles_index,
        accessed_element_states_index,
        tab_handle_index,
    ],
    indices: [line_layout_index],
    kept: [scene_index],
});

impl PaintIndex {
    pub(crate) fn scene_index(&self) -> usize {
        self.scene_index
    }
}

fn within(start: usize, end: usize, len: usize) -> bool {
    start <= end && end <= len
}

impl Window {
    /// Whether carrying `range` over reads nothing out of range.
    pub(crate) fn rendered_frame_holds_prepaint(&self, range: &Range<PrepaintStateIndex>) -> bool {
        let (start, end) = (&range.start, &range.end);
        let frame = &self.rendered_frame;
        within(
            start.hitboxes_index,
            end.hitboxes_index,
            frame.hitboxes.len(),
        ) && within(
            start.tooltips_index,
            end.tooltips_index,
            frame.tooltip_requests.len(),
        ) && within(
            start.deferred_draws_index,
            end.deferred_draws_index,
            frame.deferred_draws.len(),
        ) && within(
            start.dispatch_tree_index,
            end.dispatch_tree_index,
            frame.dispatch_tree.len(),
        ) && within(
            start.accessed_element_states_index,
            end.accessed_element_states_index,
            frame.accessed_element_states.len(),
        ) && self
            .text_system
            .holds_layouts(&(start.line_layout_index.clone()..end.line_layout_index.clone()))
    }

    /// Like [`Window::rendered_frame_holds_prepaint`]. The scene index is left out: it counts in a cached view's
    /// own scene, which the frame does not hold.
    pub(crate) fn rendered_frame_holds_paint(&self, range: &Range<PaintIndex>) -> bool {
        let (start, end) = (&range.start, &range.end);
        let frame = &self.rendered_frame;
        #[cfg(any(test, feature = "test-support"))]
        if !within(
            start.debug_bounds_index,
            end.debug_bounds_index,
            frame.debug_bounds_records.len(),
        ) {
            return false;
        }
        within(
            start.mouse_listeners_index,
            end.mouse_listeners_index,
            frame.mouse_listeners.len(),
        ) && within(
            start.input_handlers_index,
            end.input_handlers_index,
            frame.input_handlers.len(),
        ) && within(
            start.cursor_styles_index,
            end.cursor_styles_index,
            frame.cursor_styles.len(),
        ) && within(
            start.accessed_element_states_index,
            end.accessed_element_states_index,
            frame.accessed_element_states.len(),
        ) && within(
            start.tab_handle_index,
            end.tab_handle_index,
            frame.tab_stops.insertion_history.len(),
        ) && self
            .text_system
            .holds_layouts(&(start.line_layout_index.clone()..end.line_layout_index.clone()))
    }

    pub(crate) fn prepaint_range_has_deferred_draws(
        &self,
        range: &Range<PrepaintStateIndex>,
    ) -> bool {
        range.start.deferred_draws_index != range.end.deferred_draws_index
    }

    pub(super) fn reuse_prepaint_buffers(&mut self, range: &Range<PrepaintStateIndex>) {
        self.next_frame.hitboxes.extend(
            self.rendered_frame.hitboxes[range.start.hitboxes_index..range.end.hitboxes_index]
                .iter()
                .cloned(),
        );
        self.next_frame.tooltip_requests.extend(
            self.rendered_frame.tooltip_requests
                [range.start.tooltips_index..range.end.tooltips_index]
                .iter_mut()
                .map(|request| request.take()),
        );
        self.next_frame.accessed_element_states.extend(
            self.rendered_frame.accessed_element_states[range.start.accessed_element_states_index
                ..range.end.accessed_element_states_index]
                .iter()
                .map(|(id, type_id)| (id.clone(), *type_id)),
        );
        self.text_system.reuse_layouts(
            range.start.line_layout_index.clone()..range.end.line_layout_index.clone(),
        );
    }

    /// Like [`Window::reuse_prepaint`] for a run of a cached view's own output between the cached views inside it:
    /// the dispatch nodes of the run stay open on `open_nodes`, so that what the next cached view prepaints goes
    /// under the node it went under. Returns that node, the last of the run, if the run has one. Close the last run
    /// with [`Window::close_reused_nodes`]. The range must hold no deferred draws.
    pub(crate) fn reuse_prepaint_open(
        &mut self,
        range: Range<PrepaintStateIndex>,
        open_nodes: &mut Vec<DispatchNodeId>,
    ) -> Option<DispatchNodeId> {
        self.reuse_prepaint_buffers(&range);

        let nodes = range.start.dispatch_tree_index..range.end.dispatch_tree_index;
        let has_nodes = !nodes.is_empty();
        let contains_focus = self.next_frame.dispatch_tree.reuse_nodes_open(
            nodes,
            &mut self.rendered_frame.dispatch_tree,
            self.focus,
            open_nodes,
        );
        if contains_focus {
            self.next_frame.focus = self.focus;
        }
        if has_nodes {
            self.next_frame.dispatch_tree.active_node_id()
        } else {
            None
        }
    }

    pub(crate) fn close_reused_nodes(&mut self, open_nodes: &mut Vec<DispatchNodeId>) {
        while open_nodes.pop().is_some() {
            self.next_frame.dispatch_tree.pop_node();
        }
    }
}
