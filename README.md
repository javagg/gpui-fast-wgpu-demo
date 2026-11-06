# cad_viewport

A custom GPUI `Element` whose content is rendered directly by wgpu, with GPUI's
own menus and dialogs above it. On the desktop it uses gpui-fast's public API only.

```text
front  GPUI overlay     deferred draws: HUD, menus, dialogs
       wgpu swap chain  its own DX12 or Metal device, on its own thread
back   GPUI base        the root view: toolbar and panels (D3D11 or Metal)
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

## On macOS

The desktop path also builds and runs there, on Metal: the same UI, tabs,
picking and render thread, with `src/viewport_macos.rs` in place of
`src/viewport.rs`.

```text
cargo run --release -- --spin --array 30 --exit-after 14
cargo run --release -- --offscreen out
```

gpui-fast's macOS native surface is a layer-backed `NSView`, whose pointer
`platform_handle` returns as a `usize`. The viewport puts a `CAMetalLayer` on
it and gives that to wgpu as a `SurfaceTargetUnsafe::CoreAnimationLayer`. Two
things this path needs that Windows doesn't:

- No commit. A `CAMetalLayer` presents by itself, so `commit_composition` does
  nothing, and `set_visible` hides a tab properly instead of the empty-bounds
  workaround.
- `font-kit`. GPUI's text backend on macOS is behind that feature, and the
  crate builds with `default-features = false`, so without it GPUI draws every
  quad and no glyph at all: the UI comes up with blank buttons and an empty
  assembly tree.

Two things are about this machine rather than macOS:

- `gpui_apple`'s build script compiles `shaders.metal` with `xcrun metal`, which
  Command Line Tools alone don't provide. Either install Xcode, or build with
  `--features gpui_platform/runtime_shaders` to compile them at runtime instead.
- Fetching the pinned wgpu revision needs a GitHub git clone. Where that is too
  slow, put the same revision in `vendor/wgpu` and point the build at it:

  ```text
  curl -LO https://codeload.github.com/gfx-rs/wgpu/tar.gz/<rev>
  mkdir -p vendor/wgpu && tar xzf wgpu.tar.gz -C vendor/wgpu --strip-components=1
  ```

  ```toml
  [patch."https://github.com/gfx-rs/wgpu"]
  wgpu = { path = "vendor/wgpu/wgpu" }
  ```

  `vendor/wgpu` is git-ignored; drop the table to go back to the git source.

An occluded window is worth knowing about: when another window covers this one
completely, macOS stops handing out drawables and the render thread idles, which
is also why a hidden tab costs nothing.

Measured on an M1 Max at 1796x1522 with MSAA 4x: 104 fps in the viewport, and
`--offscreen` reports 5.7 ms per frame for 5,378,000 triangles in a 10x10 array.

## On Linux

The desktop path also builds and runs here: the same UI, tabs, picking and
render thread, with `src/viewport_linux.rs` in place of `src/viewport.rs`.

```text
cargo run --release -- --spin --array 30 --exit-after 14
cargo run --release -- --offscreen out
```

gpui-fast's Linux native surface is an X11 child window or a Wayland
sub-surface, whose handle `platform_surface` returns as a `RawWindowHandle`.
The viewport passes that and the window's display connection to wgpu, which
builds a Vulkan swap chain on it and presents by itself: as on macOS there is
nothing to commit, and `set_visible` maps or detaches the surface instead of
the empty-bounds workaround. GPUI's Linux renderer is wgpu too, so the
viewport's device is a second one next to GPUI's, as on Windows it is a DX12
device next to the D3D11 one.

Two things this path needs that the other platforms don't:

- The `x11` and `wayland` features of `gpui_platform`. gpui-fast builds
  `gpui_linux` with `default-features = false`, so without them the Linux
  binary runs GPUI's headless platform: it lays out and takes input, opens a
  window, and then refuses composition with
  `window composition is not supported on this platform`.
- `CompositeAlphaMode::Auto` rather than `Opaque`. An X11 or Wayland surface
  offers only `PreMultiplied` and `Inherit`, and `Surface::configure` fails
  the request instead of picking one. The content stays opaque because the
  renderer clears with `wgpu::Color::BLACK`, whose alpha is one; only the
  ground grid's fade-out edge would blend with what is behind it, and only
  under a compositor.

The build needs the usual display and text libraries:

```text
apt-get install build-essential pkg-config curl git libssl-dev libasound2-dev \
  libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-dev \
  libx11-xcb-dev libxcb1-dev libxcb-render-util0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libwayland-dev libxdo-dev fonts-noto-cjk
```

Both paths have been run in an OrbStack arm64 container, where no GPU is
available and `mesa-vulkan-drivers` supplies the only Vulkan device:

```text
adapter: llvmpipe (LLVM 20.1.2, 128 bits) (Vulkan)
array 1x1: 82.9 ms per frame; 10x10: 1256 ms; 30x30: 14836 ms
window, 898x760, array 3x3: 4.3 fps, 102 frames in 24 s
```

Those are debug-build software-rasterized numbers, so they say nothing about
speed — what they showed is that the frames, the picking reads and the tab
independence come out the same as on Metal: of the fifteen PNGs, eleven differ
from the macOS run in about one percent of pixels, and the other four, the
densest ones, differ along edges and inside the geometry, where the two
rasterizers antialias differently. Nothing moved: an amplified difference
shows no doubled or offset edge anywhere. In the window GPUI's overlay paints
above the viewport as it does elsewhere, which is what the native surface is
for.

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

This needs `WebPlatform::wgpu_context`, which gpui-fast doesn't have yet (filed as
[longbridge/gpui-fast#102](https://github.com/longbridge/gpui-fast/pull/102); see
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
