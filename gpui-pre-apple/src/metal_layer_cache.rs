use super::{
    InstanceBinding, InstanceBindings, InstanceBufferWriter, MetalRenderer, SpriteInputIndex,
    SurfaceBounds, build_path_sprite_pipeline_state, build_pipeline_state_with_destination_alpha,
    new_command_encoder_for_texture, write_instances,
};
use anyhow::Result;
use gpui::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask, Corners, DevicePixels,
    MonochromeSprite, Point, PolychromeSprite, Quad, RetainedChunk, ScaledPixels, Scene, Shadow,
    Size, TileId, Underline, point, size,
};
use metal::MTLPixelFormat;
use std::{collections::HashMap, mem, slice};

// Content that repaints in every frame (a pane while it scrolls) never pays for a pass of its own.
const MIN_CLEAN_FRAMES: u32 = 1;
// Below this, drawing a chunk in place costs less than compositing its layer.
const MIN_PRIMITIVES: usize = 64;
const MAX_IDLE_FRAMES: u64 = 4;
// A chunk that would go past this takes the place of the layers used longest ago, or is drawn in place when every
// layer is in use.
const MAX_CACHED_LAYER_BYTES: usize = 64 * 1024 * 1024;
const BYTES_PER_PIXEL: usize = 4;

pub(super) struct PrimitivePipelineSet {
    pub shadows: metal::RenderPipelineState,
    pub quads: metal::RenderPipelineState,
    pub underlines: metal::RenderPipelineState,
    pub monochrome_sprites: metal::RenderPipelineState,
    pub polychrome_sprites: metal::RenderPipelineState,
}

impl PrimitivePipelineSet {
    /// `destination_alpha` is the factor of the target's alpha in the blend. `One` adds the alpha of each primitive
    /// to an opaque target. `OneMinusSourceAlpha` blends it over a target that starts transparent, which then holds
    /// premultiplied colour and composites as the same primitives drawn in place would.
    pub(super) fn build(
        device: &metal::DeviceRef,
        library: &metal::LibraryRef,
        label_prefix: &str,
        destination_alpha: metal::MTLBlendFactor,
    ) -> Self {
        let build = |primitive: &str| {
            build_pipeline_state_with_destination_alpha(
                device,
                library,
                &format!("{label_prefix}{primitive}s"),
                &format!("{primitive}_vertex"),
                &format!("{primitive}_fragment"),
                MTLPixelFormat::BGRA8Unorm,
                destination_alpha,
            )
        };
        Self {
            shadows: build("shadow"),
            quads: build("quad"),
            underlines: build("underline"),
            monochrome_sprites: build("monochrome_sprite"),
            polychrome_sprites: build("polychrome_sprite"),
        }
    }
}

pub(super) struct MetalLayerCache {
    pub(super) pipelines: PrimitivePipelineSet,
    composite_pipeline_state: metal::RenderPipelineState,
    layers: HashMap<u64, Layer>,
    bytes: usize,
    /// The chunks of the frame before and of this one, by layer id.
    seen: HashMap<u64, SeenChunk>,
    frame: u64,
}

struct Layer {
    generation: u64,
    rect: Bounds<DevicePixels>,
    texture: metal::Texture,
    last_used_frame: u64,
    /// The texture holds nothing until the command buffer of this frame is committed.
    drawn_in_frame: u64,
}

struct SeenChunk {
    generation: u64,
    frame: u64,
    /// Frames in a row, before `frame`, that the chunk went on the frame with this generation.
    clean_frames: u32,
}

impl Layer {
    fn bytes(&self) -> usize {
        texture_bytes(self.rect.size)
    }

    fn sprite(&self, order: u32) -> PolychromeSprite {
        let bounds = Bounds {
            origin: point(
                ScaledPixels(self.rect.origin.x.0 as f32),
                ScaledPixels(self.rect.origin.y.0 as f32),
            ),
            size: size(
                ScaledPixels(self.rect.size.width.0 as f32),
                ScaledPixels(self.rect.size.height.0 as f32),
            ),
        };
        PolychromeSprite {
            order,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.0,
            bounds,
            content_mask: ContentMask { bounds },
            corner_radii: Corners::default(),
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Polychrome,
                },
                tile_id: TileId(0),
                padding: 0,
                bounds: Bounds {
                    origin: point(DevicePixels(0), DevicePixels(0)),
                    size: self.rect.size,
                },
            },
        }
    }
}

fn texture_bytes(size: Size<DevicePixels>) -> usize {
    size.width.0.max(0) as usize * size.height.0.max(0) as usize * BYTES_PER_PIXEL
}

/// The chunk's bounds out to whole device pixels, cut to the viewport.
fn layer_rect(
    chunk: &RetainedChunk,
    viewport_size: Size<DevicePixels>,
) -> Option<Bounds<DevicePixels>> {
    let bounds = chunk.bounds;
    let left = (bounds.origin.x.0.floor() as i32).max(0);
    let top = (bounds.origin.y.0.floor() as i32).max(0);
    let right =
        ((bounds.origin.x.0 + bounds.size.width.0).ceil() as i32).min(viewport_size.width.0);
    let bottom =
        ((bounds.origin.y.0 + bounds.size.height.0).ceil() as i32).min(viewport_size.height.0);
    (right > left && bottom > top).then(|| Bounds {
        origin: point(DevicePixels(left), DevicePixels(top)),
        size: size(DevicePixels(right - left), DevicePixels(bottom - top)),
    })
}

impl MetalLayerCache {
    pub(super) fn new(device: &metal::DeviceRef, library: &metal::LibraryRef) -> Self {
        Self {
            pipelines: PrimitivePipelineSet::build(
                device,
                library,
                "layer_",
                metal::MTLBlendFactor::OneMinusSourceAlpha,
            ),
            // Premultiplied source over, as the path sprites are composited.
            composite_pipeline_state: build_path_sprite_pipeline_state(
                device,
                library,
                "layer_composite",
                "polychrome_sprite_vertex",
                "polychrome_sprite_fragment",
                MTLPixelFormat::BGRA8Unorm,
            ),
            layers: HashMap::new(),
            bytes: 0,
            seen: HashMap::new(),
            frame: 0,
        }
    }

    pub(super) fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub(super) fn end_frame(&mut self) {
        let frame = self.frame;
        self.seen.retain(|_, seen| seen.frame == frame);
        self.layers
            .retain(|_, layer| frame - layer.last_used_frame <= MAX_IDLE_FRAMES);
        self.bytes = self.layers.values().map(Layer::bytes).sum();
    }

    /// Drops the layers the frame drew into, when its command buffer is not committed.
    pub(super) fn forget_frame(&mut self) {
        let frame = self.frame;
        self.layers.retain(|_, layer| layer.drawn_in_frame != frame);
        self.bytes = self.layers.values().map(Layer::bytes).sum();
    }

    pub(super) fn clear(&mut self) {
        self.layers.clear();
        self.seen.clear();
        self.bytes = 0;
    }

    /// Notes that `chunk` is on this frame. Returns how many frames in a row before this one it was on the frame
    /// as it is now.
    fn clean_frames(&mut self, chunk: &RetainedChunk) -> u32 {
        let frame = self.frame;
        let new = SeenChunk {
            generation: chunk.generation,
            frame,
            clean_frames: 0,
        };
        let seen = self.seen.entry(chunk.layer_id).or_insert(new);
        if seen.generation != chunk.generation {
            seen.generation = chunk.generation;
            seen.clean_frames = 0;
        } else if seen.frame != frame {
            seen.clean_frames = seen.clean_frames.saturating_add(1);
        }
        seen.frame = frame;
        seen.clean_frames
    }

    /// The layer `chunk` composites from in this frame.
    fn layer(&self, chunk: &RetainedChunk) -> Option<&Layer> {
        self.layers.get(&chunk.layer_id).filter(|layer| {
            layer.generation == chunk.generation && layer.last_used_frame == self.frame
        })
    }

    /// Takes the chunk's layer for this frame if it is up to date and covers `rect`.
    fn keep(&mut self, chunk: &RetainedChunk, rect: Bounds<DevicePixels>) -> bool {
        match self.layers.get_mut(&chunk.layer_id) {
            Some(layer) if layer.generation == chunk.generation && layer.rect == rect => {
                layer.last_used_frame = self.frame;
                true
            }
            _ => false,
        }
    }

    /// A texture of `rect`'s size to draw `chunk` into: the one of its stale layer when the size is the same, else
    /// a new one, for which layers no frame uses any more make room. `None` when the layers in use leave no room.
    fn take_texture(
        &mut self,
        device: &metal::DeviceRef,
        chunk: &RetainedChunk,
        rect: Bounds<DevicePixels>,
    ) -> Option<metal::Texture> {
        if let Some(stale) = self.layers.remove(&chunk.layer_id) {
            self.bytes -= stale.bytes();
            if stale.rect.size == rect.size {
                return Some(stale.texture);
            }
        }
        let bytes = texture_bytes(rect.size);
        while self.bytes + bytes > MAX_CACHED_LAYER_BYTES {
            let frame = self.frame;
            let oldest = self
                .layers
                .iter()
                .filter(|(_, layer)| layer.last_used_frame < frame)
                .min_by_key(|(_, layer)| layer.last_used_frame)
                .map(|(layer_id, _)| *layer_id)?;
            if let Some(layer) = self.layers.remove(&oldest) {
                self.bytes -= layer.bytes();
            }
        }
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_width(rect.size.width.0 as u64);
        descriptor.set_height(rect.size.height.0 as u64);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        descriptor.set_storage_mode(metal::MTLStorageMode::Private);
        Some(device.new_texture(&descriptor))
    }

    fn insert(
        &mut self,
        chunk: &RetainedChunk,
        rect: Bounds<DevicePixels>,
        texture: metal::Texture,
    ) {
        let layer = Layer {
            generation: chunk.generation,
            rect,
            texture,
            last_used_frame: self.frame,
            drawn_in_frame: self.frame,
        };
        self.bytes += layer.bytes();
        self.layers.insert(chunk.layer_id, layer);
    }
}

impl MetalRenderer {
    /// Gives each chunk of the scene that is worth a layer one that is up to date, drawing into it when it is
    /// missing or stale. A chunk that stays drawn in place is searched for chunks of its own.
    pub(super) fn refresh_layers(
        &mut self,
        scene: &Scene,
        writer: &mut InstanceBufferWriter,
        command_buffer: &metal::CommandBufferRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<()> {
        for chunk in &scene.retained_chunks {
            if self.layer_cache.clean_frames(chunk) >= MIN_CLEAN_FRAMES
                && let Some(rect) = layer_rect(chunk, viewport_size)
            {
                if self.layer_cache.keep(chunk, rect) {
                    continue;
                }
                let summary = chunk.scene.summary();
                if summary.primitive_count >= MIN_PRIMITIVES
                    && summary.is_translation_invariant
                    && self.draw_layer(chunk, rect, writer, command_buffer)?
                {
                    continue;
                }
            }
            self.refresh_layers(&chunk.scene, writer, command_buffer, viewport_size)?;
        }
        Ok(())
    }

    /// Draws `chunk` into a layer the size of `rect`, with every primitive moved back by `rect`'s origin. `false`
    /// when the cache has no room for the layer.
    fn draw_layer(
        &mut self,
        chunk: &RetainedChunk,
        rect: Bounds<DevicePixels>,
        writer: &mut InstanceBufferWriter,
        command_buffer: &metal::CommandBufferRef,
    ) -> Result<bool> {
        let Some(texture) = self.layer_cache.take_texture(&self.device, chunk, rect) else {
            return Ok(false);
        };
        let origin = point(
            ScaledPixels(rect.origin.x.0 as f32),
            ScaledPixels(rect.origin.y.0 as f32),
        );
        let bindings = write_scene_instances(&chunk.scene, writer, Some(origin), None)?;
        let mut command_encoder = new_command_encoder_for_texture(
            command_buffer,
            &texture,
            rect.size,
            Some(metal::MTLClearColor::new(0., 0., 0., 0.)),
        );
        self.encode_scene(
            &chunk.scene,
            &bindings,
            &self.layer_cache.pipelines,
            writer,
            &texture,
            rect.size,
            command_buffer,
            &mut command_encoder,
        )?;
        command_encoder.end_encoding();
        self.layer_cache.insert(chunk, rect, texture);
        Ok(true)
    }

    pub(super) fn composite_layer(
        &self,
        layer: &LayerBinding,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        command_encoder.set_render_pipeline_state(&self.layer_cache.composite_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&layer.sprite.buffer),
            layer.sprite.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&layer.size) as u64,
            &layer.size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&layer.sprite.buffer),
            layer.sprite.offset as u64,
        );
        command_encoder
            .set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&layer.texture));
        command_encoder.draw_primitives_instanced(metal::MTLPrimitiveType::Triangle, 0, 6, 1);
    }
}

pub(super) struct SceneBindings {
    pub own: InstanceBindings,
    /// By the chunk's index in [`Scene::retained_chunks`].
    pub chunks: Vec<ChunkBindings>,
}

pub(super) enum ChunkBindings {
    InPlace(SceneBindings),
    Layer(LayerBinding),
}

pub(super) struct LayerBinding {
    sprite: InstanceBinding,
    texture: metal::Texture,
    size: Size<DevicePixels>,
}

/// Writes the instances of the scene and of its chunks. A chunk with a layer in `layers` for this frame gets the
/// sprite of that layer instead. With an `origin`, every primitive is moved back by it, to draw into a layer.
pub(super) fn write_scene_instances(
    scene: &Scene,
    writer: &mut InstanceBufferWriter,
    origin: Option<Point<ScaledPixels>>,
    layers: Option<&MetalLayerCache>,
) -> Result<SceneBindings> {
    let own = match origin {
        None => write_instances(scene, writer)?,
        Some(origin) => write_moved_instances(scene, writer, origin)?,
    };
    let chunks = scene
        .retained_chunks
        .iter()
        .map(
            |chunk| match layers.and_then(|layers| layers.layer(chunk)) {
                Some(layer) => Ok(ChunkBindings::Layer(LayerBinding {
                    sprite: writer.write(slice::from_ref(&layer.sprite(chunk.order)))?,
                    texture: layer.texture.clone(),
                    size: layer.rect.size,
                })),
                None => write_scene_instances(&chunk.scene, writer, origin, layers)
                    .map(ChunkBindings::InPlace),
            },
        )
        .collect::<Result<_>>()?;
    Ok(SceneBindings { own, chunks })
}

/// The scene must be translation invariant ([`gpui::SceneSummary::is_translation_invariant`]).
fn write_moved_instances(
    scene: &Scene,
    writer: &mut InstanceBufferWriter,
    origin: Point<ScaledPixels>,
) -> Result<InstanceBindings> {
    let moved = |bounds: Bounds<ScaledPixels>| Bounds {
        origin: point(bounds.origin.x - origin.x, bounds.origin.y - origin.y),
        size: bounds.size,
    };
    let mask = |mask: ContentMask<ScaledPixels>| ContentMask {
        bounds: moved(mask.bounds),
    };
    Ok(InstanceBindings {
        quads: writer.write_iter(scene.quads.iter().map(|quad| Quad {
            bounds: moved(quad.bounds),
            content_mask: mask(quad.content_mask),
            ..*quad
        }))?,
        shadows: writer.write_iter(scene.shadows.iter().map(|shadow| Shadow {
            bounds: moved(shadow.bounds),
            content_mask: mask(shadow.content_mask),
            element_bounds: moved(shadow.element_bounds),
            ..*shadow
        }))?,
        underlines: writer.write_iter(scene.underlines.iter().map(|underline| Underline {
            bounds: moved(underline.bounds),
            content_mask: mask(underline.content_mask),
            ..*underline
        }))?,
        monochrome_sprites: writer.write_iter(scene.monochrome_sprites.iter().map(|sprite| {
            MonochromeSprite {
                bounds: moved(sprite.bounds),
                content_mask: mask(sprite.content_mask),
                ..*sprite
            }
        }))?,
        polychrome_sprites: writer.write_iter(scene.polychrome_sprites.iter().map(|sprite| {
            PolychromeSprite {
                bounds: moved(sprite.bounds),
                content_mask: mask(sprite.content_mask),
                ..*sprite
            }
        }))?,
        surfaces: writer.write::<SurfaceBounds>(&[])?,
    })
}

#[cfg(test)]
mod tests {
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

    /// A frame with a background, the chunk over it, and a translucent quad painted over part of the chunk.
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

        // Five clean chunks in one frame: four get a layer, the fifth is drawn in place.
        let five = frame_of((1..=5).map(|layer_id| chunk(&content, layer_id, 1)));
        for _ in 0..2 {
            renderer.render_scene(&five, viewport).unwrap();
        }
        assert_eq!(renderer.layer_cache.layers.len(), 4);
        assert_eq!(renderer.layer_cache.bytes, MAX_CACHED_LAYER_BYTES);

        // A sixth chunk alone on the frame takes the place of a layer that frame does not use.
        let sixth = frame_of([chunk(&content, 6, 1)]);
        for _ in 0..2 {
            renderer.render_scene(&sixth, viewport).unwrap();
        }
        assert!(renderer.layer_cache.layers.contains_key(&6));
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
}
