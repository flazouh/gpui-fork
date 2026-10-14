# Patches

One line for each patch: what it changes, why, and the test that holds it. `atelier-0.3.6` is `upstream-0.3.6` plus the patches below, apart from this file and the README.

| Patch | Why | Test |
|---|---|---|
| Groups: `Window::paint_group(bounds, GroupEffects { opacity, colour_matrix, blur }, f)` paints what `f` paints apart and composites it as one surface (wgpu renderer; Metal draws the content in place with the opacity given to each shape, `Scene::flattened`) | A browser's `opacity` below one and CSS `filter` functions apply to a group as a whole: two overlapping shapes show as one surface, which giving each shape the opacity cannot do | `ace-page`: `record::tests::shapes_that_do_not_overlap_in_a_group_are_exact_and_two_that_overlap_are_counted`, and `tools/pics/groups.html` against Chromium |
