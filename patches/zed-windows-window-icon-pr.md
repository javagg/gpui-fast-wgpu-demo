# gpui_windows: honor `WindowOptions::icon`

Filed as https://github.com/zed-industries/zed/pull/66603 against `main` @ `4a6121b`.


## The problem

`WindowParams::icon` is documented as x11-only, and on Windows it is read by
nothing: every window gets the icon of the window class, which
`load_icon` takes from the host executable's own resources
(`LoadImageW(module, PCWSTR(1), IMAGE_ICON, LR_DEFAULTSIZE | LR_SHARED)`).
So an application has no way to set a per-window icon at runtime — a
document window, a utility panel, or a build of gpui that embeds no icon
resource all show the same fallback.

## The change

Convert the RGBA image to an `HICON` and apply it with `WM_SETICON` for both
`ICON_BIG` and `ICON_SMALL`, so the title bar, taskbar and alt-tab all show
it:

- the color bitmap is a 32bpp premultiplied BGRA DIB;
- the AND mask sets a bit for every pixel whose alpha is zero, because
  Windows falls back to the mask where the color bitmap has no alpha to
  blend, and an all-zero mask would turn a fully transparent image into an
  opaque black square;
- the mask is a 1bpp `BI_RGB` DIB whose `biClrUsed` is zero, which needs the
  two palette entries that follow the header, so it is built from a
  `#[repr(C)]` header-plus-pair rather than `BITMAPINFO`, which only carries
  room for one `RGBQUAD`.

Callers that pass no icon keep the class fallback unchanged. A conversion
failure logs and leaves the window alone rather than failing creation.

Two lifetime points, since GDI handles have no RAII here:

- the icon is created and applied *after* the fallible placement steps
  (`border_offset.update`, `retrieve_window_placement`, `SetWindowPlacement`),
  so a failure on those paths returns before an `HICON` exists and cannot
  leak it;
- `CreateIconIndirect` copies both bitmaps, so they are released whether it
  succeeds or fails, and the resulting `HICON` is stored on the window and
  destroyed with it in `Drop` — it is not `LR_SHARED`, so we own it.

Buffer packing is split out of the GDI calls into a pure `pack_icon_buffers`,
which is what the tests exercise: DWORD-aligned mask rows at widths 1, 9 and
33 with the padding bytes verified clear, zero-alpha pixels on successive
rows, a fully transparent image whose mask covers every pixel, and partial
alpha premultiplied into BGRA byte order.

## Prior art

This is the same fix landed for the gpui-fast derivative as
[gpui-ce/gpui-ce#372](https://github.com/gpui-ce/gpui-ce/pull/372), where it
was verified on Windows 11 against a 256x256 PNG with per-pixel alpha in the
title bar, taskbar and alt-tab, and a repeated open/close smoke run for
handle leaks.

## Why not macOS

There is no per-window icon to set. `NSWindow` has no `icon` property and does
not respond to `setIcon:`: checked against the SDK on macOS 15, the selector
appears in neither `NSWindow`'s nor `NSResponder`'s method lists, and Swift
reports no `icon` member. The two things that do exist are app-global or mean
something else:

- `NSApplication.applicationIconImage` colors the dock tile, for every window;
- the title bar's document-icon button is nil until `representedURL` is set,
  and then carries that URL's semantics - a clickable path menu and a proxy
  icon - so overriding its image is not a window icon.

`WindowParams::icon` therefore stays x11 and Windows, which is what the doc
comment now says.

## Checklist

- [x] I've followed the [style guide](https://github.com/zed-industries/zed/blob/main/CONTRIBUTING.md#style)
- [x] `rustfmt` reports no changes
- [x] The new code type-checks against `x86_64-pc-windows-msvc` (`windows` 0.62)
- [x] The four packing tests pass, run in isolation against the same source
- [ ] `cargo test -p gpui_windows --features test-support --lib` on a full workspace checkout
- [x] I've signed the CLA
