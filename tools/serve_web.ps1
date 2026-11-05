# Serves web\dist to this machine only. A wasm module needs its proper content
# type, so opening index.html from disk won't do.
param([int]$Port = 8765)
$root = Join-Path (Split-Path -Parent $PSScriptRoot) 'web\dist'
$types = @{ '.html' = 'text/html; charset=utf-8'; '.js' = 'text/javascript'; '.wasm' = 'application/wasm' }
$listener = New-Object System.Net.HttpListener
$listener.Prefixes.Add("http://localhost:$Port/")
$listener.Start()
"serving $root at http://localhost:$Port/"
while ($listener.IsListening) {
    $context = $listener.GetContext()
    $relative = [Uri]::UnescapeDataString($context.Request.Url.AbsolutePath.TrimStart('/'))
    if (-not $relative) { $relative = 'index.html' }
    $path = [IO.Path]::GetFullPath((Join-Path $root $relative))
    $response = $context.Response
    if ($path.StartsWith($root) -and (Test-Path -LiteralPath $path -PathType Leaf)) {
        $bytes = [IO.File]::ReadAllBytes($path)
        $type = $types[[IO.Path]::GetExtension($path)]
        $response.ContentType = if ($type) { $type } else { 'application/octet-stream' }
        $response.ContentLength64 = $bytes.Length
        $response.OutputStream.Write($bytes, 0, $bytes.Length)
    } else {
        $response.StatusCode = 404
    }
    $response.Close()
}
