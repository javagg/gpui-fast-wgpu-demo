//! The custom GPUI `Element` whose content wgpu renders directly, on Linux.
//!
//! gpui-fast's Linux backend hands a native surface as an X11 child window or
//! a Wayland sub-surface, which `platform_handle` returns as a
//! `RawWindowHandle`. wgpu builds a swap chain on that handle and presents to
//! it itself:
//!
//! ```text
//! front  GPUI overlay   deferred draws: HUD, menus, dialogs
//!        swap chain     this viewport, on the Vulkan device of the render thread
//! back   GPUI base      the root view, on GPUI's own wgpu device
//! ```
//!
//! Like macOS there is nothing to commit: a swap chain presents on its own, and
//! `set_visible` hides the surface rather than emptying its bounds. GPUI's
//! Linux renderer is wgpu as well, so this viewport adds a second Vulkan
//! device next to GPUI's, as Windows adds a DX12 one next to its D3D11.

use std::cell::Cell;
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, DevicePixels, Pixels, Window, WindowCompositionSurface};
use raw_window_handle::{HasDisplayHandle as _, RawWindowHandle};

use crate::render::{Cmd, Msg, RenderHost, TabId, TabScene};
use crate::viewport_element::forward_input;

pub use crate::viewport_element::{SpareSurface, ViewportElement};

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
        // The window's display connection, which both the X11 and Wayland
        // backends hand out and which outlives every surface made from it.
        let display = window.display_handle().context("getting the display handle")?.as_raw();
        let surface = match spare {
            Some(surface) => surface,
            None => composition.create_native_surface()?,
        };

        let handle = surface.platform_surface()?.platform_handle()?;
        let native_window = handle
            .downcast::<RawWindowHandle>()
            .map_err(|_| anyhow!("the native surface is not a window handle"))?;

        let wgpu_surface = host.create_surface(display, *native_window)?;
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
    /// Hiding unmaps the X11 window or detaches the sub-surface, so the
    /// compositor stops asking for frames.
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
    /// use, keeping the same swap chain.
    pub fn close(&self) -> SpareSurface {
        self.set_visible(false);
        self.host.send(Msg::RemoveTab(self.id));
        self.surface.clone()
    }

    /// Nothing to do here: the swap chain presents without a parent compositor
    /// being asked to commit, which is what Windows needs.
    pub fn commit_composition(&self) {}

    /// Places the surface. Runs in prepaint, so it follows GPUI's layout.
    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>, scale: f32) -> Result<()> {
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
