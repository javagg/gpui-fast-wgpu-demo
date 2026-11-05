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

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, DevicePixels, Pixels, Point, Window, WindowCompositionSurface};
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows::core::{IUnknown, Interface as _};

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
