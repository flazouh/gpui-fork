use super::*;
use crate::metal_renderer::InstanceBufferPool;
use gpui::{BorderStyle, Edges, Hsla, hsla};
use image::RgbaImage;
use parking_lot::Mutex;
use std::sync::Arc;

const VIEWPORT: Size<DevicePixels> = Size {
    width: DevicePixels(400),
    height: DevicePixels(300),
};

fn renderer() -> MetalRenderer {
    MetalRenderer::new_headless(Arc::new(Mutex::new(InstanceBufferPool::default())))
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(x), ScaledPixels(y)),
        size: size(ScaledPixels(width), ScaledPixels(height)),
    }
}

fn quad(bounds: Bounds<ScaledPixels>, mask: Bounds<ScaledPixels>, color: Hsla) -> Quad {
    Quad {
        order: 0,
        border_style: BorderStyle::default(),
        bounds,
        content_mask: ContentMask { bounds: mask },
        background: color.into(),
        border_color: hsla(0., 0., 0., 0.8),
        corner_radii: Corners::all(ScaledPixels(6.)),
        border_widths: Edges::all(ScaledPixels(1.5)),
    }
}

fn plain_quad(bounds: Bounds<ScaledPixels>, color: Hsla) -> Quad {
    Quad {
        corner_radii: Corners::default(),
        border_widths: Edges::default(),
        ..quad(bounds, bounds, color)
    }
}

/// What a cached view may paint: translucent round quads that lie over each other, shadows and underlines, at
/// positions between device pixels, some cut by a content mask. It starts at `(x, y)` and goes past the right
/// edge of the viewport. With `backdrop`, all of it lies on an opaque quad that fills whole device pixels, as
/// the content of a view with a background does.
fn content(x: f32, y: f32, hue: f32, backdrop: bool) -> Arc<Scene> {
    let mask = bounds(x, y, 420., 170.);
    let mut scene = Scene::default();
    if backdrop {
        let around = bounds(x.floor() - 12., y.floor() - 12., 450., 210.);
        scene.insert_primitive(plain_quad(around, hsla(0.75, 0.4, 0.2, 1.)));
    }
    for row in 0..8 {
        for column in 0..12 {
            let cell = bounds(x + column as f32 * 31.25, y + row as f32 * 21.5, 44., 30.);
            if (row + column) % 5 == 0 {
                scene.insert_primitive(Shadow {
                    order: 0,
                    blur_radius: ScaledPixels(5.),
                    bounds: cell,
                    corner_radii: Corners::all(ScaledPixels(6.)),
                    content_mask: ContentMask { bounds: mask },
                    color: hsla(0., 0., 0., 0.5),
                    element_bounds: cell,
                    element_corner_radii: Corners::all(ScaledPixels(6.)),
                    inset: 0,
                    pad: 0,
                });
            }
            let color = hsla((hue + column as f32 / 12.) % 1., 0.7, 0.5, 0.6);
            scene.insert_primitive(quad(cell, mask, color));
            if (row + column) % 3 == 0 {
                scene.insert_primitive(Underline {
                    order: 0,
                    pad: 0,
                    bounds: bounds(cell.origin.x.0, cell.origin.y.0 + 24., 44., 4.),
                    content_mask: ContentMask { bounds: mask },
                    color: hsla(0., 0., 1., 0.9),
                    thickness: ScaledPixels(1.5),
                    wavy: (column % 2 == 0).into(),
                });
            }
        }
    }
    scene.finish();
    Arc::new(scene)
}

fn chunk(content: &Arc<Scene>, layer_id: u64, generation: u64) -> RetainedChunk {
    RetainedChunk {
        order: 0,
        bounds: content.covered_bounds().unwrap(),
        layer_id,
        generation,
        scene: content.clone(),
    }
}

fn frame(chunk: Option<RetainedChunk>) -> Scene {
    let everything = bounds(0., 0., 400., 300.);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(everything, everything, hsla(0.6, 0.3, 0.9, 1.)));
    scene.insert_primitive(quad(
        bounds(20., 20., 200., 120.),
        everything,
        hsla(0.1, 0.8, 0.5, 1.),
    ));
    if let Some(chunk) = chunk {
        scene.insert_retained_chunk(chunk);
    }
    scene.insert_primitive(quad(
        bounds(150., 90., 180., 150.),
        everything,
        hsla(0.3, 0.9, 0.3, 0.5),
    ));
    scene.finish();
    scene
}

fn render(renderer: &mut MetalRenderer, scene: &Scene) -> RgbaImage {
    renderer.render_scene_to_image(scene, VIEWPORT).unwrap()
}

fn greatest_difference(a: &RgbaImage, b: &RgbaImage) -> u8 {
    assert_eq!(a.dimensions(), b.dimensions());
    a.as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap()
}

/// Three frames of one renderer with the same chunk: drawn in place, drawn into a new layer, and composited
/// from the kept layer.
fn in_place_and_layered(content: &Arc<Scene>) -> (RgbaImage, RgbaImage, RgbaImage) {
    assert!(content.summary().primitive_count >= MIN_PRIMITIVES);
    let scene = frame(Some(chunk(content, 1, 1)));
    let mut renderer = renderer();

    let in_place = render(&mut renderer, &scene);
    assert!(
        renderer.layer_cache.layers.is_empty(),
        "a chunk in its first frame has no layer"
    );
    let without = render(&mut self::renderer(), &frame(None));
    assert!(
        greatest_difference(&in_place, &without) > 100,
        "the chunk shows on the frame"
    );

    let drawn = render(&mut renderer, &scene);
    assert_eq!(
        renderer.layer_cache.layers.len(),
        1,
        "the clean chunk has a layer"
    );
    let kept = render(&mut renderer, &scene);
    (in_place, drawn, kept)
}

#[test]
fn an_opaque_chunk_composited_from_its_layer_is_the_chunk_drawn_in_place() {
    let (in_place, drawn, kept) = in_place_and_layered(&content(37.5, 53.25, 0., true));
    assert_eq!(greatest_difference(&in_place, &drawn), 0);
    assert_eq!(greatest_difference(&in_place, &kept), 0);
}

#[test]
fn a_translucent_chunk_composited_from_its_layer_is_within_the_rounding_of_the_layer() {
    let (in_place, drawn, kept) = in_place_and_layered(&content(37.5, 53.25, 0., false));
    assert_eq!(greatest_difference(&drawn, &kept), 0);

    // A layer holds 8 bits a channel. What is blended into it and then over the frame is rounded once more
    // than what is blended over the frame in place, for each translucent primitive on a pixel: here up to
    // three (a shadow, a quad's edge and an underline, or quads that lie over each other).
    assert!(greatest_difference(&in_place, &drawn) <= 3);
    let off_by_more_than_one = in_place
        .pixels()
        .zip(drawn.pixels())
        .filter(|(a, b)| a.0.iter().zip(b.0).any(|(a, b)| a.abs_diff(b) > 1))
        .count();
    let pixels = (VIEWPORT.width.0 * VIEWPORT.height.0) as usize;
    assert!(
        off_by_more_than_one * 100 < pixels,
        "{off_by_more_than_one} of {pixels} pixels are off by more than one"
    );
}

fn card_with_a_shadow() -> Arc<Scene> {
    let everything = bounds(0., 0., 400., 300.);
    let card = bounds(120., 100., 160., 80.);
    let mut scene = Scene::default();
    scene.insert_primitive(Shadow {
        order: 0,
        blur_radius: ScaledPixels(15.),
        bounds: card,
        corner_radii: Corners::all(ScaledPixels(6.)),
        content_mask: ContentMask { bounds: everything },
        color: hsla(0., 0., 0., 0.6),
        element_bounds: card,
        element_corner_radii: Corners::all(ScaledPixels(6.)),
        inset: 0,
        pad: 0,
    });
    for index in 0..MIN_PRIMITIVES {
        let (column, row) = ((index % 16) as f32, (index / 16) as f32);
        let cell = bounds(120. + column * 10., 100. + row * 20., 10., 20.);
        scene.insert_primitive(plain_quad(cell, hsla(column / 16., 0.6, 0.5, 1.)));
    }
    scene.finish();
    Arc::new(scene)
}

#[test]
fn the_blur_of_a_shadow_past_its_bounds_is_on_the_layer() {
    let (in_place, drawn, kept) = in_place_and_layered(&card_with_a_shadow());
    assert!(greatest_difference(&in_place, &drawn) <= 1);
    assert!(greatest_difference(&in_place, &kept) <= 1);
}

#[test]
fn a_chunk_that_painted_again_is_not_composited_from_its_old_layer() {
    let (first, second) = (
        content(37.5, 53.25, 0., true),
        content(37.5, 53.25, 0.4, true),
    );
    let mut renderer = renderer();
    for _ in 0..2 {
        render(&mut renderer, &frame(Some(chunk(&first, 1, 1))));
    }
    assert_eq!(renderer.layer_cache.layers[&1].generation, 1);

    let repainted = render(&mut renderer, &frame(Some(chunk(&second, 1, 2))));
    let in_place = render(&mut self::renderer(), &frame(Some(chunk(&second, 1, 2))));
    assert_eq!(greatest_difference(&repainted, &in_place), 0);

    let layered = render(&mut renderer, &frame(Some(chunk(&second, 1, 2))));
    assert_eq!(greatest_difference(&layered, &in_place), 0);
    assert_eq!(
        renderer.layer_cache.layers[&1].generation, 2,
        "the chunk has a layer of what it painted last"
    );
}

#[test]
fn a_chunk_that_was_off_the_frame_starts_again_without_a_layer() {
    let content = content(37.5, 53.25, 0., true);
    let mut renderer = renderer();
    render(&mut renderer, &frame(Some(chunk(&content, 1, 1))));
    render(&mut renderer, &frame(None));

    render(&mut renderer, &frame(Some(chunk(&content, 1, 1))));
    assert!(
        renderer.layer_cache.layers.is_empty(),
        "the two frames the chunk was on are not in a row"
    );
    render(&mut renderer, &frame(Some(chunk(&content, 1, 1))));
    assert_eq!(renderer.layer_cache.layers.len(), 1);
}

#[test]
fn a_window_that_is_not_opaque_draws_every_chunk_in_place() {
    let content = content(37.5, 53.25, 0., false);
    let scene = frame(Some(chunk(&content, 1, 1)));
    let mut transparent = renderer();
    transparent.update_transparency(true);
    let first = render(&mut transparent, &scene);
    let clean = render(&mut transparent, &scene);
    assert!(transparent.layer_cache.layers.is_empty());
    assert_eq!(greatest_difference(&clean, &first), 0);
}

/// 64 quads spread over a square of 2048 device pixels: a layer of 16 MiB, a quarter of what the cache holds.
fn large_content() -> Arc<Scene> {
    let everything = bounds(0., 0., 2048., 2048.);
    let mut scene = Scene::default();
    for index in 0..64 {
        let offset = index as f32 * 32.;
        scene.insert_primitive(quad(
            bounds(offset, offset, 32., 32.),
            everything,
            hsla(0.5, 0.5, 0.5, 1.),
        ));
    }
    scene.finish();
    Arc::new(scene)
}

fn frame_of(chunks: impl IntoIterator<Item = RetainedChunk>) -> Scene {
    let mut scene = Scene::default();
    for chunk in chunks {
        scene.insert_retained_chunk(chunk);
    }
    scene.finish();
    scene
}

#[test]
fn the_layers_of_a_renderer_stay_within_their_memory() {
    let viewport = size(DevicePixels(2048), DevicePixels(2048));
    let content = large_content();
    let layer_bytes = 2048 * 2048 * BYTES_PER_PIXEL;
    assert_eq!(MAX_CACHED_LAYER_BYTES, 4 * layer_bytes);
    let mut renderer = renderer();

    let five = frame_of((1..=5).map(|layer_id| chunk(&content, layer_id, 1)));
    for _ in 0..2 {
        renderer.render_scene(&five, viewport).unwrap();
    }
    assert_eq!(
        renderer.layer_cache.layers.len(),
        4,
        "of five clean chunks, four get a layer and the fifth is drawn in place"
    );
    assert_eq!(renderer.layer_cache.bytes, MAX_CACHED_LAYER_BYTES);

    let sixth = frame_of([chunk(&content, 6, 1)]);
    for _ in 0..2 {
        renderer.render_scene(&sixth, viewport).unwrap();
    }
    assert!(
        renderer.layer_cache.layers.contains_key(&6),
        "a sixth chunk takes the place of a layer its frame does not use"
    );
    assert_eq!(renderer.layer_cache.layers.len(), 4);
    assert_eq!(renderer.layer_cache.bytes, MAX_CACHED_LAYER_BYTES);

    for _ in 0..MAX_IDLE_FRAMES {
        renderer.render_scene(&sixth, viewport).unwrap();
    }
    assert_eq!(
        renderer.layer_cache.layers.len(),
        1,
        "a layer no frame used for more than MAX_IDLE_FRAMES frames is dropped"
    );
    assert_eq!(renderer.layer_cache.bytes, layer_bytes);

    renderer.destroy();
    assert!(renderer.layer_cache.layers.is_empty());
    assert_eq!(renderer.layer_cache.bytes, 0);
}
