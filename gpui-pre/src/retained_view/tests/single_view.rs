use crate::{
    self as gpui, AppContext as _, AvailableSpace, Bounds, Context, Entity, IntoElement,
    ParentElement, Render, ScaledPixels, Size, StyleRefinement, Styled, TestAppContext,
    VisualTestContext, Window, canvas, div, fill, prelude::FluentBuilder as _, px, rems, rgb, size,
};

fn retained_quad_sizes(cx: &mut VisualTestContext) -> Vec<Size<ScaledPixels>> {
    cx.update(|window, _| {
        window
            .rendered_frame
            .scene
            .retained_chunks
            .iter()
            .flat_map(|chunk| chunk.scene.quads.iter().map(|quad| quad.bounds.size))
            .collect()
    })
}

struct RemBox {
    renders: usize,
}

impl Render for RemBox {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div().w(rems(2.)).h(rems(1.)).bg(rgb(0xff0000))
    }
}

struct RemRoot {
    child: Entity<RemBox>,
}

impl Render for RemRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            self.child
                .clone()
                .cached(StyleRefinement::default().w(px(100.)).h(px(100.))),
        )
    }
}

#[gpui::test]
fn a_cached_view_lays_out_again_when_the_rem_size_changes(cx: &mut TestAppContext) {
    let window = cx.open_window(size(px(400.), px(400.)), |_, cx| RemRoot {
        child: cx.new(|_| RemBox { renders: 0 }),
    });
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let scale = cx.update(|window, _| window.scale_factor());
    let scaled = |width: f32, height: f32| size(px(width).scale(scale), px(height).scale(scale));
    assert_eq!(retained_quad_sizes(&mut cx), [scaled(32., 16.)]);

    cx.update(|window, cx| {
        window.set_rem_size(px(20.));
        window
            .root::<RemRoot>()
            .unwrap()
            .unwrap()
            .update(cx, |_, cx| cx.notify());
    });
    cx.run_until_parked();
    assert_eq!(retained_quad_sizes(&mut cx), [scaled(40., 20.)]);
}

struct Bar {
    width: f32,
}

impl Render for Bar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(self.width)).h(px(10.)).bg(rgb(0xff0000))
    }
}

struct BarRoot {
    bar: Entity<Bar>,
    hidden: bool,
}

impl Render for BarRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            div().when(self.hidden, |bar| bar.invisible()).child(
                self.bar
                    .clone()
                    .cached(StyleRefinement::default().w(px(100.)).h(px(100.))),
            ),
        )
    }
}

#[gpui::test]
fn a_cached_view_that_rendered_while_hidden_shows_what_it_rendered_last(cx: &mut TestAppContext) {
    let window = cx.open_window(size(px(400.), px(400.)), |_, cx| BarRoot {
        bar: cx.new(|_| Bar { width: 10. }),
        hidden: false,
    });
    let root = window.root(cx).unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let scale = cx.update(|window, _| window.scale_factor());
    let scaled = |width: f32| size(px(width).scale(scale), px(10.).scale(scale));
    assert_eq!(retained_quad_sizes(&mut cx), [scaled(10.)]);

    let set_hidden = |hidden: bool, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            root.update(cx, |root, cx| {
                root.hidden = hidden;
                cx.notify();
            })
        });
        cx.run_until_parked();
    };
    set_hidden(true, &mut cx);
    assert!(retained_quad_sizes(&mut cx).is_empty());
    cx.update(|_, cx| {
        root.read(cx).bar.clone().update(cx, |bar, cx| {
            bar.width = 30.;
            cx.notify();
        })
    });
    cx.run_until_parked();

    set_hidden(false, &mut cx);
    assert_eq!(
        retained_quad_sizes(&mut cx),
        [scaled(30.)],
        "the first frame the bar is visible in shows its new width"
    );
}

struct Box {
    renders: usize,
}

impl Render for Box {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div().size_full().bg(rgb(0x00ff00))
    }
}

/// Paints its child in a layer and a quad over part of it afterwards, in the same layer.
struct LayerRoot {
    child: Entity<Box>,
    cached: bool,
}

impl Render for LayerRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (child, cached) = (self.child.clone(), self.cached);
        canvas(
            move |bounds, window, cx| {
                let mut child = if cached {
                    child
                        .cached(StyleRefinement::default().size_full())
                        .into_any_element()
                } else {
                    child.into_any_element()
                };
                child.prepaint_as_root(
                    bounds.origin,
                    bounds.size.map(AvailableSpace::Definite),
                    window,
                    cx,
                );
                child
            },
            |bounds, mut child, window, cx| {
                window.paint_layer(bounds, |window| {
                    child.paint(window, cx);
                    let over = Bounds::new(bounds.origin, size(px(50.), px(50.)));
                    window.paint_quad(fill(over, rgb(0xff0000)));
                });
            },
        )
        .size_full()
    }
}

#[gpui::test]
fn a_cached_view_in_a_layer_is_drawn_in_paint_order_as_an_uncached_one(cx: &mut TestAppContext) {
    let mut frames = Vec::new();
    for cached in [false, true] {
        let window = cx.open_window(size(px(200.), px(200.)), |_, cx| LayerRoot {
            child: cx.new(|_| Box { renders: 0 }),
            cached,
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        // A second frame, in which the cached view is carried over.
        cx.update(|window, cx| {
            window
                .root::<LayerRoot>()
                .unwrap()
                .unwrap()
                .update(cx, |_, cx| cx.notify())
        });
        cx.run_until_parked();
        frames.push(cx.update(|window, cx| {
            let renders = window
                .root::<LayerRoot>()
                .unwrap()
                .unwrap()
                .read(cx)
                .child
                .read(cx)
                .renders;
            let scene = &window.rendered_frame.scene;
            let batches = scene
                .batches()
                .map(|batch| batch.label())
                .collect::<Vec<_>>();
            let quads = scene
                .quads
                .iter()
                .map(|quad| (quad.order, quad.bounds))
                .collect::<Vec<_>>();
            (renders, batches, quads)
        }));
    }
    let (uncached, cached) = (&frames[0], &frames[1]);
    assert_eq!(uncached.0, 2, "an uncached view renders in each frame");
    assert_eq!(
        cached.0, 1,
        "the cached view was carried over in the second frame"
    );
    assert_eq!(
        (&cached.1, &cached.2),
        (&uncached.1, &uncached.2),
        "the layer draws the view's quad, then the quad painted over it"
    );
}
