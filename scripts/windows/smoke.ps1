# The app starts and draws: launches dataman.exe on a fresh config, waits for its window,
# screenshots it (dist\windows\screenshots\*.png) and fails on no window, an early exit or a
# blank picture. Also asks both binaries their version from a console.
#   scripts\windows\smoke.ps1 [-Exe dist\windows\Dataman\dataman.exe] [-Seconds 60]
param(
    [string]$Exe = "dist\windows\Dataman\dataman.exe",
    [int]$Seconds = 60
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\..\.."
Set-Location $root
$Exe = (Resolve-Path $Exe).Path
$shots = "dist\windows\screenshots"
New-Item -ItemType Directory -Force $shots | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[Win]::SetProcessDPIAware() | Out-Null

# A GUI program with the console it was started from: --version still prints.
$v = (& $Exe --version | Out-String).Trim()
Write-Host "dataman.exe --version: $v"
if (-not $v) { throw "dataman.exe --version printed nothing" }
$mcp = Join-Path (Split-Path $Exe) "dataman-mcp.exe"
if (Test-Path $mcp) { Write-Host "dataman-mcp.exe --version: $((& $mcp --version | Out-String).Trim())" }

$cfg = Join-Path $env:RUNNER_TEMP "dataman-smoke"
if (-not $env:RUNNER_TEMP) { $cfg = Join-Path $env:TEMP "dataman-smoke" }
Remove-Item -Recurse -Force $cfg -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $cfg | Out-Null
$env:XDG_CONFIG_HOME = $cfg
$env:DATAMAN_LOG_DIR = Join-Path $cfg "logs"
$env:DATAMAN_NO_NOTIFY = "1"
$out = Join-Path $cfg "stdout.txt"
$err = Join-Path $cfg "stderr.txt"
$p = Start-Process -FilePath $Exe -PassThru -RedirectStandardOutput $out -RedirectStandardError $err

function Save-Shot([string]$name) {
    $h = $p.MainWindowHandle
    $r = New-Object Win+RECT
    [Win]::GetWindowRect($h, [ref]$r) | Out-Null
    $w = $r.Right - $r.Left; $hh = $r.Bottom - $r.Top
    Write-Host "window $w x $hh at $($r.Left),$($r.Top)"
    # The window's own pixels (PW_RENDERFULLCONTENT: DirectX content too), whatever covers it.
    $bmp = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $dc = $g.GetHdc()
    $ok = [Win]::PrintWindow($h, $dc, 2)
    $g.ReleaseHdc($dc); $g.Dispose()
    $bmp.Save("$shots\$name-window.png", [System.Drawing.Imaging.ImageFormat]::Png)
    # And the screen as it shows, whole and the window's part of it.
    $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $screen = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    $g = [System.Drawing.Graphics]::FromImage($screen)
    $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $screen.Size)
    $g.Dispose()
    $screen.Save("$shots\$name-screen.png", [System.Drawing.Imaging.ImageFormat]::Png)
    $screen.Dispose()
    $crop = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($crop)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $crop.Size)
    $g.Dispose()
    # Drawn: many colours, not one fill (PrintWindow can come back black for a DirectX window).
    $best = 0
    foreach ($img in @($bmp, $crop)) {
        $colors = @{}
        for ($x = 0; $x -lt $w; $x += [Math]::Max(1, [int]($w / 64))) {
            for ($y = 0; $y -lt $hh; $y += [Math]::Max(1, [int]($hh / 48))) { $colors[$img.GetPixel($x, $y).ToArgb()] = 1 }
        }
        Write-Host "$($colors.Count) colours sampled"
        $best = [Math]::Max($best, $colors.Count)
    }
    $bmp.Dispose(); $crop.Dispose()
    Write-Host "PrintWindow: $ok"
    return $best
}

Add-Type -AssemblyName System.Windows.Forms
$deadline = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $deadline) {
    if ($p.HasExited) { break }
    $p.Refresh()
    if ($p.MainWindowHandle -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 250
}
if ($p.HasExited) {
    Get-Content $out, $err -ErrorAction SilentlyContinue
    throw "dataman.exe exited with $($p.ExitCode) before showing a window"
}
if ($p.MainWindowHandle -eq [IntPtr]::Zero) {
    Get-Content $out, $err -ErrorAction SilentlyContinue
    throw "no window after $Seconds s"
}
$title = $p.MainWindowTitle
Write-Host "window '$title' after $([int]((Get-Date) - $p.StartTime).TotalMilliseconds) ms"
[Win]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
Start-Sleep -Seconds 5
$colors = Save-Shot "dataman"
$alive = -not $p.HasExited
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Write-Host "--- stderr"; Get-Content $err -ErrorAction SilentlyContinue | Select-Object -Last 40
Get-ChildItem -Recurse $env:DATAMAN_LOG_DIR -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "log: $($_.FullName)" }
if (-not $alive) { throw "dataman.exe died while drawing" }
if ($title -ne "Dataman") { throw "the window is titled '$title'" }
if ($colors -lt 8) { throw "the window looks blank ($colors colours)" }
Write-Host "PASS dataman.exe starts and draws ($colors colours)"
