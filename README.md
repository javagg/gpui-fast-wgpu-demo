# cad_viewport

A custom GPUI `Element` whose content is rendered directly by wgpu, with GPUI's
own menus and dialogs above it. It uses gpui-fast's public API only.

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

This machine's network fails cargo's TLS revocation checks, so the first build
was made against a local copy of the crates, in `..\.cargo-home`:

```powershell
$env:CARGO_HOME = "D:\workspaces\dev9\works\gpwgpu\.cargo-home"
cargo build --release
```

Elsewhere a plain `cargo build --release` should do. gpui-fast is a submodule
in `vendor\gpui-fast`, so clone with `git clone --recurse-submodules`, or run
`git submodule update --init` in an existing clone.
