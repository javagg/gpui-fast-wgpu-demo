# Drives the web demo in headless Edge over the DevTools protocol: collects the
# console, takes screenshots, and sends real mouse and key events. Expects
# tools\serve_web.ps1 to be serving at $Url.
param(
    [string]$OutDir,
    [string]$Url = "http://localhost:8765/",
    [int]$DebugPort = 9333,
    # Renders in software, for a session without a desktop or a GPU process.
    [switch]$Software
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$edge = "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
$profile = Join-Path $OutDir "profile"
$browserArgs = @(
    "--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-extensions",
    "--user-data-dir=$profile", "--remote-debugging-port=$DebugPort", "--window-size=1360,860",
    "--enable-unsafe-webgpu"
)
if ($Software) {
    $browserArgs += @(
        "--use-webgpu-adapter=swiftshader", "--enable-unsafe-swiftshader",
        "--use-angle=swiftshader", "--ignore-gpu-blocklist"
    )
}
$browserArgs += "about:blank"
$browser = Start-Process -FilePath $edge -ArgumentList $browserArgs -PassThru

function Get-Target {
    for ($i = 0; $i -lt 50; $i++) {
        try {
            $targets = Invoke-RestMethod -UseBasicParsing -Uri "http://127.0.0.1:$DebugPort/json" -TimeoutSec 2
            $page = @($targets | Where-Object { $_.type -eq 'page' })[0]
            if ($page) { return $page }
        } catch { }
        [Threading.Thread]::Sleep(200)
    }
    throw "no page to attach to"
}
$target = Get-Target
$socket = New-Object System.Net.WebSockets.ClientWebSocket
$socket.Options.KeepAliveInterval = [TimeSpan]::FromSeconds(30)
[void]$socket.ConnectAsync([Uri]$target.webSocketDebuggerUrl, [Threading.CancellationToken]::None).GetAwaiter().GetResult()

$script:nextId = 0
$script:console = New-Object System.Collections.ArrayList
$script:pendingReceive = $null
$script:buffer = New-Object byte[] (1MB)

function Read-Message([int]$timeoutMs) {
    if (-not $script:pendingReceive) {
        $script:message = New-Object IO.MemoryStream
        $script:pendingReceive = $socket.ReceiveAsync([ArraySegment[byte]]::new($script:buffer), [Threading.CancellationToken]::None)
    }
    while ($true) {
        if (-not $script:pendingReceive.Wait($timeoutMs)) { return $null }
        $result = $script:pendingReceive.Result
        $script:message.Write($script:buffer, 0, $result.Count)
        if ($result.EndOfMessage) {
            $script:pendingReceive = $null
            $text = [Text.Encoding]::UTF8.GetString($script:message.ToArray())
            return $text | ConvertFrom-Json
        }
        $script:pendingReceive = $socket.ReceiveAsync([ArraySegment[byte]]::new($script:buffer), [Threading.CancellationToken]::None)
    }
}

function Add-Event($event) {
    switch ($event.method) {
        'Runtime.consoleAPICalled' {
            $text = ($event.params.args | ForEach-Object { if ($null -ne $_.value) { "$($_.value)" } else { "$($_.description)" } }) -join ' '
            [void]$script:console.Add("[$($event.params.type)] $text")
        }
        'Runtime.exceptionThrown' {
            $details = $event.params.exceptionDetails
            [void]$script:console.Add("[exception] $($details.text) $($details.exception.description)")
        }
        'Log.entryAdded' {
            [void]$script:console.Add("[log $($event.params.entry.level)] $($event.params.entry.text)")
        }
    }
}

function Send-Command([string]$method, $params = @{}) {
    $id = ++$script:nextId
    $json = @{ id = $id; method = $method; params = $params } | ConvertTo-Json -Depth 8 -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    [void]$socket.SendAsync([ArraySegment[byte]]::new($bytes), 'Text', $true, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
    while ($true) {
        $reply = Read-Message 30000
        if (-not $reply) { throw "no reply to $method" }
        if ($reply.id -eq $id) {
            if ($reply.error) { throw "$method failed: $($reply.error.message)" }
            return $reply.result
        }
        Add-Event $reply
    }
}

function Wait-Page([int]$ms) {
    $until = [DateTime]::Now.AddMilliseconds($ms)
    while ($true) {
        $left = [int]($until - [DateTime]::Now).TotalMilliseconds
        if ($left -le 0) { return }
        $event = Read-Message $left
        if ($event) { Add-Event $event }
    }
}

function Get-Value([string]$expression) {
    (Send-Command 'Runtime.evaluate' @{ expression = $expression; awaitPromise = $true; returnByValue = $true }).result.value
}
function Save-Shot([string]$name) {
    $shot = Send-Command 'Page.captureScreenshot' @{ format = 'png' }
    [IO.File]::WriteAllBytes((Join-Path $OutDir "$name.png"), [Convert]::FromBase64String($shot.data))
    "shot $name"
}
function Send-Mouse([string]$type, [double]$x, [double]$y, [string]$button = 'none', [int]$buttons = 0, [double]$deltaY = 0) {
    [void](Send-Command 'Input.dispatchMouseEvent' @{ type = $type; x = $x; y = $y; button = $button; buttons = $buttons; clickCount = $(if ($type -like 'mouse[PR]*') { 1 } else { 0 }); deltaX = 0; deltaY = $deltaY })
}
function Send-Click([double]$x, [double]$y, [string]$button = 'left') {
    $mask = if ($button -eq 'right') { 2 } else { 1 }
    Send-Mouse 'mouseMoved' $x $y
    Wait-Page 60
    Send-Mouse 'mousePressed' $x $y $button $mask
    Wait-Page 60
    Send-Mouse 'mouseReleased' $x $y $button 0
}
function Send-Drag([double]$x0, [double]$y0, [double]$dx, [double]$dy, [string]$button = 'left') {
    $mask = if ($button -eq 'middle') { 4 } else { 1 }
    Send-Mouse 'mouseMoved' $x0 $y0
    Wait-Page 30
    Send-Mouse 'mousePressed' $x0 $y0 $button $mask
    for ($step = 1; $step -le 15; $step++) {
        Send-Mouse 'mouseMoved' ($x0 + $dx * $step / 15) ($y0 + $dy * $step / 15) $button $mask
        Wait-Page 25
    }
    Send-Mouse 'mouseReleased' ($x0 + $dx) ($y0 + $dy) $button 0
}

try {
    [void](Send-Command 'Runtime.enable')
    [void](Send-Command 'Log.enable')
    [void](Send-Command 'Page.enable')
    [void](Send-Command 'Emulation.setDeviceMetricsOverride' @{ width = 1360; height = 860; deviceScaleFactor = 1; mobile = $false })
    [void](Send-Command 'Page.navigate' @{ url = $Url })
    Wait-Page 6000
    "webgpu in this browser: " + (Get-Value "(async () => { if (!navigator.gpu) return 'navigator.gpu missing'; const a = await navigator.gpu.requestAdapter(); if (!a) return 'no adapter'; const i = a.info || {}; return 'adapter ' + i.vendor + ' ' + i.architecture + ' ' + i.description + ' fallback=' + a.isFallbackAdapter; })()")
    Save-Shot '01_loaded'

    # Layout, in logical pixels: toolbar 44 and tabs 32 high, tree 214 and properties 248 wide.
    $cx = (214 + (1360 - 248)) / 2; $cy = (76 + (860 - 24)) / 2

    Send-Drag $cx ($cy + 40) 220 60; Wait-Page 500; Save-Shot '02_orbit'
    Send-Mouse 'mouseWheel' $cx $cy 'none' 0 -100; Wait-Page 500; Save-Shot '03_zoom'
    Send-Click ($cx - 60) ($cy - 60) 'right'; Wait-Page 500; Save-Shot '04_context_menu'
    Send-Click ($cx + 150) ($cy + 150); Wait-Page 300

    # The tabs' titles, left to right: assembly, part, drawing.
    Send-Click 338 60; Wait-Page 800; Save-Shot '05_tab_part'
    Send-Drag $cx $cy 150 40; Wait-Page 500; Save-Shot '06_tab_part_orbit'
    Send-Click 425 60; Wait-Page 800; Save-Shot '07_tab_drawing'
    Send-Drag $cx $cy 80 30; Wait-Page 500; Save-Shot '08_drawing_panned'
    Send-Click 250 60; Wait-Page 800; Save-Shot '09_back_to_assembly'
    Send-Click 1018 60; Wait-Page 800; Save-Shot '10_new_part_tab'
    Send-Click (1360 - 42) 22; Wait-Page 500; Save-Shot '11_dialog'
} finally {
    "--- console"
    # After a panic every later event logs the same error, so show each once.
    $script:console | ForEach-Object { if ($_.Length -gt 400) { $_.Substring(0, 400) } else { $_ } } | Select-Object -Unique | Select-Object -First 30
    try { [void](Send-Command 'Browser.close') } catch { }
    if (-not $browser.WaitForExit(8000)) { "browser still running" }
}
