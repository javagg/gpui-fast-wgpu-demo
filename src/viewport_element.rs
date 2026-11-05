//! The parts of a viewport that are the same however its content reaches the
//! screen: the element that takes part in layout, and the input it forwards.
//!
//! `ViewportElement` holds the `Viewport` of whichever platform module
//! `mod viewport` selected, and only ever asks it to place itself, so this
//! file is the whole GPUI-facing surface of the viewport.

use std::rc::Rc;
use std::sync::mpsc;

use gpui::{
    App, Bounds, DispatchEventResult, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, MouseButton, Pixels, PlatformInput, ScrollDelta, Style, Window,
    WindowCompositionSurface, relative,
};

use crate::render::{Button, Cmd, Msg, Pointer, PointerKind, TabId};
use crate::viewport::Viewport;

/// What a closed tab leaves behind for the next one to use.
pub type SpareSurface = WindowCompositionSurface;

/// Passes a pointer event on to the render thread, in the tab's own
/// coordinates and in the window's.
///
/// Runs on the UI thread during event dispatch; it only passes events on, so
/// the camera keeps up with the mouse however busy the UI thread gets.
pub(crate) fn forward_input(
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
