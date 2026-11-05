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

Elsewhere a plain `cargo build --release` should do, with `..\gpui-fast` beside
this directory.
