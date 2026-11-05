//! The GPUI side: toolbar, tabs and panels around the viewport, overlays above
//! it. Each tab keeps its own settings, and only the active tab's surface shows.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{
    AnyElement, Bounds, Context, Div, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Rgba, SharedString, Stateful, Window,
    WindowCompositionSurface, anchored, canvas, deferred, div, point, prelude::*, px, relative, rgb,
    rgba,
};

use crate::camera::Projection;
use crate::render::{
    Cmd, DisplayMode, PartRef, RenderHost, Section, Stats, TabId, TabScene, UiEvent, ViewPreset,
};
use crate::scene::Model;
use crate::viewport::{Viewport, ViewportElement};

const PANEL: u32 = 0x1b1e24;
const PANEL_RAISED: u32 = 0x262a32;
const BORDER: u32 = 0x343943;
const TEXT: u32 = 0xd5d9e0;
const TEXT_DIM: u32 = 0x8b93a1;
const ACCENT: u32 = 0x3b82f6;
const AMBER: u32 = 0xf5a524;
const VIEWPORT_BACKGROUND: u32 = 0x0e1117;

/// Measurements for `--exit-after`.
#[derive(Clone, Copy, Default)]
pub struct RunMetrics {
    pub ui_rebuilds: u64,
    pub worst_ui_stall_ms: f32,
    pub stats: Stats,
    /// When the renderer's first frame was up.
    pub ready_at: Option<Instant>,
}

/// The kinds of content a tab can hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Assembly,
    Part,
    Drawing,
}

impl TabKind {
    fn name(self) -> &'static str {
        match self {
            TabKind::Assembly => "装配体",
            TabKind::Part => "零件",
            TabKind::Drawing => "工程图",
        }
    }
}

struct Tab {
    id: TabId,
    kind: TabKind,
    title: SharedString,
    viewport: Rc<Viewport>,
    model: Arc<Model>,

    mode: DisplayMode,
    projection: Projection,
    spin: bool,
    array: u32,
    section: Section,

    ready: bool,
    stats: Stats,
    hover: Option<(PartRef, f32, f32)>,
    selected: Option<PartRef>,
}

pub struct CadApp {
    host: Rc<RenderHost>,
    assembly_model: Arc<Model>,
    part_model: Arc<Model>,
    focus: FocusHandle,

    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: TabId,
    opened: [u32; 3],
    /// Surfaces of closed tabs, which the window can't remove.
    spare_surfaces: Vec<WindowCompositionSurface>,

    panels_visible: bool,
    failure: Option<SharedString>,
    context_menu: Option<(f32, f32)>,
    dialog_open: bool,

    slider_bounds: Rc<Cell<Bounds<Pixels>>>,
    slider_dragging: bool,

    ui_rebuilds: u64,
    /// How far the UI thread's last tick slipped; what a stall would show.
    ui_stall_ms: Rc<Cell<f32>>,
    /// The worst slip of the current reporting period.
    worst_ui_stall_ms: Rc<Cell<f32>>,
    metrics: Rc<Cell<RunMetrics>>,
}

pub struct CadAppOptions {
    pub array: u32,
    pub spin: bool,
    pub mode: DisplayMode,
    pub metrics: Rc<Cell<RunMetrics>>,
}

impl CadApp {
    pub fn new(
        host: Rc<RenderHost>,
        assembly_model: Arc<Model>,
        part_model: Arc<Model>,
        mut events: UnboundedReceiver<(TabId, UiEvent)>,
        options: CadAppOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some((tab, event)) = events.next().await {
                if this.update(cx, |this, cx| this.on_event(tab, event, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Ticks on the UI thread: a tick that is late measures a stall.
        let ui_stall_ms = Rc::new(Cell::new(0.0));
        let worst_ui_stall_ms = Rc::new(Cell::new(0.0f32));
        let overall_worst = Rc::new(Cell::new(0.0f32));
        cx.spawn({
            let worst = worst_ui_stall_ms.clone();
            let overall = overall_worst.clone();
            let metrics = options.metrics.clone();
            async move |_, cx| {
                const TICK: Duration = Duration::from_millis(50);
                let started = Instant::now();
                loop {
                    let tick = Instant::now();
                    cx.background_executor().timer(TICK).await;
                    let late = tick.elapsed().saturating_sub(TICK).as_secs_f32() * 1000.0;
                    worst.set(worst.get().max(late));
                    // The first second covers startup, not the steady state.
                    if started.elapsed() > Duration::from_secs(1) {
                        overall.set(overall.get().max(late));
                        let mut current = metrics.get();
                        current.worst_ui_stall_ms = overall.get();
                        metrics.set(current);
                    }
                }
            }
        })
        .detach();

        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut app = Self {
            host,
            assembly_model,
            part_model,
            focus,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            opened: [0; 3],
            spare_surfaces: Vec::new(),
            panels_visible: true,
            failure: None,
            context_menu: None,
            dialog_open: false,
            slider_bounds: Rc::new(Cell::new(Bounds::default())),
            slider_dragging: false,
            ui_rebuilds: 0,
            ui_stall_ms,
            worst_ui_stall_ms,
            metrics: options.metrics,
        };

        // One tab of each kind to start with. Only the first is shown; the
        // others draw nothing until they are first looked at.
        let mut first = TabScene::view_3d(app.assembly_model.clone());
        first.array = options.array;
        first.spin = options.spin;
        first.mode = options.mode;
        app.open_tab(TabKind::Assembly, first, window);
        for kind in [TabKind::Part, TabKind::Drawing] {
            let scene = app.scene_for(kind);
            app.open_tab(kind, scene, window);
        }
        app.show_only_active();
        app
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn scene_for(&self, kind: TabKind) -> TabScene {
        match kind {
            TabKind::Assembly => TabScene::view_3d(self.assembly_model.clone()),
            TabKind::Part => TabScene::view_3d(self.part_model.clone()),
            TabKind::Drawing => TabScene::drawing(self.assembly_model.clone()),
        }
    }

    /// Creates a tab and its native surface, which works at any time.
    fn open_tab(&mut self, kind: TabKind, scene: TabScene, window: &Window) -> bool {
        let id = self.next_tab_id;
        let model = scene.model.clone();
        let (mode, projection, spin, array) = (scene.mode, scene.projection, scene.spin, scene.array);
        let spare = self.spare_surfaces.pop();
        match Viewport::new(window, self.host.clone(), id, scene, spare) {
            Ok(viewport) => {
                self.next_tab_id += 1;
                let count = &mut self.opened[kind as usize];
                *count += 1;
                self.tabs.push(Tab {
                    id,
                    kind,
                    title: format!("{} {count}", kind.name()).into(),
                    viewport,
                    model,
                    mode,
                    projection,
                    spin,
                    array,
                    section: Section {
                        axis: None,
                        position: 0.5,
                    },
                    ready: false,
                    stats: Stats::default(),
                    hover: None,
                    selected: None,
                });
                true
            }
            Err(error) => {
                eprintln!("opening a tab: {error:#}");
                self.failure = Some(format!("无法打开新标签页：{error:#}").into());
                false
            }
        }
    }

    fn add_tab(&mut self, kind: TabKind, window: &mut Window, cx: &mut Context<Self>) {
        let scene = self.scene_for(kind);
        if self.open_tab(kind, scene, window) {
            self.activate(self.tabs.len() - 1, cx);
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let closed = self.tabs.remove(index);
        self.spare_surfaces.push(closed.viewport.close());
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = self.active.min(self.tabs.len() - 1);
            self.tabs[self.active].viewport.set_visible(true);
        }
        self.context_menu = None;
        cx.notify();
    }

    /// Switches tabs: shows the new surface first, then hides the old one, so
    /// nothing but one of them is ever on screen.
    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index != self.active {
            self.tabs[index].viewport.set_visible(true);
            self.tabs[self.active].viewport.set_visible(false);
            self.tabs[self.active].hover = None;
            self.active = index;
        }
        self.context_menu = None;
        self.slider_dragging = false;
        cx.notify();
    }

    fn show_only_active(&self) {
        for (index, tab) in self.tabs.iter().enumerate() {
            if index != self.active {
                tab.viewport.set_visible(false);
            }
        }
    }

    fn on_event(&mut self, id: TabId, event: UiEvent, cx: &mut Context<Self>) {
        if let UiEvent::Failed(message) = event {
            self.failure = Some(message.into());
            cx.notify();
            return;
        }
        let active_id = self.tab().id;
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            // A tab closed while its events were on their way.
            return;
        };
        match event {
            UiEvent::Failed(_) => unreachable!("handled above"),
            UiEvent::Ready => {
                tab.ready = true;
                let mut metrics = self.metrics.get();
                metrics.ready_at.get_or_insert_with(Instant::now);
                self.metrics.set(metrics);
            }
            UiEvent::Stats(stats) => {
                tab.stats = stats;
                if id == active_id {
                    self.ui_stall_ms.set(self.worst_ui_stall_ms.replace(0.0));
                    let mut metrics = self.metrics.get();
                    metrics.stats = stats;
                    self.metrics.set(metrics);
                }
            }
            UiEvent::Hover {
                part,
                window_x,
                window_y,
            } => {
                let hover = part.map(|part| (part, window_x, window_y));
                if hover == tab.hover {
                    return;
                }
                tab.hover = hover;
            }
            UiEvent::Selected(part) => tab.selected = part,
            UiEvent::CommitComposition => {
                tab.viewport.commit_composition();
                return;
            }
            UiEvent::ContextMenu { window_x, window_y } => {
                self.context_menu = Some((window_x, window_y));
            }
            UiEvent::ViewportPressed => {
                if self.context_menu.take().is_none() {
                    return;
                }
            }
        }
        cx.notify();
    }

    fn set_mode(&mut self, mode: DisplayMode, cx: &mut Context<Self>) {
        self.tab_mut().mode = mode;
        self.tab().viewport.send(Cmd::SetMode(mode));
        cx.notify();
    }

    fn set_projection(&mut self, projection: Projection, cx: &mut Context<Self>) {
        self.tab_mut().projection = projection;
        self.tab().viewport.send(Cmd::SetProjection(projection));
        cx.notify();
    }

    fn set_spin(&mut self, spin: bool, cx: &mut Context<Self>) {
        self.tab_mut().spin = spin;
        self.tab().viewport.send(Cmd::SetSpin(spin));
        cx.notify();
    }

    fn set_array(&mut self, array: u32, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.array = array;
        tab.selected = None;
        tab.viewport.send(Cmd::SelectPart(None));
        tab.viewport.send(Cmd::SetArray(array));
        cx.notify();
    }

    fn set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.tab_mut().section = section;
        self.tab().viewport.send(Cmd::SetSection(section));
        cx.notify();
    }

    fn set_view(&mut self, preset: ViewPreset) {
        self.tab().viewport.send(Cmd::SetView(preset));
    }

    fn select_part(&mut self, part: usize, cx: &mut Context<Self>) {
        self.tab_mut().selected = Some(PartRef { assembly: 0, part });
        self.tab().viewport.send(Cmd::SelectPart(Some(part)));
        cx.notify();
    }

    fn drag_slider(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let bounds = self.slider_bounds.get();
        let width = f32::from(bounds.size.width).max(1.0);
        let position = (f32::from(x - bounds.origin.x) / width).clamp(0.0, 1.0);
        self.set_section(
            Section {
                axis: self.tab().section.axis,
                position,
            },
            cx,
        );
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "f" => self.tab().viewport.send(Cmd::Fit),
            "1" => self.set_view(ViewPreset::Iso),
            "2" => self.set_view(ViewPreset::Top),
            "3" => self.set_view(ViewPreset::Front),
            "4" => self.set_view(ViewPreset::Right),
            "o" => {
                let next = match self.tab().projection {
                    Projection::Perspective => Projection::Orthographic,
                    Projection::Orthographic => Projection::Perspective,
                };
                self.set_projection(next, cx);
            }
            "space" => self.set_spin(!self.tab().spin, cx),
            "tab" => {
                let next = (self.active + 1) % self.tabs.len();
                self.activate(next, cx);
            }
            "escape" => {
                self.context_menu = None;
                self.dialog_open = false;
                cx.notify();
            }
            _ => {}
        }
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let tab = self.tab();
        let view = |id: &'static str, label: &'static str, preset: ViewPreset, cx: &mut Context<Self>| {
            tool_button(id, label, false).on_click(cx.listener(move |this, _, _, _| this.set_view(preset)))
        };
        let mode = |id: &'static str, label: &'static str, mode: DisplayMode, tab: &Tab, cx: &mut Context<Self>| {
            tool_button(id, label, tab.mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
        };
        let projection = |id: &'static str, label: &'static str, value: Projection, tab: &Tab, cx: &mut Context<Self>| {
            tool_button(id, label, tab.projection == value)
                .on_click(cx.listener(move |this, _, _, cx| this.set_projection(value, cx)))
        };
        let array = |id: &'static str, label: &'static str, value: u32, tab: &Tab, cx: &mut Context<Self>| {
            tool_button(id, label, tab.array == value)
                .on_click(cx.listener(move |this, _, _, cx| this.set_array(value, cx)))
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .h(px(44.))
            .px_3()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(tool_group(
                "视图",
                vec![
                    view("view-iso", "等轴", ViewPreset::Iso, cx).into_any_element(),
                    view("view-top", "俯视", ViewPreset::Top, cx).into_any_element(),
                    view("view-front", "前视", ViewPreset::Front, cx).into_any_element(),
                    view("view-right", "右视", ViewPreset::Right, cx).into_any_element(),
                    tool_button("view-fit", "适配", false)
                        .on_click(cx.listener(|this, _, _, _| this.tab().viewport.send(Cmd::Fit)))
                        .into_any_element(),
                ],
            ))
            .child(tool_group(
                "显示",
                vec![
                    mode("mode-shaded", "着色", DisplayMode::Shaded, tab, cx).into_any_element(),
                    mode("mode-edges", "着色+边线", DisplayMode::ShadedEdges, tab, cx).into_any_element(),
                    mode("mode-hidden", "消隐线", DisplayMode::HiddenLine, tab, cx).into_any_element(),
                ],
            ))
            .child(tool_group(
                "投影",
                vec![
                    projection("proj-persp", "透视", Projection::Perspective, tab, cx).into_any_element(),
                    projection("proj-ortho", "正交", Projection::Orthographic, tab, cx).into_any_element(),
                ],
            ))
            .child(tool_group(
                "阵列",
                vec![
                    array("array-1", "1", 1, tab, cx).into_any_element(),
                    array("array-3", "3²", 3, tab, cx).into_any_element(),
                    array("array-10", "10²", 10, tab, cx).into_any_element(),
                    array("array-30", "30²", 30, tab, cx).into_any_element(),
                ],
            ))
            .child(
                tool_button("spin", "运转", tab.spin)
                    .on_click(cx.listener(|this, _, _, cx| this.set_spin(!this.tab().spin, cx))),
            )
            .child(div().flex_1())
            .child(
                tool_button("panels", "面板", self.panels_visible).on_click(cx.listener(|this, _, _, cx| {
                    this.panels_visible = !this.panels_visible;
                    cx.notify();
                })),
            )
            .child(tool_button("about", "层级说明", self.dialog_open).on_click(cx.listener(
                |this, _, _, cx| {
                    this.dialog_open = true;
                    cx.notify();
                },
            )))
    }

    /// The document tabs over the viewport, painted by GPUI as usual.
    fn tab_bar(&self, cx: &mut Context<Self>) -> Div {
        let can_close = self.tabs.len() > 1;
        let mut bar = div()
            .flex()
            .flex_none()
            .items_end()
            .gap_px()
            .h(px(32.))
            .px_1()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER));
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            bar = bar.child(
                div()
                    .id(("tab", tab.id as usize))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .pl_3()
                    .pr_2()
                    .rounded_t_md()
                    .border_t_2()
                    .border_color(rgb(if active { ACCENT } else { PANEL_RAISED }))
                    .bg(rgb(if active { VIEWPORT_BACKGROUND } else { PANEL_RAISED }))
                    .text_xs()
                    .text_color(rgb(if active { 0xffffff } else { TEXT_DIM }))
                    .cursor_pointer()
                    .when(!active, |tab| tab.hover(|style| style.bg(rgb(0x323742))))
                    .on_click(cx.listener(move |this, _, _, cx| this.activate(index, cx)))
                    .child(tab.title.clone())
                    .when(can_close, |row| {
                        row.child(
                            div()
                                .id(("close-tab", tab.id as usize))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(16.))
                                .rounded_sm()
                                .text_color(rgb(TEXT_DIM))
                                .hover(|style| style.bg(rgb(0x4a4f5a)).text_color(rgb(0xffffff)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(index, cx);
                                }))
                                .child("×"),
                        )
                    }),
            );
        }
        let add = |id: &'static str, label: &'static str, kind: TabKind, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(24.))
                .mb(px(2.))
                .px_2()
                .rounded_md()
                .text_xs()
                .text_color(rgb(TEXT_DIM))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(PANEL_RAISED)).text_color(rgb(0xffffff)))
                .on_click(cx.listener(move |this, _, window, cx| this.add_tab(kind, window, cx)))
                .child(label)
        };
        bar.child(div().flex_1())
            .child(add("add-assembly", "+ 装配体", TabKind::Assembly, cx))
            .child(add("add-part", "+ 零件", TabKind::Part, cx))
            .child(add("add-drawing", "+ 工程图", TabKind::Drawing, cx))
    }

    fn tree_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let tab = self.tab();
        let selected = tab.selected.map(|part| part.part);
        let hovered = tab.hover.map(|(part, _, _)| part.part);
        let mut rows = div().flex().flex_col();
        for (index, part) in tab.model.parts.iter().enumerate() {
            let is_selected = selected == Some(index);
            let is_hovered = hovered == Some(index);
            rows = rows.child(
                div()
                    .id(("part", index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py(px(3.))
                    .text_xs()
                    .cursor_pointer()
                    .text_color(rgb(if is_selected { 0xffffff } else { TEXT }))
                    .when(is_selected, |row| row.bg(rgb(ACCENT)))
                    .when(is_hovered && !is_selected, |row| row.bg(rgb(0x4a3a18)))
                    .hover(|row| row.bg(rgb(PANEL_RAISED)))
                    .child(part.name.clone())
                    .child(
                        div()
                            .text_color(rgb(if is_selected { 0xdbeafe } else { TEXT_DIM }))
                            .child(format_count(u64::from(tab.model.part_triangles(index)))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_part(index, cx))),
            );
        }
        div()
            .id("tree")
            .flex_none()
            .w(px(214.))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(panel_title("装配树"))
            .child(rows)
    }

    fn properties_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let tab = self.tab();
        let selected = tab.selected.map(|reference| {
            let part = &tab.model.parts[reference.part];
            (reference, part.name.clone(), tab.model.part_triangles(reference.part))
        });
        let section_button = |id: &'static str, label: &'static str, axis: Option<usize>, tab: &Tab, cx: &mut Context<Self>| {
            tool_button(id, label, tab.section.axis == axis).on_click(cx.listener(move |this, _, _, cx| {
                this.set_section(
                    Section {
                        axis,
                        position: this.tab().section.position,
                    },
                    cx,
                );
            }))
        };

        let stats = &tab.stats;
        let fps_label = if stats.fps < 0.5 {
            "空闲 (0)".to_string()
        } else {
            format!("{:.0}", stats.fps)
        };

        // Each tab's frame count: only the shown tab's should be moving.
        let mut frames = div().px_3().pb_3();
        for (index, other) in self.tabs.iter().enumerate() {
            let state = if index == self.active { "显示中" } else { "已隐藏" };
            frames = frames.child(property_owned(
                other.title.clone(),
                format!("{} 帧 · {state}", other.stats.frames_total),
            ));
        }

        div()
            .id("properties")
            .flex_none()
            .w(px(248.))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(PANEL))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(panel_title("属性"))
            .child(match selected {
                Some((reference, name, triangles)) => div()
                    .px_3()
                    .pb_3()
                    .child(property("零件", name))
                    .child(property("所在装配", format!("#{}", reference.assembly + 1)))
                    .child(property("三角形", format_count(u64::from(triangles)))),
                None => div()
                    .px_3()
                    .pb_3()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("在视口或装配树里点选零件"),
            })
            .child(panel_title("剖切"))
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(section_button("section-off", "关", None, tab, cx))
                            .child(section_button("section-x", "X", Some(0), tab, cx))
                            .child(section_button("section-y", "Y", Some(1), tab, cx))
                            .child(section_button("section-z", "Z", Some(2), tab, cx)),
                    )
                    .child(self.slider(cx)),
            )
            .child(panel_title("当前标签页 · wgpu"))
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .child(property("帧率 (FPS)", fps_label))
                    .child(property("单帧编码", format!("{:.2} ms", stats.encode_ms)))
                    .child(property("三角形", format_count(stats.triangles)))
                    .child(property("实例 / Draw", format!("{} / {}", stats.instances, stats.draw_calls)))
                    .child(property("分辨率", format!("{}×{} MSAA 4×", stats.width, stats.height)))
                    .child(property("适配器", self.host.adapter.clone())),
            )
            .child(panel_title("各标签页累计帧数"))
            .child(frames)
            .child(panel_title("GPUI UI 线程"))
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .child(property("界面重建次数", self.ui_rebuilds.to_string()))
                    .child(property("最大卡顿", format!("{:.1} ms", self.ui_stall_ms.get()))),
            )
    }

    fn slider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.slider_bounds.clone();
        let section = self.tab().section;
        let position = section.position;
        let enabled = section.axis.is_some();
        div()
            .id("section-slider")
            .relative()
            .h(px(18.))
            .w_full()
            .flex()
            .items_center()
            .cursor_pointer()
            .child(
                canvas(move |element_bounds, _, _| bounds.set(element_bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(rgb(PANEL_RAISED))
                    .child(
                        div()
                            .h_full()
                            .w(relative(position))
                            .rounded_full()
                            .bg(rgb(if enabled { ACCENT } else { TEXT_DIM })),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left(relative(position))
                    .ml(px(-6.))
                    .size(px(12.))
                    .rounded_full()
                    .bg(rgb(if enabled { 0xffffff } else { TEXT_DIM })),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.slider_dragging = true;
                    this.drag_slider(event.position.x, cx);
                }),
            )
    }

    /// Overlays over the viewport, painted by GPUI on the surface above it.
    fn overlays(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays: Vec<AnyElement> = Vec::new();
        let tab = self.tab();

        let stats = &tab.stats;
        let fps = if stats.fps < 0.5 {
            "按需渲染 · 空闲".to_string()
        } else {
            format!("{:.0} FPS · {:.2} ms", stats.fps, stats.encode_ms)
        };
        overlays.push(
            deferred(
                div()
                    .absolute()
                    .top_3()
                    .left_3()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgba(0xffffff26))
                    .bg(rgba(0x0b0e14b8))
                    .text_xs()
                    .text_color(rgb(TEXT))
                    .child(
                        div()
                            .text_color(rgb(AMBER))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(format!("{} · GPUI HUD 叠在 wgpu 上", tab.title)),
                    )
                    .child(div().mt_1().child(fps))
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .child(format!("{} 三角形", format_count(stats.triangles))),
                    ),
            )
            .priority(1)
            .into_any_element(),
        );

        if let Some(failure) = &self.failure {
            overlays.push(
                deferred(
                    div()
                        .absolute()
                        .bottom_3()
                        .left_3()
                        .right_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgba(0x7f1d1de6))
                        .text_xs()
                        .text_color(rgb(0xffffff))
                        .child(failure.clone()),
                )
                .priority(1)
                .into_any_element(),
            );
        }

        if let Some((part, x, y)) = tab.hover
            && self.context_menu.is_none()
        {
            let name = tab.model.parts[part.part].name.clone();
            overlays.push(
                deferred(
                    anchored().position(point(px(x + 14.), px(y + 16.))).child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(0x0b0e14e6))
                            .border_1()
                            .border_color(rgb(AMBER))
                            .text_xs()
                            .text_color(rgb(0xffffff))
                            .child(name),
                    ),
                )
                .priority(2)
                .into_any_element(),
            );
        }

        if let Some((x, y)) = self.context_menu {
            let item = |id: &'static str, label: &'static str| {
                div()
                    .id(id)
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .text_xs()
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|row| row.bg(rgb(ACCENT)).text_color(rgb(0xffffff)))
                    .child(label)
            };
            let close = |this: &mut Self, cx: &mut Context<Self>| {
                this.context_menu = None;
                cx.notify();
            };
            overlays.push(
                deferred(
                    anchored().position(point(px(x), px(y))).snap_to_window().child(
                        div()
                            .id("context-menu")
                            .occlude()
                            .w(px(170.))
                            .p_1()
                            .rounded_lg()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .shadow_xl()
                            .child(item("menu-fit", "适配全部  F").on_click(cx.listener(move |this, _, _, cx| {
                                this.tab().viewport.send(Cmd::Fit);
                                close(this, cx);
                            })))
                            .child(item("menu-iso", "等轴视图  1").on_click(cx.listener(move |this, _, _, cx| {
                                this.set_view(ViewPreset::Iso);
                                close(this, cx);
                            })))
                            .child(item("menu-clear", "清除选择").on_click(cx.listener(move |this, _, _, cx| {
                                this.tab_mut().selected = None;
                                this.tab().viewport.send(Cmd::SelectPart(None));
                                close(this, cx);
                            })))
                            .child(div().my_1().h(px(1.)).bg(rgb(BORDER)))
                            .child(item("menu-about", "层级说明…").on_click(cx.listener(move |this, _, _, cx| {
                                this.dialog_open = true;
                                close(this, cx);
                            }))),
                    ),
                )
                .priority(3)
                .into_any_element(),
            );
        }
        overlays
    }

    fn dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let layer = |index: &'static str, name: &'static str, detail: &'static str, color: Rgba| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(div().w(px(4.)).h(px(30.)).rounded_sm().bg(color))
                .child(div().w(px(18.)).text_xs().text_color(rgb(TEXT_DIM)).child(index))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().text_color(rgb(TEXT)).child(name))
                        .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(detail)),
                )
        };
        deferred(
            div()
                .id("dialog-scrim")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x05070b99))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.dialog_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("dialog")
                        .occlude()
                        .w(px(520.))
                        .p_5()
                        .rounded_xl()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_xl()
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff))
                                .child("一个窗口，多层表面"),
                        )
                        .child(
                            div()
                                .mt_2()
                                .mb_2()
                                .text_sm()
                                .text_color(rgb(TEXT_DIM))
                                .child("这个对话框由 GPUI 绘制，后面的视口仍在由 wgpu 实时渲染。开着它按空格键试试。"),
                        )
                        .child(layer("03", "GPUI overlay", "deferred 绘制：HUD、菜单、这个对话框", rgb(AMBER).into()))
                        .child(layer(
                            "02",
                            "wgpu 交换链（每个标签页一条）",
                            "同一个 DX12 设备和渲染线程，只显示当前标签页",
                            rgb(ACCENT).into(),
                        ))
                        .child(layer("01", "GPUI base", "根视图：工具栏、标签栏、面板（D3D11）", rgb(TEXT_DIM).into()))
                        .child(
                            div().mt_4().flex().justify_end().child(
                                tool_button("dialog-close", "关闭", true).on_click(cx.listener(|this, _, _, cx| {
                                    this.dialog_open = false;
                                    cx.notify();
                                })),
                            ),
                        ),
                ),
        )
        .priority(4)
    }
}

impl Render for CadApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ui_rebuilds += 1;
        let mut metrics = self.metrics.get();
        metrics.ui_rebuilds = self.ui_rebuilds;
        self.metrics.set(metrics);

        // Only the active tab's element is drawn, so only its surface follows
        // the layout; the others catch up when they are next shown.
        let mut viewport_area = div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(rgb(VIEWPORT_BACKGROUND))
            .child(ViewportElement {
                viewport: self.tab().viewport.clone(),
            });
        for overlay in self.overlays(cx) {
            viewport_area = viewport_area.child(overlay);
        }
        let center = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.tab_bar(cx))
            .child(viewport_area);

        let toolbar = self.toolbar(cx);
        let hints = if self.tab().kind == TabKind::Drawing {
            "工程图：拖动 平移 · 滚轮 缩放 · 单击 选择 · 右键单击 菜单 · F 适配 · Tab 切换标签页"
        } else {
            "左键/右键拖动 旋转 · 中键/Shift+左键 平移 · 滚轮 缩放 · 单击 选择 · 右键单击 菜单 · F 适配 · O 投影 · 空格 运转 · Tab 切换标签页"
        };
        let mut body = div().flex().flex_1().min_h_0();
        if self.panels_visible {
            body = body.child(self.tree_panel(cx));
        }
        body = body.child(center);
        if self.panels_visible {
            body = body.child(self.properties_panel(cx));
        }

        div()
            .id("root")
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.slider_dragging {
                    this.drag_slider(event.position.x, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.slider_dragging = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.context_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .size_full()
            .font_family("Microsoft YaHei UI")
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .child(toolbar)
            .child(body)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(24.))
                    .px_3()
                    .bg(rgb(PANEL))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(hints),
            )
            .when(self.dialog_open, |root| root.child(self.dialog(cx)))
    }
}

fn tool_button(id: &'static str, label: &'static str, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px_2()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .text_color(rgb(if active { 0xffffff } else { TEXT }))
        .bg(rgb(if active { ACCENT } else { PANEL_RAISED }))
        .hover(move |style| style.bg(rgb(if active { 0x5b97f8 } else { 0x323742 })))
        .child(label)
}

fn tool_group(label: &'static str, buttons: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().mr_1().text_xs().text_color(rgb(TEXT_DIM)).child(label))
        .children(buttons)
}

fn panel_title(title: &'static str) -> Div {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT_DIM))
        .child(title)
}

fn property(label: &'static str, value: impl Into<SharedString>) -> Div {
    property_owned(label.into(), value)
}

fn property_owned(label: SharedString, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .justify_between()
        .gap_2()
        .py(px(2.))
        .text_xs()
        .child(div().flex_none().text_color(rgb(TEXT_DIM)).child(label))
        .child(div().text_color(rgb(TEXT)).child(value.into()))
}

fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1} M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1} K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}
