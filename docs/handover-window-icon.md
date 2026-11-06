# Handover: the window-icon fix upstream

State as of 2026-11-06. Everything below is pushed; nothing here is half-applied
to a working tree.

## Where things stand

| Place | Ref | State |
| --- | --- | --- |
| zed-industries/zed | [#66603](https://github.com/zed-industries/zed/pull/66603) | open, mergeable, 2 commits, 2 files, +259/−2 |
| javagg/zed | `gpui-windows-window-icon` | the PR's head branch |
| longbridge/gpui-fast | [#102](https://github.com/longbridge/gpui-fast/pull/102) | open, mergeable, 2 commits: reviewed, nits taken |
| this repository | `main` | macOS port, shared viewport element, these docs |

#66603 commits: `22ba5a8a` adds the fix, `6bf885f9` answers Copilot's review by
logging a failed `DeleteObject` instead of discarding it. Its checks are all
green (`cla-signed`, `authorship-and-label`, `route-pr`, `welcome`,
`nudge-contributor`); Zed's real build CI is not dispatched for a fork until a
maintainer triggers it, so there is no build result to watch yet.

## What #66603 still needs

1. **A Windows screenshot.** Zed's first-contribution note asks for evidence
   that fits the change. The change is a title bar, taskbar and alt-tab icon, so
   the evidence is a picture of those.
2. **A testing note written by the author.** The same note asks for human-worded
   replies. Draft below; rewrite it before posting.
3. Nothing else is outstanding: Copilot's one comment is answered and fixed.

### Recipe for the Windows run

The patch does not apply to gpui-fast (its `gpui_windows` carries the fork's own
`fast/` hooks), so test against Zed itself:

```powershell
git clone --depth 1 https://github.com/zed-industries/zed
cd zed
git checkout 4a6121bb0f10c6e8037b741b7dc0a4cd639ae289
git apply zed-windows-window-icon.patch
```

Then make an example set an icon — paste into `crates/gpui/examples/hello_world.rs`
before `open_window`, in the `WindowOptions`:

```rust
// A 64x64 icon: opaque green, with the top-left quadrant fully transparent so
// the AND mask has something to do.
let mut icon = image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 200, 90, 255]));
for (x, y, pixel) in icon.enumerate_pixels_mut() {
    if x < 32 && y < 32 {
        *pixel = image::Rgba([0, 0, 0, 0]);
    }
}
// WindowOptions { icon: Some(std::sync::Arc::new(icon)), ..Default::default() }
```

What to look at: the title bar's left corner, the taskbar button, and alt-tab.
The transparent quadrant must stay transparent — a black square there means the
mask is wrong. Then close and reopen the window a few times and watch the GDI
object count in Task Manager; it should not climb, which is the `Drop` path.

`cargo test -p gpui_windows --features test-support --lib` also runs the four
`pack_icon_buffers` tests, which is the only part that needs no window.

### Draft of the testing note

> **How this was tested.** The same change runs on Windows 11 in
> [gpui-ce/gpui-ce#372](https://github.com/gpui-ce/gpui-ce/pull/372), where a
> 256x256 PNG set through `WindowOptions::icon` shows correctly in the title
> bar, taskbar and alt-tab with per-pixel alpha, and a repeated open/close run
> leaks no GDI handles. Screenshot attached. On the machine that prepared this
> port there is no Windows, so what was verified here is that `rustfmt` is
> clean, that the new code and the `WM_SETICON`/`DestroyIcon` call sites
> type-check against `x86_64-pc-windows-msvc` with `windows` 0.62.2, and that
> the four packing tests pass. Every Win32 signature used was read out of the
> `windows` 0.62.2 source rather than assumed.

## gpui-fast #102, the web accessor

Reviewed by @huacnlee (three nits) and by Copilot (three comments). `25bbf8c`
takes the two that were real: the rule 5 exception now names
`WebPlatform::wgpu_context()` and cites #102 rather than #99, and the accessor's
own comment now says the web helpers in `gpui_platform` build a platform without
handing it back.

Two suggestions were declined on the thread, with reasons. Keeping the field
private and moving the `impl` into `platform.rs` would put gpui-fast code in an
upstream file, against rules 1 and 2 - and the reviewer's own nit 3 accepted the
visibility bump. Gating `mod fast` on `test` as well as wasm cannot work:
`WebPlatform` lives in `platform`, which `gpui_web.rs:28` compiles only under
`target_family = "wasm"`, and `gpui_wgpu` is a wasm-target dependency, so a host
test build would fail to resolve both names rather than catch anything.

Left open for the maintainers: whether `gpui_platform` should return the
`Rc<WebPlatform>` its three web constructors build, so an application does not
have to construct the platform itself. Wider than this accessor, so it was not
grown into this PR. This repository is one consumer of the current setup -
`src/viewport_web.rs:270` builds the platform, keeps a second `Rc`, and calls the
accessor from `Element::paint`.

## macOS: there is nothing to fix, and that is worth knowing

`WindowParams::icon` is also ignored on macOS, but not because of a missing
call — AppKit has no per-window icon:

- `responds(to: setIcon:)` is false on an `NSWindow`, and no `setIcon:` appears
  in the method lists of `NSWindow`, `NSResponder` or `NSObject`;
- Swift has no `NSWindow.icon` member at all;
- `NSApplication.applicationIconImage` is app-global, so it colors the dock tile
  for every window;
- the title bar's document-icon button is nil until `representedURL` is set, and
  once it exists it carries that URL's semantics — a proxy icon and a clickable
  path menu. Overriding its image is not a window icon.

So the doc comment says *x11 and Windows*, and a macOS PR would be a different
feature, not this one. This was checked with `swift` against the macOS 15 SDK on
an M1 Max; the reasoning is in #66603's description under "Why not macOS".

## This machine, so the next person is not ambushed

- GitHub over git is unusable here: `github.com:443` times out or drops
  mid-transfer, while `api.github.com` and `codeload.github.com` work. Sources
  were fetched as tarballs, and both PRs were built through the Git data API
  rather than a clone. Push with
  `git -c http.version=HTTP/1.1 -c credential.helper='!gh auth git-credential' push https://github.com/javagg/<repo>.git main`,
  retrying: it fails often and succeeds on a later attempt.
- `vendor/wgpu` is a tarball of the pinned revision, and `Cargo.toml` needs
  `[patch."https://github.com/gfx-rs/wgpu"] wgpu = { path = "vendor/wgpu/wgpu" }`
  to build here at all. That table is deliberately **not committed** — it points
  at a git-ignored directory. Add it, build, then drop it before committing.
- No Xcode, only Command Line Tools, so `gpui_apple`'s build script cannot run
  `xcrun metal`: build with `--features gpui_platform/runtime_shaders`.
- `vendor/gpui-fast` is tarball content without its own `.git`, so
  `git -C vendor/gpui-fast ...` resolves to *this* repository. That silently
  breaks `tools/build_web.ps1`, which patches the submodule that way.

## Local artifacts

| File | What |
| --- | --- |
| `patches/zed-windows-window-icon.patch` | #66603's current diff, for `git apply` |
| `patches/zed-windows-window-icon-pr.md` | #66603's body as submitted |
| `patches/gpui-fast-web-wgpu-context.patch` | #102's change, applied by `tools/build_web.ps1` |

The first two are the upstream work, kept here so a second machine can fetch
them with the repository instead of carrying files around: `git apply
patches/zed-windows-window-icon.patch` is the whole recipe input.
