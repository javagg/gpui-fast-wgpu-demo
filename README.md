# cad_viewport

A custom GPUI `Element` whose content is rendered directly by wgpu, with GPUI's
own menus and dialogs above it. On the desktop it uses gpui-fast's public API only.

```text
front  GPUI overlay     deferred draws: HUD, menus, dialogs
       wgpu swap chain  its own DX12 device, on its own thread
back   GPUI base        the root view: toolbar and panels (D3D11)
```

The part to read is `src/viewport.rs`.

## Running

Start it on the desktop. A shell in a service session (Session 0), as remote
shells often are, reports `cannot open windows while headless`.

```text
target\release\cad_viewport.exe
target\release\cad_viewport.exe --array 30 --spin --exit-after 14
target\release\cad_viewport.exe --offscreen out     (no window: PNGs, picking, frame times)
```

`tools\run_test.cmd` runs a scripted session and saves screenshots and
measurements to `test-output`.

## In a browser

The same UI, tabs and renderer also run as WebAssembly, on a different path to the
screen. A page has no window composition and an opaque canvas, but GPUI's renderer
there is wgpu itself, so `src/viewport_web.rs` draws during `Element::paint` into a
`WgpuDynamicTexture` on GPUI's own device and paints it as a sprite. The viewport
then takes part in GPUI's painter order like any other element.

```powershell
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130
tools\build_web.ps1    # builds web\dist
tools\serve_web.ps1    # serves it at http://localhost:8765/
```

The `wasm-bindgen` command line must match the `wasm-bindgen` crate in `Cargo.lock`.
`tools\test_web.ps1` drives the page in headless Edge; add `-Software` where no GPU
process is available, which also means WebGL2 rather than WebGPU.

This needs `WebPlatform::wgpu_context`, which gpui-fast doesn't have yet (see
`docs/upstream-issues.md`), so `tools\build_web.ps1` applies
`patches\gpui-fast-web-wgpu-context.patch` to the submodule, and git then shows the
submodule as modified.

Rendering has been seen working with WebGPU on a real GPU, and with WebGL2 in
software. Differences from the desktop version:

- Everything runs on the page's main thread, so a slow frame delays the UI too.
- Clicking and hovering don't pick parts, because a page can't wait for a readback.
  The assembly tree still selects them.

## Tabs

Each tab has its own native surface and swap chain, scene and camera, all on one
wgpu device and render thread, which share the compiled pipelines. Switching tabs
shows one surface and hides the other. A hidden tab draws nothing and gives up its
MSAA and depth targets, but its swap chain keeps the last frame, so showing it
again needs no new frame unless something changed. Tabs can be opened and closed
while the window is open.

GPUI does not manage this itself: an element that stops being drawn leaves its
native surface where it was, so the application has to hide it.

On Windows it can't do that with `set_visible` or `remove_surface`. Both cast the
visual to `IDCompositionVisual3`, which visuals of the device gpui-fast creates
with `DCompositionCreateDevice` don't implement, so they fail with E_NOINTERFACE
(0x80004002). This demo hides a surface by giving it empty bounds, which GPUI
neither shows nor routes input to, and reuses the surfaces of closed tabs.

## Attaching wgpu to a composition visual

wgpu's `CompositionVisual` surface sets its swap chain as the visual's content
but cannot commit, because GPUI owns the DirectComposition device. Commit after
the first frame presented since each `surface.configure`, or the viewport stays
black until something else commits, such as resizing the window. `set_bounds`
does not help while the bounds hold still, so this demo uses `set_parent_origin`,
which commits unconditionally. That is an implementation detail of gpui-fast's
Windows backend, not a documented guarantee.

## Building

gpui-fast is a submodule in `vendor\gpui-fast`, so clone with submodules, or run
`git submodule update --init` in an existing clone:

```powershell
git clone --recurse-submodules https://github.com/javagg/gpui-fast-wgpu-demo.git
cd gpui-fast-wgpu-demo
cargo build --release
```
