# Runs the demo through a scripted session and saves what its window shows.
# Input goes to the window with PostMessage, so the real cursor never moves.
# Has to run on the interactive desktop: GPUI cannot open a window from a
# service session (Session 0), which is where remote shells often run.
param(
    [string]$Exe,
    [string]$OutDir,
    [string]$Scenario = "interact",
    [string[]]$AppArgs = @("--exit-after", "16")
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public struct RECT { public int Left, Top, Right, Bottom; }
public struct POINT { public int X, Y; }
public static class W {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint code, uint type);
}
"@
[void][W]::SetProcessDpiAwarenessContext([IntPtr](-4))
New-Item -ItemType Directory -Force $OutDir | Out-Null

$stdout = Join-Path $OutDir "$Scenario.stdout.txt"
$stderr = Join-Path $OutDir "$Scenario.stderr.txt"
$p = Start-Process -FilePath $Exe -ArgumentList $AppArgs -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 100 -and $hwnd -eq [IntPtr]::Zero; $i++) {
    Start-Sleep -Milliseconds 100
    $p.Refresh()
    if ($p.HasExited) { "exited early"; Get-Content $stderr -Tail 20; exit 1 }
    $hwnd = $p.MainWindowHandle
}
if ($hwnd -eq [IntPtr]::Zero) { "no window"; exit 1 }
"window $hwnd, pid $($p.Id)"

function Get-ClientSize { $r = New-Object RECT; [void][W]::GetClientRect($hwnd, [ref]$r); ,@($r.Right, $r.Bottom) }
$scale = [W]::GetDpiForWindow($hwnd) / 96.0
"scale $scale, client $((Get-ClientSize) -join 'x')"

function Save-Shot([string]$name) {
    $c = Get-ClientSize
    $bmp = New-Object System.Drawing.Bitmap $c[0], $c[1]
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    # PW_CLIENTONLY | PW_RENDERFULLCONTENT: includes DirectComposition content.
    [void][W]::PrintWindow($hwnd, $hdc, 3)
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save((Join-Path $OutDir "$name.png"), [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    "shot $name"
}
function Get-LParam([int]$x, [int]$y) { [IntPtr](($y -shl 16) -bor ($x -band 0xffff)) }
function Send-Mouse([uint32]$m, [int]$w, [int]$x, [int]$y) { [void][W]::PostMessage($hwnd, $m, [IntPtr]$w, (Get-LParam $x $y)) }
function Send-Move([int]$x, [int]$y, [int]$buttons = 0) { Send-Mouse 0x0200 $buttons $x $y }
function Send-LeftClick([int]$x, [int]$y) { Send-Move $x $y; Start-Sleep -Milliseconds 40; Send-Mouse 0x0201 1 $x $y; Start-Sleep -Milliseconds 40; Send-Mouse 0x0202 0 $x $y }
function Send-RightClick([int]$x, [int]$y) { Send-Move $x $y; Start-Sleep -Milliseconds 40; Send-Mouse 0x0204 2 $x $y; Start-Sleep -Milliseconds 40; Send-Mouse 0x0205 0 $x $y }
function Send-Drag([int]$x0, [int]$y0, [int]$dx, [int]$dy, [string]$button = 'left') {
    $down, $up, $mk = if ($button -eq 'middle') { 0x0207, 0x0208, 0x10 } else { 0x0201, 0x0202, 1 }
    Send-Move $x0 $y0; Start-Sleep -Milliseconds 30; Send-Mouse $down $mk $x0 $y0
    for ($s = 1; $s -le 20; $s++) { Send-Move ($x0 + $dx * $s / 20) ($y0 + $dy * $s / 20) $mk; Start-Sleep -Milliseconds 16 }
    Send-Mouse $up 0 ($x0 + $dx) ($y0 + $dy)
}
function Send-Wheel([int]$x, [int]$y, [int]$notches) {
    $pt = New-Object POINT; $pt.X = $x; $pt.Y = $y; [void][W]::ClientToScreen($hwnd, [ref]$pt)
    [void][W]::PostMessage($hwnd, 0x020A, [IntPtr]((120 * $notches) -shl 16), (Get-LParam $pt.X $pt.Y))
}
function Send-Key([int]$vk) {
    $scan = [int][W]::MapVirtualKey([uint32]$vk, 0)
    [void][W]::PostMessage($hwnd, 0x0100, [IntPtr]$vk, [IntPtr](1 -bor ($scan -shl 16)))
    Start-Sleep -Milliseconds 20
    [void][W]::PostMessage($hwnd, 0x0101, [IntPtr]$vk, [IntPtr]([int64]0xC0000001 -bor ($scan -shl 16)))
}
function Get-Px([double]$logical) { [int]($logical * $scale) }

# Layout, in logical pixels: toolbar 44 and tabs 32 high, tree 214 and properties 248 wide.
$c = Get-ClientSize
$vx0 = Get-Px 214; $vx1 = $c[0] - (Get-Px 248); $vy0 = Get-Px 76; $vy1 = $c[1] - (Get-Px 24)
$cx = [int](($vx0 + $vx1) / 2); $cy = [int](($vy0 + $vy1) / 2)

switch -Wildcard ($Scenario) {
    'interact' {
        Start-Sleep -Seconds 3; Save-Shot '01_initial'
        Send-Move $cx $cy; Start-Sleep -Milliseconds 500; Save-Shot '02_hover'
        Send-LeftClick $cx $cy; Start-Sleep -Milliseconds 500; Save-Shot '03_selected'
        Send-Drag $cx ($cy + 40) 220 60; Start-Sleep -Milliseconds 400; Save-Shot '04_orbit'
        Send-Wheel $cx $cy 4; Start-Sleep -Milliseconds 400; Save-Shot '05_zoom'
        Send-Drag $cx $cy 120 -40 'middle'; Start-Sleep -Milliseconds 400; Save-Shot '06_pan'
        Send-RightClick ($cx - 80) ($cy - 40); Start-Sleep -Milliseconds 500; Save-Shot '07_context_menu'
        Send-Key 0x1B; Start-Sleep -Milliseconds 300
        Send-Key 0x46; Start-Sleep -Milliseconds 700; Save-Shot '08_fit_key'
        Send-LeftClick ($c[0] - (Get-Px 42)) (Get-Px 22); Start-Sleep -Milliseconds 500; Save-Shot '09_dialog'
        Send-Key 0x1B; Start-Sleep -Milliseconds 300
        Send-Key 0x4F; Start-Sleep -Milliseconds 400; Save-Shot '10_orthographic'
    }
    'layout' {
        Start-Sleep -Seconds 3
        # The panels button sits left of the about button on the toolbar's right.
        Send-LeftClick ($c[0] - (Get-Px 110)) (Get-Px 22); Start-Sleep -Milliseconds 700; Save-Shot '11_panels_off'
        Send-LeftClick ($c[0] - (Get-Px 110)) (Get-Px 22); Start-Sleep -Milliseconds 700; Save-Shot '12_panels_on'
    }
    'idle' {
        Start-Sleep -Seconds 4
        $p.Refresh(); $before = $p.TotalProcessorTime
        Start-Sleep -Seconds 10
        $p.Refresh(); $used = ($p.TotalProcessorTime - $before).TotalSeconds
        "idle: {0:N3}s of CPU over 10s ({1:N2}% of one core)" -f $used, ($used / 10 * 100)
    }
    'stress*' {
        Start-Sleep -Seconds 6; Save-Shot $Scenario
    }
}

if (-not $p.WaitForExit(90000)) { "still running after 90s"; exit 2 }
"--- stdout"; Get-Content $stdout
"--- stderr"; Get-Content $stderr -Tail 15
