# Issues to raise with gpui-fast

Issues 1 and 2 are not filed yet. Issue 3 is filed as
[longbridge/gpui-fast#102](https://github.com/longbridge/gpui-fast/pull/102), an
open pull request with the change described below. All three were found against
[longbridge/gpui-fast](https://github.com/longbridge/gpui-fast)
at commit `86a8228`, the revision of the `vendor/gpui-fast` submodule. Check that they
still apply before filing.

Issue 1 and 2 are Windows-only, and issue 3 is web-only: on macOS none of them
apply, since `set_visible` works there and a `CAMetalLayer` presents without a
composition to commit.

## 1. Native surfaces cannot be hidden or removed on Windows

`PlatformSurfaceAttachment::set_visible` and `WindowComposition::remove_surface` always
fail on Windows with `E_NOINTERFACE` (`0x80004002`, "No such interface supported").

**Cause.** `crates/gpui_windows/src/directx_renderer.rs` creates the device with
`DCompositionCreateDevice`, which returns an `IDCompositionDevice`.
`crates/gpui_windows/src/fast/composition.rs` then casts its visuals to
`IDCompositionVisual3` to call `SetOpacity2`, and those visuals don't implement it.

**To reproduce.** Create a native surface with `create_native_surface` and call
`surface.platform_surface()?.set_visible(false)`. Nothing in the repository calls this
on Windows, including the `native_webview` example, which may be why it went unnoticed.
Seen on Windows 11 with a Radeon RX 550; the cause is not specific to that hardware.

**Affected, from reading the code:**

| Call | Effect | Tested |
| --- | --- | --- |
| `set_visible` | Returns the error, and the surface stays visible | Yes |
| `remove_surface` | Calls `set_visible(false)?` first, so returns before removing anything | No |
| `PortalState::rebind`, on device recovery | Has the same cast, so recovering a native surface should fail | No |

**Possible fixes, neither tried:**

- Create the device with `DCompositionCreateDevice3`, so that its visuals implement
  `IDCompositionVisual3`.
- Hide a surface without opacity, for example by emptying its rectangle clip, which the
  V1 interfaces can do.

**Workaround.** Set the surface's bounds to empty. GPUI neither shows nor routes input to
an empty surface. Keep the surfaces of closed tabs for reuse, since they can't be removed.
`src/viewport.rs` does both.

## 2. No public way to commit the composition after a GPU producer presents

wgpu's `SurfaceTargetUnsafe::CompositionVisual` sets its swap chain as the visual's content
but can't commit, because GPUI owns the `IDCompositionDevice`. Unless something commits after
the first present, the surface stays black until the window is resized.

`set_bounds` doesn't help while the bounds hold still, because `ManagedPlatformSurface`
returns early. `set_parent_origin` does commit unconditionally, which is what this demo
relies on, but that is an implementation detail.

**Possible fix.** Add a documented way to request a commit, or have the attachment commit
when asked to bind external content.

## 3. A stock web application can't use `WgpuDynamicTexture`

`WgpuDynamicTexture` has to be allocated from the context drawing the window, and
`WebPlatform` keeps that context private, as the commit that added the type (#99) says.
So the feature is out of reach unless the application replaces the platform.

**Proposed change, filed as #102.** A public accessor, three lines plus hooks.
`patches/gpui-fast-web-wgpu-context.patch` carries it, applied to the submodule by
`tools/build_web.ps1`, and is written to the fork's conventions: the logic in
`crates/gpui_web/src/fast/wgpu_context.rs` behind a `fast/mod.rs`, a `mod fast;` hook in
`gpui_web.rs`, `pub(crate)` on the field in `platform.rs`, and the accessor listed in
`docs/upstream-sync.md` as a second exception to its no-new-public-API rule.

```rust
impl WebPlatform {
    pub fn wgpu_context(&self) -> Option<WgpuContext> {
        self.wgpu_context.borrow().clone()
    }
}
```

The fork's rules forbid new public API without listing it in `docs/upstream-sync.md`, so a
pull request would add it to that list, and should run `script/check-upstream`.

With this, a custom element renders during `paint` on GPUI's device and everything else
draws over it, which `src/viewport_web.rs` shows working.
