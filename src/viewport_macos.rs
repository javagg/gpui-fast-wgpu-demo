//! The custom GPUI `Element` whose content wgpu renders directly, on macOS.
//!
//! gpui-fast's macOS backend gives a native surface as a layer-backed `NSView`
//! whose pointer `platform_handle` returns as a `usize`. This element puts a
//! `CAMetalLayer` on that view and hands it to wgpu, which renders into it on
//! its own device and thread:
//!
//! ```text
//! front  GPUI overlay    deferred draws: HUD, menus, dialogs
//!        CAMetalLayer    this viewport, on the Metal device of the render thread
//! back   GPUI base       the root view, on the window's own CAMetalLayer
//! ```
//!
//! Unlike Windows there is no composition device to commit: a `CAMetalLayer`
//! presents by itself, and `set_visible` works, so hiding a tab needs no
//! workaround.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::mpsc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, Bounds, DevicePixels, DispatchEventResult, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, MouseButton, Pixels, PlatformInput, ScrollDelta,
    Style, Window, WindowCompositionSurface, relative,
};
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};

use crate::render::{Button, Cmd, Msg, Pointer, PointerKind, RenderHost, TabId, TabScene};

/// What a closed tab leaves behind for the next one to use.
pub type SpareSurface = WindowCompositionSurface;

/// One tab's viewport: its native surface in the window and its renderer on
/// the render thread. Every tab has its own, on the one wgpu device.
pub struct Viewport {
    host: Rc<RenderHost>,
    id: TabId,
    /// Where the element was last laid out, and at what scale factor.
    geometry: Rc<Cell<(Bounds<Pixels>, f32)>>,
    size: Cell<(u32, u32)>,
    surface: WindowCompositionSurface,
}

impl Viewport {
    /// Can be called at any time, not only while opening the window. `spare`
    /// is the surface of a closed tab to use instead of creating another.
    pub fn new(
        window: &Window,
        host: Rc<RenderHost>,
        id: TabId,
        scene: TabScene,
        spare: Option<WindowCompositionSurface>,
    ) -> Result<Rc<Self>> {
        let composition = window.enable_window_composition()?;
        let surface = match spare {
            Some(surface) => surface,
            None => composition.create_native_surface()?,
        };

        let handle = surface.platform_surface()?.platform_handle()?;
        let view = handle
            .downcast_ref::<usize>()
            .ok_or_else(|| anyhow!("the native surface is not an AppKit view"))?;
        let layer = unsafe { metal_layer(*view as *mut AnyObject) }?;

        let wgpu_surface = host.create_surface(layer)?;
        host.send(Msg::AddTab {
            id,
            surface: wgpu_surface,
            scene,
        });

        let geometry = Rc::new(Cell::new((Bounds::default(), window.scale_factor())));
        composition.on_input(surface.id(), {
            let commands = host.sender();
            let geometry = geometry.clone();
            move |event| {
                let (bounds, scale) = geometry.get();
                forward_input(event, &commands, id, bounds, scale)
            }
        })?;

        Ok(Rc::new(Self {
            host,
            id,
            geometry,
            size: Cell::new((0, 0)),
            surface,
        }))
    }

    pub fn send(&self, cmd: Cmd) {
        self.host.send(Msg::Tab(self.id, cmd));
    }

    /// Shows or hides the tab's viewport. GPUI won't do this on its own: an
    /// element that stops being drawn leaves its native surface where it was.
    /// `set_visible` hides the AppKit view, which also stops wgpu from being
    /// given any more drawables.
    pub fn set_visible(&self, visible: bool) {
        let hidden = self
            .surface
            .platform_surface()
            .and_then(|surface| surface.set_visible(visible));
        if let Err(error) = hidden {
            eprintln!("changing a tab's visibility: {error:#}");
        }
        self.send(Cmd::SetVisible(visible));
    }

    /// Hides the tab's renderer and returns its surface for another tab to
    /// use, keeping the same `CAMetalLayer` and swap chain.
    pub fn close(&self) -> SpareSurface {
        self.set_visible(false);
        self.host.send(Msg::RemoveTab(self.id));
        self.surface.clone()
    }

    /// Nothing to do here: a `CAMetalLayer` presents without a parent
    /// compositor being asked to commit, which is what Windows needs.
    pub fn commit_composition(&self) {}

    /// Places the surface. Runs in prepaint, so it follows GPUI's layout.
    fn set_bounds(&self, bounds: Bounds<Pixels>, scale: f32) -> Result<()> {
        self.geometry.set((bounds, scale));
        let device: Bounds<DevicePixels> = bounds.to_device_pixels(scale);
        self.surface.platform_surface()?.set_bounds(device)?;
        let size = (device.size.width.0.max(1) as u32, device.size.height.0.max(1) as u32);
        if self.size.replace(size) != size {
            self.send(Cmd::Resize {
                width: size.0,
                height: size.1,
            });
        }
        Ok(())
    }
}

/// Puts a `CAMetalLayer` on a layer-backed view as its backing layer, and
/// returns it for wgpu to render into. AppKit keeps a backing layer's size in
/// step with the view, so `set_bounds` alone is enough to resize it.
///
/// # Safety
///
/// `view` must be a live `NSView` that GPUI created with `setWantsLayer:YES`.
unsafe fn metal_layer(view: *mut AnyObject) -> Result<*mut c_void> {
    let class = AnyClass::get(c"CAMetalLayer")
        .context("the Metal framework is unavailable: no CAMetalLayer class")?;
    let layer: *mut AnyObject = msg_send![class, new];
    anyhow::ensure!(!layer.is_null(), "failed to create a CAMetalLayer");
    unsafe {
        // Already true of a native surface, but setting it again is harmless
        // and makes the layer swap AppKit does below unconditional.
        let _: () = msg_send![&*view, setWantsLayer: true];
        let _: () = msg_send![&*view, setLayer: layer];
        // AppKit retained it as the view's backing layer.
        let _: () = msg_send![layer, release];
    }
    Ok(layer as *mut c_void)
}

/// Runs on the UI thread during event dispatch; it only passes events on, so
/// the camera keeps up with the mouse however busy the UI thread gets.
fn forward_input(
    event: &PlatformInput,
    commands: &mpsc::Sender<Msg>,
    tab: TabId,
    bounds: Bounds<Pixels>,
    scale: f32,
) -> DispatchEventResult {
    let map_button = |button: MouseButton| match button {
        MouseButton::Left => Some(Button::Left),
        MouseButton::Middle => Some(Button::Middle),
        MouseButton::Right => Some(Button::Right),
        MouseButton::Navigate(_) => None,
    };
    let (kind, position, shift) = match event {
        PlatformInput::MouseDown(event) => match map_button(event.button) {
            Some(button) => (PointerKind::Down(button), event.position, event.modifiers.shift),
            None => return passed_on(),
        },
        PlatformInput::MouseUp(event) => match map_button(event.button) {
            Some(button) => (PointerKind::Up(button), event.position, event.modifiers.shift),
            None => return passed_on(),
        },
        PlatformInput::MouseMove(event) => (PointerKind::Move, event.position, event.modifiers.shift),
        PlatformInput::MouseExited(event) => (PointerKind::Leave, event.position, event.modifiers.shift),
        PlatformInput::Scroll(event) => {
            // A wheel notch is usually three lines; touchpads and high-resolution
            // wheels send many small fractions of a line instead.
            let lines = match event.delta {
                ScrollDelta::Lines(lines) => lines.y,
                ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 20.0,
            };
            (PointerKind::Scroll(lines), event.position, event.modifiers.shift)
        }
        _ => return passed_on(),
    };
    let pointer = Pointer {
        kind,
        x: f32::from(position.x - bounds.origin.x) * scale,
        y: f32::from(position.y - bounds.origin.y) * scale,
        window_x: f32::from(position.x),
        window_y: f32::from(position.y),
        shift,
    };
    commands.send(Msg::Tab(tab, Cmd::Pointer(pointer))).ok();
    DispatchEventResult {
        propagate: false,
        default_prevented: false,
    }
}

fn passed_on() -> DispatchEventResult {
    DispatchEventResult {
        propagate: true,
        default_prevented: false,
    }
}

/// The element itself: it takes part in layout, and draws nothing of its own.
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
    type PrepaintState = ();

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
        if let Err(error) = self.viewport.set_bounds(bounds, window.scale_factor()) {
            eprintln!("placing the viewport: {error:#}");
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}
