//! The custom GPUI `Element` in a browser, where wgpu really does draw from it.
//!
//! The desktop viewport (`viewport.rs`) has to stack a swap chain of its own
//! under GPUI's overlays. A page has no window composition and an opaque canvas,
//! but GPUI's renderer here is wgpu itself, so the element renders into a
//! `WgpuDynamicTexture` on GPUI's own device during paint, and paints it as a
//! sprite. It takes part in GPUI's painter order like any other element, so
//! anything may draw over it, and clips and opacity apply.
//!
//! This offers the same interface as the desktop viewport, so `ui.rs` runs on it
//! unchanged.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, AppContext as _, Bounds, DevicePixels, DispatchPhase, Element, ElementId, GlobalElementId,
    Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollEvent, Style, TitlebarOptions,
    Window, WindowBounds, WindowOptions, px, relative, size,
};
use gpui_wgpu::{WgpuContext, WgpuDynamicTexture};

use crate::render::{
    Button, Cmd, DisplayMode, Pointer, PointerKind, RenderHost, TabId, TabRenderer, TabScene,
    UiEvent,
};
use crate::ui::{CadApp, CadAppOptions, RunMetrics};

/// Closed tabs leave nothing behind here.
pub type SpareSurface = ();

/// One tab's viewport: its renderer, and the texture it draws into.
pub struct Viewport {
    id: TabId,
    renderer: RefCell<TabRenderer>,
    context: WgpuContext,
    /// The texture, and the size it was allocated at.
    texture: RefCell<Option<(WgpuDynamicTexture, (u32, u32))>>,
    /// Where the element was last laid out, and at what scale factor.
    geometry: Cell<(Bounds<Pixels>, f32)>,
    redraw: UnboundedSender<(TabId, UiEvent)>,
}

impl Viewport {
    pub fn new(
        window: &Window,
        host: Rc<RenderHost>,
        id: TabId,
        scene: TabScene,
        _spare: Option<SpareSurface>,
    ) -> Result<Rc<Self>> {
        Ok(Rc::new(Self {
            id,
            renderer: RefCell::new(host.new_tab(id, scene)),
            context: host.context.clone(),
            texture: RefCell::new(None),
            geometry: Cell::new((Bounds::default(), window.scale_factor())),
            redraw: host.ui_sender(),
        }))
    }

    /// Changes something, and asks GPUI to paint again if that changed the view.
    pub fn send(&self, cmd: Cmd) {
        let mut renderer = self.renderer.borrow_mut();
        renderer.apply(cmd);
        if renderer.wants_frame() {
            self.redraw.unbounded_send((self.id, UiEvent::Redraw)).ok();
        }
    }

    pub fn set_visible(&self, visible: bool) {
        self.send(Cmd::SetVisible(visible));
    }

    pub fn close(&self) -> SpareSurface {}

    /// There is no composition to commit: GPUI presents the sprite itself.
    pub fn commit_composition(&self) {}

    /// Passes a pointer event on, and paints again if it changed the view.
    fn pointer(&self, kind: PointerKind, position: Point<Pixels>, shift: bool, window: &mut Window) {
        let (bounds, scale) = self.geometry.get();
        let pointer = Pointer {
            kind,
            x: f32::from(position.x - bounds.origin.x) * scale,
            y: f32::from(position.y - bounds.origin.y) * scale,
            window_x: f32::from(position.x),
            window_y: f32::from(position.y),
            shift,
        };
        let mut renderer = self.renderer.borrow_mut();
        renderer.apply(Cmd::Pointer(pointer));
        if renderer.wants_frame() {
            window.refresh();
        }
    }

    /// Renders if anything changed, and paints the texture.
    fn draw(&self, bounds: Bounds<Pixels>, window: &mut Window) -> Result<()> {
        let scale = window.scale_factor();
        self.geometry.set((bounds, scale));
        let device: Bounds<DevicePixels> = bounds.to_device_pixels(scale);
        let target = (device.size.width.0.max(1) as u32, device.size.height.0.max(1) as u32);

        let mut slot = self.texture.borrow_mut();
        let mut renderer = self.renderer.borrow_mut();
        if slot.as_ref().is_none_or(|(_, allocated)| *allocated != target) {
            let texture = self.context.create_dynamic_texture(size(
                DevicePixels(target.0 as i32),
                DevicePixels(target.1 as i32),
            ))?;
            *slot = Some((texture, target));
            renderer.resize(target.0, target.1);
        }
        let (texture, _) = slot.as_ref().expect("allocated above");
        if renderer.wants_frame() {
            texture.render(window, |view| {
                renderer.draw(view);
                Ok(())
            })?;
        }
        texture.paint(window, bounds)?;
        if renderer.wants_frame() {
            window.request_animation_frame();
        }
        Ok(())
    }
}

fn pointer_button(button: MouseButton) -> Option<Button> {
    match button {
        MouseButton::Left => Some(Button::Left),
        MouseButton::Middle => Some(Button::Middle),
        MouseButton::Right => Some(Button::Right),
        MouseButton::Navigate(_) => None,
    }
}

pub struct ViewportElement {
    pub viewport: Rc<Viewport>,
}

impl IntoElement for ViewportElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ViewportElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        if let Err(error) = self.viewport.draw(bounds, window) {
            log_error(&format!("drawing the viewport: {error:#}"));
        }

        // Input through ordinary GPUI listeners: whatever is painted above the
        // viewport gets the mouse first, as for any other element.
        window.on_mouse_event({
            let (viewport, hitbox) = (self.viewport.clone(), hitbox.clone());
            move |event: &MouseDownEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble
                    && hitbox.is_hovered(window)
                    && let Some(button) = pointer_button(event.button)
                {
                    window.capture_pointer(hitbox.id);
                    viewport.pointer(PointerKind::Down(button), event.position, event.modifiers.shift, window);
                }
            }
        });
        window.on_mouse_event({
            let (viewport, hitbox) = (self.viewport.clone(), hitbox.clone());
            move |event: &MouseUpEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble
                    && hitbox.is_hovered(window)
                    && let Some(button) = pointer_button(event.button)
                {
                    viewport.pointer(PointerKind::Up(button), event.position, event.modifiers.shift, window);
                }
            }
        });
        window.on_mouse_event({
            let (viewport, hitbox) = (self.viewport.clone(), hitbox.clone());
            move |event: &MouseMoveEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    viewport.pointer(PointerKind::Move, event.position, event.modifiers.shift, window);
                }
            }
        });
        window.on_mouse_event({
            let (viewport, hitbox) = (self.viewport.clone(), hitbox.clone());
            move |event: &ScrollEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    // A browser reports a wheel notch as about 100 pixels, and
                    // the renderer expects about three lines for one.
                    let lines = match event.delta {
                        ScrollDelta::Lines(lines) => lines.y,
                        ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 33.0,
                    };
                    viewport.pointer(PointerKind::Scroll(lines), event.position, event.modifiers.shift, window);
                }
            }
        });
    }
}

fn log_error(message: &str) {
    web_sys_console(message);
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn web_sys_console(message: &str);
}

pub fn run() {
    gpui_platform::web_init();
    // Kept as a concrete type: the application only sees `dyn Platform`, and the
    // context is needed from it once graphics are up.
    let platform = Rc::new(gpui_web::WebPlatform::new_with_backend_and_font_fallback(
        false,
        gpui_web::WebBackendPreference::Auto,
        // The labels are Chinese, and the page only loads a Latin font.
        gpui_web::CanvasFontFallback::EmojiAndCjk,
    ));
    gpui::Application::with_platform(platform.clone()).run(move |cx: &mut App| {
        let fonts = vec![
            std::borrow::Cow::Borrowed(
                include_bytes!("../vendor/gpui-fast/assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf")
                    .as_slice(),
            ),
            std::borrow::Cow::Borrowed(
                include_bytes!("../vendor/gpui-fast/assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf")
                    .as_slice(),
            ),
        ];
        if let Err(error) = cx.text_system().add_fonts(fonts) {
            log_error(&format!("loading fonts: {error:#}"));
            return;
        }

        let Some(context) = platform.wgpu_context() else {
            log_error("graphics finished launching without a wgpu context");
            return;
        };
        let (ui_sender, ui_receiver) = futures::channel::mpsc::unbounded();
        let host = Rc::new(RenderHost::new(context, ui_sender));
        let model = Arc::new(crate::scene::gearbox());
        let flange = Arc::new(crate::scene::flange());
        let metrics = Rc::new(Cell::new(RunMetrics::default()));
        let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("GPUI + wgpu · CAD 视口演示".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    CadApp::new(
                        host,
                        model,
                        flange,
                        ui_receiver,
                        CadAppOptions {
                            array: 1,
                            spin: false,
                            mode: DisplayMode::ShadedEdges,
                            metrics,
                        },
                        window,
                        cx,
                    )
                })
            },
        );
        if let Err(error) = opened {
            log_error(&format!("opening the window: {error:#}"));
        }
        cx.activate(true);
    });
}
