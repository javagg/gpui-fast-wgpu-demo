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
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result, anyhow};
use futures::channel::mpsc::UnboundedSender;
use gpui::{
    App, Bounds, DevicePixels, DispatchEventResult, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, MouseButton, Pixels, PlatformInput, Point,
    ScrollDelta, Style, Window, WindowCompositionSurface, relative,
};
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows::core::{IUnknown, Interface as _};

use crate::render::{self, Button, Cmd, Pointer, PointerKind, RenderOptions, UiEvent};
use crate::scene::Model;

/// The viewport's connection to the window and to its render thread.
pub struct Viewport {
    commands: mpsc::Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    /// Where the element was last laid out, and at what scale factor.
    geometry: Rc<Cell<(Bounds<Pixels>, f32)>>,
    size: Cell<(u32, u32)>,
    pub adapter: String,
    surface: WindowCompositionSurface,
}

impl Viewport {
    /// Call while opening the window.
    pub fn new(
        window: &Window,
        model: Arc<Model>,
        ui: UnboundedSender<UiEvent>,
        options: RenderOptions,
    ) -> Result<Rc<Self>> {
        let composition = window.enable_window_composition()?;
        let surface = composition.create_native_surface()?;

        let handle = surface.platform_surface()?.platform_handle()?;
        let visual: IDCompositionVisual = handle
            .downcast::<IUnknown>()
            .map_err(|_| anyhow!("the native surface is not a DirectComposition visual"))?
            .cast()
            .context("casting to IDCompositionVisual")?;

        let gpu = render::create_gpu(&visual)?;
        let adapter = render::adapter_summary(&gpu.adapter);
        // wgpu's configure sets the swap chain as the visual's content but,
        // not owning the device, cannot commit it. Setting the parent origin
        // is the public call that makes GPUI commit the device.
        surface.platform_surface()?.set_parent_origin(Point::default())?;

        let (commands, receiver) = mpsc::channel();
        let thread = render::spawn(gpu, model, receiver, ui.clone(), options);

        let geometry = Rc::new(Cell::new((Bounds::default(), window.scale_factor())));
        composition.on_input(surface.id(), {
            let commands = commands.clone();
            let geometry = geometry.clone();
            move |event| {
                let (bounds, scale) = geometry.get();
                forward_input(event, &commands, bounds, scale)
            }
        })?;

        Ok(Rc::new(Self {
            commands,
            thread: Some(thread),
            geometry,
            size: Cell::new((0, 0)),
            adapter,
            surface,
        }))
    }

    pub fn send(&self, cmd: Cmd) {
        self.commands.send(cmd).ok();
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

impl Drop for Viewport {
    /// Stops the renderer before `surface` releases the visual it presents to.
    fn drop(&mut self) {
        self.send(Cmd::Shutdown);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Runs on the UI thread during event dispatch; it only passes events on, so
/// the camera keeps up with the mouse however busy the UI thread gets.
fn forward_input(
    event: &PlatformInput,
    commands: &mpsc::Sender<Cmd>,
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
    commands
        .send(Cmd::Pointer(Pointer {
            kind,
            x: f32::from(position.x - bounds.origin.x) * scale,
            y: f32::from(position.y - bounds.origin.y) * scale,
            window_x: f32::from(position.x),
            window_y: f32::from(position.y),
            shift,
        }))
        .ok();
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
