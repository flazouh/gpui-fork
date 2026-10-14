# Patches

One line for each patch: what it changes, why, and the test that holds it. `atelier-0.3.6` is `upstream-0.3.6` plus the patches below, apart from this file and the README.

| Patch | Why | Test |
|---|---|---|
| Groups: `Window::paint_group(bounds, GroupEffects { opacity, colour_matrix, blur }, f)` paints what `f` paints apart and composites it as one surface (wgpu renderer; Metal draws the content in place with the opacity given to each shape, `Scene::flattened`) | A browser's `opacity` below one and CSS `filter` functions apply to a group as a whole: two overlapping shapes show as one surface, which giving each shape the opacity cannot do | `ace-page`: `record::tests::shapes_that_do_not_overlap_in_a_group_are_exact_and_two_that_overlap_are_counted`, and `tools/pics/groups.html` against Chromium |
| Group shadow: `GroupEffects::drop_shadow` paints a shadow of the group's alpha (moved, blurred, in one colour) under the group (wgpu; Metal leaves it out) | CSS `filter: drop-shadow()` | `ace-page`: `record::tests::a_drop_shadow_filter_is_the_groups_shadow`, and `tools/pics/t/mask.html` against Chromium |
| Group mask: `Window::paint_group_masked` shows a group in proportion to the alpha of a second scene (wgpu, by a pass that multiplies the surface by the mask's alpha; Metal leaves the mask out) | CSS `mask-image`: Blitz draws it as a `DestIn` layer | `ace-page`: `record::tests::a_layer_that_keeps_the_group_where_it_is_opaque_masks_it`, and `tools/pics/t/mask.html` |
| Gradient fill: `Window::paint_gradient_fill(bounds, radii, GradientFill)` paints a radial or sweep gradient of up to 16 stops, with pad, repeat or reflect, in a pass of its own (wgpu; a group with no scene; Metal leaves it out) | CSS `radial-gradient()`, `conic-gradient()` and their repeating forms: GPUI's own gradient has two stops and is linear | `ace-page`: `record::tests::a_radial_or_sweep_gradient_over_a_box_is_the_renderers_own_fill`, and `tools/pics/t/grad.html` against Chromium |
