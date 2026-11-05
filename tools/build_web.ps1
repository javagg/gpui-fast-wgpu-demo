# Builds the browser version into web\dist. The wasm-bindgen command line has to
# use the same format as the wasm-bindgen crate in Cargo.lock.
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# gpui-fast doesn't expose its wgpu context on the web yet. The check reports on
# stderr, which Windows PowerShell would throw on under 'Stop', so that comes after.
$patch = Join-Path $root 'patches\gpui-fast-web-wgpu-context.patch'
git -C vendor\gpui-fast apply --reverse --check $patch 2>$null
if ($LASTEXITCODE -ne 0) {
    git -C vendor\gpui-fast apply $patch
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    "applied $patch"
}

$ErrorActionPreference = 'Stop'
cargo build --release --target wasm32-unknown-unknown
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
wasm-bindgen --target web --no-typescript --out-dir web\dist target\wasm32-unknown-unknown\release\cad_viewport.wasm
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item web\index.html web\dist\index.html
"built: {0:N1} MB of wasm" -f ((Get-Item web\dist\cad_viewport_bg.wasm).Length / 1MB)
