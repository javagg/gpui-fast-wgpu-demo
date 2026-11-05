//! The custom GPUI `Element` whose content wgpu renders directly.
//!
//! GPUI's `Element::paint` only records primitives into a `Scene`; it never
//! sees a render pass, and on Windows GPUI's renderer is Direct3D 11 anyway.
//! So the element does not draw at all. It asks for layout like any other
//! element and hands its bounds to a native composition surface, a
//! DirectComposition visual that holds a wgpu swap chain of its own:
//!
//! ```text
//! front  GPUI overlay    deferred draws: HUD, menus, dialogs
//!        wgpu swap chain this viewport, on the DX12 device of the render thread
//! back   GPUI base       the root view
//! ```

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, Bounds, DevicePixels, DispatchEventResult, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, MouseButton, Pixels, PlatformInput, Point,
    ScrollDelta, Style, Window, WindowCompositionSurface, relative,
};
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows::core::{IUnknown, Interface as _};

use crate::render::{Button, Cmd, Msg, Pointer, PointerKind, RenderHost, TabId, TabScene};

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
        let visual: IDCompositionVisual = handle
            .downcast::<IUnknown>()
            .map_err(|_| anyhow!("the native surface is not a DirectComposition visual"))?
            .cast()
            .context("casting to IDCompositionVisual")?;

        let wgpu_surface = host.create_surface(&visual)?;
        // wgpu's configure sets the swap chain as the visual's content but,
        // not owning the device, cannot commit it. Setting the parent origin
        // is the public call that makes GPUI commit the device.
        surface.platform_surface()?.set_parent_origin(Point::default())?;
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

    /// Shows or hides the tab's surface. GPUI won't do this on its own: an
    /// element that stops being drawn leaves its native surface where it was.
    ///
    /// Hiding gives the surface empty bounds, which GPUI neither shows nor
    /// routes input to. `PlatformSurfaceAttachment::set_visible` cannot do it:
    /// it casts the visual to `IDCompositionVisual3`, which visuals of the
    /// device from `DCompositionCreateDevice` don't implement, so on Windows it
    /// always fails with E_NOINTERFACE.
    pub fn set_visible(&self, visible: bool) {
        let bounds = if visible {
            let (bounds, scale) = self.geometry.get();
            bounds.to_device_pixels(scale)
        } else {
            Bounds::default()
        };
        let placed = self
            .surface
            .platform_surface()
            .and_then(|surface| surface.set_bounds(bounds));
        if let Err(error) = placed {
            eprintln!("changing a tab's visibility: {error:#}");
        }
        self.send(Cmd::SetVisible(visible));
    }

    /// Takes the surface off screen and stops the tab's renderer, and returns
    /// the surface for another tab to use. It can't be removed from the window:
    /// `WindowComposition::remove_surface` hides it first, which fails as
    /// described at [`Self::set_visible`].
    pub fn close(&self) -> WindowCompositionSurface {
        self.set_visible(false);
        self.host.send(Msg::RemoveTab(self.id));
        self.surface.clone()
    }

    /// Makes GPUI commit its DirectComposition device. Setting the parent
    /// origin is the public call that does so even when nothing has changed.
    pub fn commit_composition(&self) {
        let committed = self
            .surface
            .platform_surface()
            .and_then(|surface| surface.set_parent_origin(Point::default()));
        if let Err(error) = committed {
            eprintln!("committing the composition: {error:#}");
        }
    }

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
