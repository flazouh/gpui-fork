use crate::{
    self as gpui, AnyView, App, AppContext as _, AvailableSpace, Bounds, Context, Entity, EntityId,
    FocusHandle, InteractiveElement, IntoElement, Modifiers, PaintOperation, ParentElement, Pixels,
    Primitive, Render, ScaledPixels, Scene, Size, StatefulInteractiveElement, StyleRefinement,
    Styled, TestAppContext, View, ViewElement, VisualTestContext, Window, canvas, div, fill, point,
    prelude::FluentBuilder as _, px, rems, rgb, size,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

type Log = Rc<RefCell<Vec<String>>>;

const HEADER_HEIGHT: f32 = 40.;
const ROW_HEIGHT: f32 = 20.;
const ROW_COUNT: usize = 10;

struct Row {
    index: usize,
    label: u64,
    renders: usize,
    focus: FocusHandle,
    log: Log,
}

impl Render for Row {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        let (index, log) = (self.index, self.log.clone());
        div()
            .id(("row", index))
            .size_full()
            .bg(rgb(0x204060))
            .track_focus(&self.focus)
            .on_click(move |_, _, _| log.borrow_mut().push(format!("row{index}")))
            .child(format!("row {index}: {}", self.label))
    }
}

struct Status {
    count: u64,
    renders: usize,
}

impl Render for Status {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div().child(format!("status {}", self.count))
    }
}

/// A hand-written view the pane draws uncached. Its entity gives it an identity, and rendering does not read it.
#[derive(Clone)]
struct Badge {
    entity: Entity<Status>,
    renders: Rc<Cell<usize>>,
}

impl View for Badge {
    fn entity_id(&self) -> Option<EntityId> {
        Some(self.entity.entity_id())
    }

    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        div().child("badge")
    }
}

#[derive(Default)]
struct Pane {
    rows: Vec<Entity<Row>>,
    status: Option<Entity<Status>>,
    badge: Option<Badge>,
    renders: usize,
    log: Log,
    header_reads_first_row: bool,
    rows_in_hover_group: bool,
    changes_rows_when_it_renders: bool,
}

impl Render for Pane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        if self.changes_rows_when_it_renders {
            for row in &self.rows {
                row.update(cx, |row, _| row.label += 100);
            }
        }
        let header = if self.header_reads_first_row {
            format!("first row: {}", self.rows[0].read(cx).label)
        } else {
            "pane".to_string()
        };
        let log = self.log.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("header")
                    .h(px(HEADER_HEIGHT))
                    .w_full()
                    .flex_none()
                    .on_click(move |_, _, _| log.borrow_mut().push("header".into()))
                    .child(header)
                    .children(self.status.clone())
                    .children(self.badge.clone().map(ViewElement::new)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .when(self.rows_in_hover_group, |rows| rows.group("rows"))
                    .children(self.rows.iter().map(|row| {
                        row.clone()
                            .cached(StyleRefinement::default().w_full().h(px(ROW_HEIGHT)))
                    })),
            )
    }
}

/// A cached view around the pane, for three cached views in each other.
struct Shell {
    pane: Entity<Pane>,
    renders: usize,
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div().size_full().child(
            self.pane
                .clone()
                .cached(StyleRefinement::default().size_full()),
        )
    }
}

struct Root {
    content: AnyView,
    /// Clickable boxes drawn before the content and beside it: more of them move the content's output in the frame
    /// without moving the content.
    extra: usize,
    opacity: f32,
    hidden: bool,
    /// An entity the root reads when it renders, which has the window draw again when it is notified.
    watched: Option<Entity<Status>>,
    log: Log,
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(watched) = &self.watched {
            watched.read(cx);
        }
        div()
            .size_full()
            .children((0..self.extra).map(|index| {
                let log = self.log.clone();
                div()
                    .id(("extra", index))
                    .absolute()
                    .left(px(400.))
                    .top(px(index as f32 * 20.))
                    .size(px(10.))
                    .on_click(move |_, _, _| log.borrow_mut().push(format!("extra{index}")))
            }))
            .child(
                div()
                    .opacity(self.opacity)
                    .when(self.hidden, |content| content.invisible())
                    .child(
                        self.content
                            .clone()
                            .cached(StyleRefinement::default().w(px(300.)).h(px(600.))),
                    ),
            )
    }
}

struct Harness {
    cx: VisualTestContext,
    root: Entity<Root>,
    shell: Option<Entity<Shell>>,
    pane: Entity<Pane>,
    rows: Vec<Entity<Row>>,
    log: Log,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        Self::build(cx, false, |_| {})
    }

    fn with_shell(cx: &mut TestAppContext) -> Self {
        Self::build(cx, true, |_| {})
    }

    fn with_pane(cx: &mut TestAppContext, configure: impl FnOnce(&mut Pane)) -> Self {
        Self::build(cx, false, configure)
    }

    fn build(cx: &mut TestAppContext, shell: bool, configure: impl FnOnce(&mut Pane)) -> Self {
        let log = Log::default();
        let mut parts = None;
        let window = cx.open_window(size(px(800.), px(600.)), |_, cx| {
            let rows: Vec<Entity<Row>> = (0..ROW_COUNT)
                .map(|index| {
                    cx.new(|cx| Row {
                        index,
                        label: 0,
                        renders: 0,
                        focus: cx.focus_handle(),
                        log: log.clone(),
                    })
                })
                .collect();
            let pane = cx.new(|_| {
                let mut pane = Pane {
                    rows: rows.clone(),
                    log: log.clone(),
                    ..Pane::default()
                };
                configure(&mut pane);
                pane
            });
            let shell = shell.then(|| {
                cx.new(|_| Shell {
                    pane: pane.clone(),
                    renders: 0,
                })
            });
            let content = match &shell {
                Some(shell) => AnyView::from(shell.clone()),
                None => AnyView::from(pane.clone()),
            };
            parts = Some((shell, pane, rows));
            Root {
                content,
                extra: 0,
                opacity: 1.,
                hidden: false,
                watched: None,
                log: log.clone(),
            }
        });
        let root = window.root(cx).unwrap();
        let (shell, pane, rows) = parts.unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        Self {
            cx,
            root,
            shell,
            pane,
            rows,
            log,
        }
    }

    fn bump_row(&mut self, row: usize) {
        let row = self.rows[row].clone();
        self.cx.update(|_, cx| {
            row.update(cx, |row, cx| {
                row.label += 1;
                cx.notify();
            })
        });
        self.cx.run_until_parked();
    }

    fn notify_pane(&mut self) {
        let pane = self.pane.clone();
        self.cx.update(|_, cx| pane.update(cx, |_, cx| cx.notify()));
        self.cx.run_until_parked();
    }

    fn update_root(&mut self, update: impl FnOnce(&mut Root)) {
        let root = self.root.clone();
        self.cx.update(|_, cx| {
            root.update(cx, |root, cx| {
                update(root);
                cx.notify();
            })
        });
        self.cx.run_until_parked();
    }

    fn pane_renders(&mut self) -> usize {
        let pane = self.pane.clone();
        self.cx.update(|_, cx| pane.read(cx).renders)
    }

    fn shell_renders(&mut self) -> usize {
        let shell = self.shell.clone().unwrap();
        self.cx.update(|_, cx| shell.read(cx).renders)
    }

    fn row_renders(&mut self) -> Vec<usize> {
        let rows = self.rows.clone();
        self.cx
            .update(|_, cx| rows.iter().map(|row| row.read(cx).renders).collect())
    }

    fn click_row(&mut self, row: usize) {
        let y = HEADER_HEIGHT + row as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.;
        self.cx
            .simulate_click(point(px(20.), px(y)), Modifiers::default());
    }

    fn click_header(&mut self) {
        self.cx
            .simulate_click(point(px(20.), px(HEADER_HEIGHT / 2.)), Modifiers::default());
    }

    fn clicks(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    /// How many callbacks wait for the next frame: each asks the platform for a frame.
    fn frame_requests(&mut self) -> usize {
        self.cx
            .update(|window, _| window.next_frame_callbacks.borrow().len())
    }

    /// What the last frame holds: every paint operation of its scene with those of its retained chunks in their
    /// place, its hitboxes, and how many dispatch nodes and mouse listeners it has.
    fn frame(&mut self) -> (Vec<String>, Vec<Bounds<Pixels>>, usize, usize) {
        self.cx.update(|window, _| {
            let frame = &window.rendered_frame;
            let mut operations = Vec::new();
            scene_operations(&frame.scene, &mut operations);
            (
                operations,
                frame.hitboxes.iter().map(|hitbox| hitbox.bounds).collect(),
                frame.dispatch_tree.len(),
                frame.mouse_listeners.len(),
            )
        })
    }
}

fn scene_operations(scene: &Scene, operations: &mut Vec<String>) {
    for operation in &scene.paint_operations {
        match operation {
            PaintOperation::Primitive(primitive) => {
                let kind = match primitive {
                    Primitive::Shadow(_) => "shadow",
                    Primitive::Quad(_) => "quad",
                    Primitive::Path(_) => "path",
                    Primitive::Underline(_) => "underline",
                    Primitive::MonochromeSprite(_) => "monochrome sprite",
                    Primitive::SubpixelSprite(_) => "subpixel sprite",
                    Primitive::PolychromeSprite(_) => "polychrome sprite",
                    Primitive::Surface(_) => "surface",
                };
                operations.push(format!("{kind} {:?}", primitive.bounds()));
            }
            PaintOperation::MonochromeRun(run) => operations.push(format!(
                "run {:?}",
                run.iter().map(|sprite| sprite.bounds).collect::<Vec<_>>()
            )),
            PaintOperation::Group(group) => operations.push(format!("group {:?}", group.bounds)),
            PaintOperation::RetainedChunk(chunk) => scene_operations(&chunk.scene, operations),
            PaintOperation::StartLayer(bounds) => operations.push(format!("layer {bounds:?}")),
            PaintOperation::EndLayer => operations.push("end layer".into()),
        }
    }
}

#[gpui::test]
fn a_notified_row_renders_alone_and_the_pane_replays_around_it(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let pane_renders = h.pane_renders();
    let row_renders = h.row_renders();
    for _ in 0..3 {
        h.bump_row(0);
    }
    assert_eq!(
        h.pane_renders(),
        pane_renders,
        "the pane did not render for its row"
    );
    let mut expected = row_renders;
    expected[0] += 3;
    assert_eq!(
        h.row_renders(),
        expected,
        "only the notified row rendered, once a notify"
    );
    assert_eq!(h.frame_requests(), 0, "a replay asks for no further frame");

    h.click_row(5);
    h.click_row(0);
    h.click_header();
    h.bump_row(7);
    h.click_row(9);
    h.click_row(7);
    assert_eq!(h.clicks(), ["row5", "row0", "header", "row9", "row7"]);
}

#[gpui::test]
fn a_row_carried_over_inside_its_pane_for_some_frames_is_still_carried_over_in_a_replay(
    cx: &mut TestAppContext,
) {
    let mut h = Harness::with_shell(cx);
    let (shell_renders, pane_renders) = (h.shell_renders(), h.pane_renders());
    let mut expected = h.row_renders();

    // The root renders, and the shell with all inside it is carried over whole.
    h.update_root(|root| root.extra = 1);
    h.update_root(|root| root.extra = 2);
    h.bump_row(2);
    assert_eq!(h.frame_requests(), 0, "a replay asks for no further frame");

    expected[2] += 1;
    assert_eq!(h.row_renders(), expected, "only the notified row rendered");
    assert_eq!(h.shell_renders(), shell_renders);
    assert_eq!(h.pane_renders(), pane_renders);
    h.click_row(7);
    h.click_row(2);
    assert_eq!(h.clicks(), ["row7", "row2"]);
}

#[gpui::test]
fn a_replayed_frame_holds_what_a_rendered_frame_holds(cx: &mut TestAppContext) {
    let mut h = Harness::with_shell(cx);
    let pane_renders = h.pane_renders();
    h.bump_row(3);
    h.bump_row(8);
    assert_eq!(
        h.pane_renders(),
        pane_renders,
        "the frames before were replays"
    );
    let replayed = h.frame();

    h.cx.update(|window, _| window.refresh());
    h.cx.run_until_parked();
    assert_eq!(
        h.pane_renders(),
        pane_renders + 1,
        "a refresh renders the pane"
    );
    assert_eq!(replayed, h.frame());
}

#[gpui::test]
fn three_cached_views_in_each_other_replay_around_the_notified_one(cx: &mut TestAppContext) {
    let mut h = Harness::with_shell(cx);
    let (shell_renders, pane_renders) = (h.shell_renders(), h.pane_renders());
    let row_renders = h.row_renders();

    h.bump_row(4);
    h.bump_row(4);
    assert_eq!(
        h.shell_renders(),
        shell_renders,
        "the shell did not render for a row"
    );
    assert_eq!(
        h.pane_renders(),
        pane_renders,
        "the pane did not render for a row"
    );
    let mut expected = row_renders;
    expected[4] += 2;
    assert_eq!(h.row_renders(), expected);

    h.notify_pane();
    assert_eq!(
        h.shell_renders(),
        shell_renders,
        "the shell did not render for the pane"
    );
    assert_eq!(
        h.pane_renders(),
        pane_renders + 1,
        "the notified pane rendered"
    );
    assert_eq!(h.frame_requests(), 0, "a replay asks for no further frame");

    h.bump_row(1);
    h.click_row(1);
    h.click_row(9);
    h.click_header();
    assert_eq!(h.clicks(), ["row1", "row9", "header"]);
}

#[gpui::test]
fn rows_stay_clickable_after_their_pane_moved_in_the_frame(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.update_root(|root| root.extra = 3);
    h.click_row(9);
    h.click_row(0);
    h.cx.simulate_click(point(px(405.), px(25.)), Modifiers::default());
    h.bump_row(2);
    h.click_row(8);
    h.click_row(2);
    h.update_root(|root| root.extra = 1);
    h.bump_row(6);
    h.click_row(6);
    h.click_header();
    assert_eq!(
        h.clicks(),
        ["row9", "row0", "extra1", "row8", "row2", "row6", "header"]
    );
}

#[gpui::test]
fn a_notified_pane_renders_every_row_it_changed_without_a_notify(cx: &mut TestAppContext) {
    let mut h = Harness::with_pane(cx, |pane| pane.changes_rows_when_it_renders = true);
    let mut expected = h.row_renders();
    h.notify_pane();
    for renders in &mut expected {
        *renders += 1;
    }
    assert_eq!(
        h.row_renders(),
        expected,
        "the pane changed each row as it rendered, so each row rendered"
    );
}

#[gpui::test]
fn a_pane_that_read_the_notified_row_renders(cx: &mut TestAppContext) {
    let mut h = Harness::with_pane(cx, |pane| pane.header_reads_first_row = true);
    let pane_renders = h.pane_renders();
    h.bump_row(5);
    assert_eq!(
        h.pane_renders(),
        pane_renders,
        "the header does not show row 5"
    );
    h.bump_row(0);
    assert_eq!(h.pane_renders(), pane_renders + 1, "the header shows row 0");
}

#[gpui::test]
fn a_notified_uncached_view_in_the_pane_renders_with_the_pane(cx: &mut TestAppContext) {
    let status = cx.new(|_| Status {
        count: 0,
        renders: 0,
    });
    let mut h = Harness::with_pane(cx, |pane| pane.status = Some(status.clone()));
    let renders = h.cx.update(|_, cx| status.read(cx).renders);

    h.bump_row(0);
    let after_row = h.cx.update(|_, cx| status.read(cx).renders);
    assert_eq!(
        after_row, renders,
        "a row's notify leaves the status as drawn"
    );

    h.cx.update(|_, cx| {
        status.update(cx, |status, cx| {
            status.count += 1;
            cx.notify();
        })
    });
    h.cx.run_until_parked();
    let after_status = h.cx.update(|_, cx| status.read(cx).renders);
    assert_eq!(after_status, renders + 1, "the status drew its new count");
}

#[gpui::test]
fn a_notified_view_that_no_render_read_renders_with_the_pane(cx: &mut TestAppContext) {
    let badge = Badge {
        entity: cx.new(|_| Status {
            count: 0,
            renders: 0,
        }),
        renders: Rc::default(),
    };
    let mut h = Harness::with_pane(cx, |pane| pane.badge = Some(badge.clone()));
    h.update_root(|root| root.watched = Some(badge.entity.clone()));
    let renders = badge.renders.get();

    h.cx.update(|_, cx| badge.entity.update(cx, |_, cx| cx.notify()));
    h.cx.run_until_parked();
    assert_eq!(
        badge.renders.get(),
        renders + 1,
        "the notified badge rendered"
    );
}

#[gpui::test]
fn a_pane_with_rows_in_a_hover_group_renders(cx: &mut TestAppContext) {
    let mut h = Harness::with_pane(cx, |pane| pane.rows_in_hover_group = true);
    let pane_renders = h.pane_renders();
    h.bump_row(2);
    assert_eq!(
        h.pane_renders(),
        pane_renders + 1,
        "a replay would paint the row outside the group it reads"
    );
    h.click_row(2);
    assert_eq!(h.clicks(), ["row2"]);
}

#[gpui::test]
fn focus_stays_on_a_clean_row_while_another_row_changes(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let row = h.rows[6].clone();
    let focus = h.cx.update(|window, cx| {
        let focus = row.read(cx).focus.clone();
        focus.focus(window, cx);
        focus
    });
    h.cx.run_until_parked();
    for _ in 0..3 {
        h.bump_row(1);
    }
    let (is_focused, focused) = h.cx.update(|window, cx| {
        (
            focus.is_focused(window),
            window.focused(cx).is_some_and(|focused| focused == focus),
        )
    });
    assert!(is_focused, "row 6 kept the focus");
    assert!(focused, "the frame has row 6 as the focused element");
}

fn retained_quad_alphas(cx: &mut VisualTestContext) -> Vec<f32> {
    fn collect(scene: &Scene, retained: bool, alphas: &mut Vec<f32>) {
        if retained {
            alphas.extend(scene.quads.iter().map(|quad| quad.background.solid.a));
        }
        for chunk in &scene.retained_chunks {
            collect(&chunk.scene, true, alphas);
        }
    }
    cx.update(|window, _| {
        let mut alphas = Vec::new();
        collect(&window.rendered_frame.scene, false, &mut alphas);
        alphas
    })
}

#[gpui::test]
fn a_cached_view_paints_with_the_opacity_around_it_when_that_changes(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    assert_eq!(retained_quad_alphas(&mut h.cx), [1.; ROW_COUNT]);

    h.update_root(|root| root.opacity = 0.5);
    assert_eq!(retained_quad_alphas(&mut h.cx), [0.5; ROW_COUNT]);

    let (pane_renders, row_renders) = (h.pane_renders(), h.row_renders());
    h.update_root(|root| root.extra = 1);
    assert_eq!(
        (h.pane_renders(), h.row_renders()),
        (pane_renders, row_renders),
        "under the same opacity, nothing renders for the root"
    );

    h.bump_row(3);
    assert_eq!(
        retained_quad_alphas(&mut h.cx),
        [0.5; ROW_COUNT],
        "a row that renders inside the replayed pane keeps the opacity around the pane"
    );
}

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

#[gpui::test]
fn a_cached_view_shown_again_after_frames_it_was_hidden_in_takes_clicks(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.update_root(|root| root.hidden = true);
    h.update_root(|root| root.extra = 1);
    assert!(
        retained_quad_alphas(&mut h.cx).is_empty(),
        "nothing of the pane is painted"
    );

    h.update_root(|root| root.hidden = false);
    assert_eq!(
        retained_quad_alphas(&mut h.cx),
        [1.; ROW_COUNT],
        "the pane shows in the first frame it is visible in"
    );
    assert_eq!(h.frame_requests(), 0, "that frame is whole");
    h.click_row(4);
    h.click_header();
    assert_eq!(h.clicks(), ["row4", "header"]);
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
