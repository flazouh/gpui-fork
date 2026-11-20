//! Carries the output of a cached view over from the frame before when the view sits at another place in this frame,
//! or in runs around the cached views inside it. `crate::retained_view` decides when to.

use super::{PaintIndex, PrepaintStateIndex, Window};
use crate::key_dispatch::DispatchNodeId;
use std::ops::Range;

fn within(start: usize, end: usize, len: usize) -> bool {
    start <= end && end <= len
}

impl PrepaintStateIndex {
    /// This index, which counts from `base`, counted from where `base` counts from.
    pub(crate) fn offset_by(&self, base: &Self) -> Self {
        Self {
            hitboxes_index: base.hitboxes_index + self.hitboxes_index,
            tooltips_index: base.tooltips_index + self.tooltips_index,
            deferred_draws_index: base.deferred_draws_index + self.deferred_draws_index,
            dispatch_tree_index: base.dispatch_tree_index + self.dispatch_tree_index,
            accessed_element_states_index: base.accessed_element_states_index
                + self.accessed_element_states_index,
            line_layout_index: self.line_layout_index.offset_by(&base.line_layout_index),
        }
    }

    /// This index counted from `base`. A count that `base` is past comes out as zero rather than a panic: the
    /// range it then gives is checked by [`Window::rendered_frame_holds_prepaint`] before anything reads it.
    pub(crate) fn relative_to(&self, base: &Self) -> Self {
        Self {
            hitboxes_index: self.hitboxes_index.saturating_sub(base.hitboxes_index),
            tooltips_index: self.tooltips_index.saturating_sub(base.tooltips_index),
            deferred_draws_index: self
                .deferred_draws_index
                .saturating_sub(base.deferred_draws_index),
            dispatch_tree_index: self
                .dispatch_tree_index
                .saturating_sub(base.dispatch_tree_index),
            accessed_element_states_index: self
                .accessed_element_states_index
                .saturating_sub(base.accessed_element_states_index),
            line_layout_index: self.line_layout_index.relative_to(&base.line_layout_index),
        }
    }
}

impl PaintIndex {
    /// Like [`PrepaintStateIndex::offset_by`], but for the scene index: it counts in the scene of the cached view
    /// that draws the index's owner, which does not move with that view.
    pub(crate) fn offset_by(&self, base: &Self) -> Self {
        Self {
            scene_index: self.scene_index,
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds_index: base.debug_bounds_index + self.debug_bounds_index,
            mouse_listeners_index: base.mouse_listeners_index + self.mouse_listeners_index,
            input_handlers_index: base.input_handlers_index + self.input_handlers_index,
            cursor_styles_index: base.cursor_styles_index + self.cursor_styles_index,
            accessed_element_states_index: base.accessed_element_states_index
                + self.accessed_element_states_index,
            tab_handle_index: base.tab_handle_index + self.tab_handle_index,
            line_layout_index: self.line_layout_index.offset_by(&base.line_layout_index),
        }
    }

    /// Like [`PrepaintStateIndex::relative_to`], but for the scene index, as in [`PaintIndex::offset_by`].
    pub(crate) fn relative_to(&self, base: &Self) -> Self {
        Self {
            scene_index: self.scene_index,
            #[cfg(any(test, feature = "test-support"))]
            debug_bounds_index: self
                .debug_bounds_index
                .saturating_sub(base.debug_bounds_index),
            mouse_listeners_index: self
                .mouse_listeners_index
                .saturating_sub(base.mouse_listeners_index),
            input_handlers_index: self
                .input_handlers_index
                .saturating_sub(base.input_handlers_index),
            cursor_styles_index: self
                .cursor_styles_index
                .saturating_sub(base.cursor_styles_index),
            accessed_element_states_index: self
                .accessed_element_states_index
                .saturating_sub(base.accessed_element_states_index),
            tab_handle_index: self.tab_handle_index.saturating_sub(base.tab_handle_index),
            line_layout_index: self.line_layout_index.relative_to(&base.line_layout_index),
        }
    }

    pub(crate) fn scene_index(&self) -> usize {
        self.scene_index
    }
}

impl Window {
    /// Whether the frame before holds everything `range` names, in order, so that carrying it over reads nothing
    /// out of range.
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

    /// Carries over what a prepaint range holds outside the dispatch tree and the deferred draws.
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

    /// Closes the dispatch nodes [`Window::reuse_prepaint_open`] left open.
    pub(crate) fn close_reused_nodes(&mut self, open_nodes: &mut Vec<DispatchNodeId>) {
        while open_nodes.pop().is_some() {
            self.next_frame.dispatch_tree.pop_node();
        }
    }
}
