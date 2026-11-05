# Issues to raise with gpui-fast

Not filed yet. Found against [longbridge/gpui-fast](https://github.com/longbridge/gpui-fast)
at commit `86a8228`, the revision of the `vendor/gpui-fast` submodule. Check that they
still apply before filing.

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
