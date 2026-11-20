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

    fn layer(&self, chunk: &RetainedChunk) -> Option<&Layer> {
        self.layers.get(&chunk.layer_id).filter(|layer| {
            layer.generation == chunk.generation && layer.last_used_frame == self.frame
        })
    }

    fn keep(&mut self, chunk: &RetainedChunk, rect: Bounds<DevicePixels>) -> bool {
        match self.layers.get_mut(&chunk.layer_id) {
            Some(layer) if layer.generation == chunk.generation && layer.rect == rect => {
                layer.last_used_frame = self.frame;
                true
            }
            _ => false,
        }
    }

    /// `None` when the layers this frame uses leave no room for a texture of `rect`'s size.
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
    /// A chunk that stays drawn in place is searched for chunks of its own.
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

    /// `false` when the cache has no room for the layer.
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

/// A chunk with a layer in `layers` for this frame gets the sprite of that layer in place of its instances. With an
/// `origin`, every primitive is moved back by it, to draw into a layer.
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
#[path = "metal_layer_cache_tests.rs"]
mod tests;
