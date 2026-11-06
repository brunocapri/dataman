# How well a window follows a drag, measured from outside the app. Needs the desktop session (a
# scheduled task with /IT over SSH); moves the real mouse, so nobody should be using the PC.
#   scripts\windows\drag-bench.ps1 [-Exe target\release\dataman.exe] [-ExeArgs --demo] [-SetEnv A=1,B=2]
#       [-GrabY 30] [-GrabXFromRight 800] [-Seconds 8] [-Hz 500] [-Out C:\dev\drag.json] [-Shot C:\dev\drag.png]
# Reports, as JSON and text, one line per phase:
#   idle: CPU of the process while nothing happens, and the frames GPUI drew (ZED_MEASUREMENTS).
#   hover: the same with the mouse moving over the window, no button down.
#   drag: how often the window's position changed, how old the cursor position it shows was when
#         it did (from the cursor being injected to the window being there), the same read back
#         from the composed desktop, the compositor's own pace, CPU, GPU and frames.
#   animation: Ctrl+B through the sidebar's animation, new pictures read back from the screen.
#   resize: the bottom-right corner dragged about.
# -WatchOnly <seconds> only counts new pictures on screen (with --bench-scroll: every frame differs).
# -Load <threads> keeps that many threads spinning through the whole run; the harness's own threads
# outrank them, so only the program under test feels it.
# -Reference runs all of it on a plain window Windows draws by itself: as good as it gets. The
# read-back needs nothing dark above the backdrop: over SSH, a console left open in front spoils it.
param(
    [string]$Exe = "target\release\dataman.exe",
    [string[]]$ExeArgs = @(),
    [string[]]$SetEnv = @(),
    [int]$GrabY = 30,
    [int]$GrabXFromRight = 800,
    [int]$SettleSeconds = 8,
    [int]$IdleSeconds = 10,
    [int]$Seconds = 8,
    [int]$Hz = 500,
    [string]$Out = "",
    [string]$Shot = "",
    [switch]$Reference,
    [switch]$Maximize,
    [double]$WatchOnly = 0,
    [int]$Load = 0
)
$ErrorActionPreference = "Stop"
Add-Type -Language CSharp -CompilerParameters (& {
    $c = New-Object System.CodeDom.Compiler.CompilerParameters
    $c.CompilerOptions = "/unsafe"
    [void]$c.ReferencedAssemblies.Add("System.dll")
    $c
}) -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;

public static class Drag {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx, dy; public uint data, flags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public MOUSEINPUT mi; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] i, int size);
    [DllImport("dwmapi.dll")] static extern int DwmFlush();

    // The compositor's own pace: how long each composition took to follow the last one.
    static List<double> flushes;
    static Thread flusher;
    static volatile bool stopFlush;
    public static void StartCompositorWatch() {
        flushes = new List<double>(4096);
        stopFlush = false;
        flusher = new Thread(() => {
            Thread.CurrentThread.Priority = ThreadPriority.Highest;
            var c = Stopwatch.StartNew();
            double last = c.Elapsed.TotalMilliseconds;
            while (!stopFlush) {
                DwmFlush();
                double t = c.Elapsed.TotalMilliseconds;
                flushes.Add(t - last);
                last = t;
            }
        });
        flusher.Start();
    }
    public static double[] StopCompositorWatch() {
        stopFlush = true;
        flusher.Join();
        return flushes.ToArray();
    }

    // The mouse moving over the window with no button down: every position a real input message.
    public static void Hover(int x0, int y0, double seconds, int hz) {
        var clock = Stopwatch.StartNew();
        long step = Stopwatch.Frequency / hz, next = 0;
        while (clock.Elapsed.TotalSeconds < seconds) {
            double s = clock.Elapsed.TotalSeconds;
            Mouse(x0 + (int)Math.Round(300 * Math.Sin(2 * Math.PI * s / 2.0)), y0 + (int)Math.Round(100 * Math.Sin(2 * Math.PI * s / 3.3)), 0);
            next += step;
            while (clock.ElapsedTicks < next) { }
        }
    }

    [DllImport("user32.dll")] static extern IntPtr GetDC(IntPtr h);
    [DllImport("gdi32.dll")] static extern IntPtr CreateCompatibleDC(IntPtr h);
    [DllImport("gdi32.dll")] static extern IntPtr SelectObject(IntPtr dc, IntPtr o);
    [DllImport("gdi32.dll")] static extern bool BitBlt(IntPtr d, int x, int y, int w, int h, IntPtr s, int sx, int sy, uint rop);
    [StructLayout(LayoutKind.Sequential)] struct BITMAPINFOHEADER { public uint size; public int w, h; public ushort planes, bits; public uint comp, sizeImage; public int xp, yp; public uint used, important; }
    [DllImport("gdi32.dll")] static extern IntPtr CreateDIBSection(IntPtr dc, ref BITMAPINFOHEADER b, uint usage, out IntPtr bits, IntPtr sec, uint off);

    // The window's left edge as it is on screen, read back from the composed desktop over the
    // backdrop (pure magenta): what the eye gets, where GetWindowRect only says what was asked.
    static IntPtr screen, memDc, stripBits;
    static int stripW;
    public static void StripInit() {
        screen = GetDC(IntPtr.Zero);
        memDc = CreateCompatibleDC(screen);
        stripW = GetSystemMetrics(0);
        var b = new BITMAPINFOHEADER();
        b.size = 40; b.w = stripW; b.h = 1; b.planes = 1; b.bits = 32;
        IntPtr dib = CreateDIBSection(screen, ref b, 0, out stripBits, IntPtr.Zero, 0);
        SelectObject(memDc, dib);
    }
    public static int StripEdge(int y) {
        BitBlt(memDc, 0, 0, stripW, 1, screen, 0, y, 0x00CC0020);
        unsafe {
            byte* p = (byte*)stripBits;
            for (int x = 0; x < stripW; x++) {
                // BGRA; the shadow only darkens the backdrop, the window has no strong magenta.
                if (p[x * 4 + 2] < 90 && p[x * 4 + 0] < 90) return x;
            }
        }
        return -1;
    }

    const uint MOVE = 1, DOWN = 2, UP = 4, ABS = 0x8000;
    public static void Mouse(int x, int y, uint extra) {
        var i = new INPUT[1];
        i[0].mi.dx = (int)Math.Round(x * 65535.0 / (GetSystemMetrics(0) - 1));
        i[0].mi.dy = (int)Math.Round(y * 65535.0 / (GetSystemMetrics(1) - 1));
        i[0].mi.flags = MOVE | ABS | extra;
        SendInput(1, i, Marshal.SizeOf(typeof(INPUT)));
    }

    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
    // A band taller than a table row: one row of pixels can fall between two lines of text and
    // show nothing new on a frame that did change.
    const int BandH = 64;
    static IntPtr bandDc, bandBits;
    static long StripHash(int y) {
        if (bandDc == IntPtr.Zero) {
            bandDc = CreateCompatibleDC(screen);
            var b = new BITMAPINFOHEADER();
            b.size = 40; b.w = stripW; b.h = BandH; b.planes = 1; b.bits = 32;
            SelectObject(bandDc, CreateDIBSection(screen, ref b, 0, out bandBits, IntPtr.Zero, 0));
        }
        BitBlt(bandDc, 0, 0, stripW, BandH, screen, 0, y, 0x00CC0020);
        long h = 1469598103934665603L;
        unsafe {
            int* p = (int*)bandBits;
            int n = stripW * BandH;
            for (int i = 0; i < n; i++) { h ^= p[i]; h *= 1099511628211L; }
        }
        return h;
    }

    // New pictures on screen over a stretch of time, whatever is making them.
    public static double[] Watch(int stripY, double seconds) {
        Thread.CurrentThread.Priority = ThreadPriority.Highest;
        var gaps = new List<double>();
        var c = Stopwatch.StartNew();
        long last = StripHash(stripY);
        double at = -1;
        while (c.Elapsed.TotalSeconds < seconds) {
            long h = StripHash(stripY);
            if (h != last) {
                double t = c.Elapsed.TotalMilliseconds;
                if (at >= 0) gaps.Add(t - at);
                at = t;
                last = h;
            }
        }
        return gaps.ToArray();
    }

    // Every core busy at normal priority: a compile, an e2e run.
    public static void Burn(int threads) {
        for (int i = 0; i < threads; i++) {
            var t = new Thread(() => { double x = 1; while (true) x = Math.Sqrt(x + 1); });
            t.IsBackground = true;
            t.Start();
        }
    }

    public class AnimResult { public double[] FramesPerToggle, GapMs, FirstMs; }

    // Ctrl+key toggles something animated; each read of the composed desktop waits for the next
    // composition, so a read that differs from the last is a new picture on screen.
    public static AnimResult Anim(int clickX, int clickY, int stripY, byte vk, int toggles, double watchSeconds) {
        Thread.CurrentThread.Priority = ThreadPriority.Highest;
        Mouse(clickX, clickY, DOWN);
        Thread.Sleep(30);
        Mouse(clickX, clickY, UP);
        Thread.Sleep(500);
        var frames = new List<double>();
        var gaps = new List<double>();
        var first = new List<double>();
        for (int n = 0; n < toggles; n++) {
            long base0 = StripHash(stripY);
            keybd_event(0x11, 0, 0, IntPtr.Zero);
            keybd_event(vk, 0, 0, IntPtr.Zero);
            keybd_event(vk, 0, 2, IntPtr.Zero);
            keybd_event(0x11, 0, 2, IntPtr.Zero);
            var c = Stopwatch.StartNew();
            long last = base0;
            double lastAt = -1;
            int changes = 0;
            while (c.Elapsed.TotalSeconds < watchSeconds) {
                long h = StripHash(stripY);
                double t = c.Elapsed.TotalMilliseconds;
                if (h != last) {
                    if (lastAt < 0) first.Add(t); else gaps.Add(t - lastAt);
                    lastAt = t;
                    changes++;
                    last = h;
                }
            }
            frames.Add(changes);
            Thread.Sleep(150);
        }
        var r = new AnimResult();
        r.FramesPerToggle = frames.ToArray();
        r.GapMs = gaps.ToArray();
        r.FirstMs = first.ToArray();
        return r;
    }

    // The window's bottom-right corner dragged about: how often its size changes.
    public static double[] Resize(IntPtr hwnd, double seconds, int hz) {
        RECT r0;
        GetWindowRect(hwnd, out r0);
        // Just inside the invisible resize border.
        int x0 = r0.Right - 6, y0 = r0.Bottom - 6;
        Mouse(x0, y0, 0);
        Thread.Sleep(300);
        Mouse(x0, y0, DOWN);
        Thread.Sleep(100);
        var clock = Stopwatch.StartNew();
        var changes = new List<double>();
        bool stop = false;
        var sampler = new Thread(() => {
            RECT last = r0;
            double at = -1;
            while (!Volatile.Read(ref stop)) {
                RECT r;
                GetWindowRect(hwnd, out r);
                if (r.Right != last.Right || r.Bottom != last.Bottom) {
                    double t = clock.Elapsed.TotalMilliseconds;
                    if (at >= 0) changes.Add(t - at);
                    at = t;
                    last = r;
                }
            }
        });
        sampler.Start();
        long step = Stopwatch.Frequency / hz, next = 0;
        while (clock.Elapsed.TotalSeconds < seconds) {
            double phase = (clock.Elapsed.TotalSeconds / 2.0) % 1.0;
            int d = (int)Math.Round(-150 * (phase < 0.5 ? phase * 2 : 2 - phase * 2));
            Mouse(x0 + d, y0 + d / 2, 0);
            next += step;
            while (clock.ElapsedTicks < next) { }
        }
        Volatile.Write(ref stop, true);
        sampler.Join();
        Mouse(x0, y0, UP);
        return changes.ToArray();
    }

    public class Result {
        public int Updates, Injected;
        public double[] IntervalMs, LatencyMs, ScreenIntervalMs, ScreenLatencyMs;
        public int ScreenSamples, ScreenMoves;
        public double StartMs;
    }

    // Time from the injection of a cursor position to the window being there, and between window
    // moves. Sampled by spinning on one core: GetWindowRect reads what the window manager holds.
    public static Result Run(IntPtr hwnd, int grabX, int grabY, double seconds, int hz) {
        RECT r0;
        GetWindowRect(hwnd, out r0);
        Mouse(grabX, grabY, 0);
        Thread.Sleep(300);
        Mouse(grabX, grabY, DOWN);
        Thread.Sleep(100);
        // The press becoming a move, as a hand does it: moving straight away, 1 px per millisecond.
        var startClock = Stopwatch.StartNew();
        double startMs = -1;
        int nudge = 0;
        RECT rs;
        while (startClock.Elapsed.TotalSeconds < 2) {
            nudge++;
            Mouse(grabX + nudge, grabY, 0);
            double until = startClock.Elapsed.TotalMilliseconds + 1;
            while (startClock.Elapsed.TotalMilliseconds < until) {
                GetWindowRect(hwnd, out rs);
                if (startMs < 0 && (rs.Left != r0.Left || rs.Top != r0.Top)) startMs = startClock.Elapsed.TotalMilliseconds;
            }
            if (startMs >= 0 && nudge > 40) break;
        }
        Thread.Sleep(500);
        GetWindowRect(hwnd, out rs);
        int startX = grabX + nudge, startY = grabY;
        int offX = startX - rs.Left, offY = startY - rs.Top;

        var clock = Stopwatch.StartNew();
        var ts = new List<long>(100000);
        var xs = new List<int>(100000);
        var ys = new List<int>(100000);
        bool stop = false;
        var moves = new List<KeyValuePair<long, RECT>>(100000);
        var seen = new List<KeyValuePair<long, int>>(100000);
        int stripY = startY + 300;
        int edge0 = StripEdge(stripY);
        int screenSamples = 0;
        var strip = new Thread(() => {
            int last = edge0;
            while (!Volatile.Read(ref stop)) {
                int e = StripEdge(stripY);
                long t = clock.ElapsedTicks;
                screenSamples++;
                if (e != last && e >= 0) { seen.Add(new KeyValuePair<long, int>(t, e)); last = e; }
            }
        });
        strip.Start();
        var sampler = new Thread(() => {
            RECT last = rs;
            while (!Volatile.Read(ref stop)) {
                RECT r;
                GetWindowRect(hwnd, out r);
                if (r.Left != last.Left || r.Top != last.Top) {
                    moves.Add(new KeyValuePair<long, RECT>(clock.ElapsedTicks, r));
                    last = r;
                }
            }
        });
        sampler.Priority = ThreadPriority.Highest;
        strip.Priority = ThreadPriority.Highest;
        Thread.CurrentThread.Priority = ThreadPriority.Highest;
        sampler.Start();
        long step = Stopwatch.Frequency / hz;
        long next = clock.ElapsedTicks;
        while (clock.Elapsed.TotalSeconds < seconds) {
            double s = clock.Elapsed.TotalSeconds;
            // Constant speed, so a pause on screen is a stall and not the path slowing down.
            double phase = (s / 2.0) % 1.0;
            int x = startX + (int)Math.Round(300 * (phase < 0.25 ? phase * 4 : phase < 0.75 ? 2 - phase * 4 : phase * 4 - 4));
            int y = startY;
            ts.Add(clock.ElapsedTicks); xs.Add(x); ys.Add(y);
            Mouse(x, y, 0);
            next += step;
            while (clock.ElapsedTicks < next) { }
        }
        Thread.Sleep(300);
        Volatile.Write(ref stop, true);
        sampler.Join();
        strip.Join();
        Mouse(xs[xs.Count - 1], ys[ys.Count - 1], UP);

        var intervals = new List<double>();
        var latency = new List<double>();
        double ms = 1000.0 / Stopwatch.Frequency;
        long prev = -1;
        int cursor = 0;
        foreach (var m in moves) {
            if (prev >= 0) intervals.Add((m.Key - prev) * ms);
            prev = m.Key;
            while (cursor + 1 < ts.Count && ts[cursor + 1] <= m.Key) cursor++;
            // The newest injected position the window can be showing.
            for (int i = cursor; i >= 0 && (m.Key - ts[i]) * ms < 100; i--) {
                if (xs[i] - offX == m.Value.Left && ys[i] - offY == m.Value.Top) {
                    latency.Add((m.Key - ts[i]) * ms);
                    break;
                }
            }
        }
        var screenIntervals = new List<double>();
        var screenLatency = new List<double>();
        prev = -1;
        cursor = 0;
        foreach (var m in seen) {
            if (prev >= 0) screenIntervals.Add((m.Key - prev) * ms);
            prev = m.Key;
            while (cursor + 1 < ts.Count && ts[cursor + 1] <= m.Key) cursor++;
            for (int i = cursor; i >= 0 && (m.Key - ts[i]) * ms < 100; i--) {
                if (xs[i] - startX == m.Value - edge0) {
                    screenLatency.Add((m.Key - ts[i]) * ms);
                    break;
                }
            }
        }
        var res = new Result();
        res.StartMs = startMs;
        res.ScreenSamples = screenSamples;
        res.ScreenMoves = seen.Count;
        res.ScreenIntervalMs = screenIntervals.ToArray();
        res.ScreenLatencyMs = screenLatency.ToArray();
        res.Updates = moves.Count;
        res.Injected = ts.Count;
        res.IntervalMs = intervals.ToArray();
        res.LatencyMs = latency.ToArray();
        return res;
    }
}
"@
[Drag]::SetProcessDPIAware() | Out-Null
[Drag]::StripInit()
if ($Load) { [Drag]::Burn($Load) }

function Pct([double[]]$v, [double]$p) {
    if (-not $v -or $v.Count -eq 0) { return 0 }
    $s = [double[]]($v | Sort-Object)
    return [math]::Round($s[[int][math]::Round(($s.Count - 1) * $p)], 2)
}
function Dist([double[]]$v) {
    [ordered]@{ n = $v.Count; p50 = Pct $v .5; p95 = Pct $v .95; p99 = Pct $v .99; max = Pct $v 1 }
}
function Over([double[]]$v, [double]$ms) { if (-not $v) { return 0 }; @($v | Where-Object { $_ -gt $ms }).Count }
function Text($name, $d) { "$name n=$($d.n) p50=$($d.p50)ms p95=$($d.p95)ms p99=$($d.p99)ms max=$($d.max)ms" }

# GPUI's frame timer, one line per frame drawn: "frame duration: 12.3ms".
function Frames([string]$path, [long]$from, [long]$to) {
    if (-not (Test-Path $path)) { return @() }
    $f = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
        $f.Position = $from
        $n = [int]($to - $from)
        $b = New-Object byte[] $n
        [void]$f.Read($b, 0, $n)
    } finally { $f.Close() }
    $text = [Text.Encoding]::UTF8.GetString($b)
    $out = New-Object Collections.Generic.List[double]
    foreach ($m in [regex]::Matches($text, 'frame duration: ([0-9.]+)(ns|\u00b5s|ms|s)')) {
        $x = [double]::Parse($m.Groups[1].Value, [Globalization.CultureInfo]::InvariantCulture)
        switch ($m.Groups[2].Value) { 'ns' { $x /= 1e6 } 'ms' { } 's' { $x *= 1e3 } default { $x /= 1e3 } }
        $out.Add($x)
    }
    return , $out.ToArray()
}
function Length([string]$path) {
    if (-not (Test-Path $path)) { return 0 }
    $f = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { return $f.Length } finally { $f.Close() }
}

foreach ($kv in $SetEnv) {
    $k, $v = $kv -split '=', 2
    [Environment]::SetEnvironmentVariable($k, $v)
}
$cfg = Join-Path $env:TEMP "dataman-drag-bench"
Remove-Item -Recurse -Force $cfg -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $cfg | Out-Null
$env:XDG_CONFIG_HOME = $cfg
$env:DATAMAN_LOG_DIR = Join-Path $cfg "logs"
$env:DATAMAN_NO_NOTIFY = "1"
$env:ZED_MEASUREMENTS = "1"
$err = Join-Path $cfg "stderr.txt"
# GPU load when the NVIDIA tool is there (it ships with the driver): one sample every 200 ms.
$gpuLog = Join-Path $cfg "gpu.csv"
$gpu = $null
if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {
    $gpu = Start-Process nvidia-smi -PassThru -WindowStyle Hidden -ArgumentList '--query-gpu=timestamp,utilization.gpu','--format=csv,noheader,nounits','-lms','200','-f',$gpuLog
}
function Gpu($from, $to) {
    if (-not (Test-Path $gpuLog)) { return -1 }
    $v = @()
    foreach ($line in (Get-Content $gpuLog)) {
        $a = $line -split ','
        if ($a.Count -lt 2) { continue }
        $t = [datetime]::ParseExact($a[0].Trim(), 'yyyy/MM/dd HH:mm:ss.fff', [Globalization.CultureInfo]::InvariantCulture)
        if ($t -ge $from -and $t -le $to) { $v += [double]$a[1] }
    }
    if ($v.Count -eq 0) { return -1 }
    return [math]::Round(($v | Measure-Object -Average).Average, 1)
}
# Behind everything: solid magenta over the whole screen, so the window's edge can be told from it.
$screenW = [Drag]::GetSystemMetrics(0); $screenH = [Drag]::GetSystemMetrics(1)
$back = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-Command',("Add-Type -AssemblyName System.Windows.Forms,System.Drawing; [void][System.Runtime.InteropServices.Marshal]; " +
    "`$f=New-Object Windows.Forms.Form; `$f.FormBorderStyle='None'; `$f.StartPosition='Manual'; `$f.BackColor=[Drawing.Color]::FromArgb(255,0,255); `$f.ShowInTaskbar=`$false; " +
    "`$f.Bounds=New-Object Drawing.Rectangle 0,0,4000,3000; [Windows.Forms.Application]::Run(`$f)")
Start-Sleep -Seconds 2
if ($Reference) {
    # The same drag on a plain window Windows draws and moves by itself.
    $Exe = "powershell"
    $ExeArgs = @('-NoProfile','-Command',("Add-Type -AssemblyName System.Windows.Forms,System.Drawing; " +
        "`$f=New-Object Windows.Forms.Form; `$f.Text='reference'; `$f.StartPosition='Manual'; `$f.BackColor=[Drawing.Color]::FromArgb(20,20,20); " +
        "`$f.Bounds=New-Object Drawing.Rectangle 100,40,1400,900; [Windows.Forms.Application]::Run(`$f)"))
    $GrabY = 14
}
$start = @{ FilePath = $Exe; PassThru = $true; RedirectStandardError = $err; RedirectStandardOutput = (Join-Path $cfg "stdout.txt") }
if ($ExeArgs.Count) { $start.ArgumentList = $ExeArgs }
$p = Start-Process @start
try {
    $until = (Get-Date).AddSeconds(30)
    while ($p.MainWindowHandle -eq 0) {
        if ((Get-Date) -gt $until) { throw "no window" }
        Start-Sleep -Milliseconds 200
        $p.Refresh()
    }
    $h = $p.MainWindowHandle
    # At the same place every run; SWP_NOSIZE|SWP_NOZORDER.
    # Room on both sides: the window's left edge must stay on screen for the read-back.
    [Drag]::SetWindowPos($h, [IntPtr]::Zero, 500, 40, 0, 0, 0x5) | Out-Null
    if ($Maximize) { [Drag]::ShowWindow($h, 3) | Out-Null }
    # Backdrop over whatever else is open, the window over the backdrop. SWP_NOMOVE|NOSIZE|NOACTIVATE.
    $back.Refresh()
    [Drag]::SetWindowPos($back.MainWindowHandle, [IntPtr]::Zero, 0, 0, 0, 0, 0x13) | Out-Null
    [Drag]::SetWindowPos($h, [IntPtr]::Zero, 0, 0, 0, 0, 0x13) | Out-Null
    [Drag]::SetForegroundWindow($h) | Out-Null
    # Away from the window, so nothing hovers.
    [Drag]::Mouse(1, [Drag]::GetSystemMetrics(1) - 2, 0)
    Start-Sleep -Seconds $SettleSeconds

    $r = New-Object Drag+RECT
    [Drag]::GetWindowRect($h, [ref]$r) | Out-Null
    $size = "$($r.Right - $r.Left)x$($r.Bottom - $r.Top)"

    if ($WatchOnly) {
        $gaps = [Drag]::Watch($r.Top + 1000, $WatchOnly)
        Text "new pictures on screen, between" (Dist $gaps)
        "over 25ms: $(Over $gaps 25) of $($gaps.Count)"
        return
    }

    $o0 = Length $err; $c0 = $p.TotalProcessorTime.TotalMilliseconds; $w0 = Get-Date
    Start-Sleep -Seconds $IdleSeconds
    $p.Refresh()
    $oIdle = Length $err; $cIdle = $p.TotalProcessorTime.TotalMilliseconds; $wIdle = Get-Date
    $idleWall = ($wIdle - $w0).TotalMilliseconds
    $idleFrames = Frames $err $o0 $oIdle

    if ($Shot) {
        Add-Type -AssemblyName System.Drawing
        $bmp = New-Object System.Drawing.Bitmap ($r.Right - $r.Left), ($r.Bottom - $r.Top)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
        $g.Dispose()
        $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
    }

    [Drag]::StartCompositorWatch()
    Start-Sleep -Seconds 4
    $restFlush = [Drag]::StopCompositorWatch()

    $p.Refresh()
    $c3 = $p.TotalProcessorTime.TotalMilliseconds; $o3 = Length $err; $w3 = Get-Date
    [Drag]::Hover([int](($r.Left + $r.Right) / 2), [int](($r.Top + $r.Bottom) / 2), $Seconds, $Hz)
    $p.Refresh()
    $c4 = $p.TotalProcessorTime.TotalMilliseconds; $o4 = Length $err; $w4 = Get-Date
    $hoverWall = ($w4 - $w3).TotalMilliseconds
    $hoverFrames = Frames $err $o3 $o4
    Start-Sleep -Seconds 1

    $p.Refresh()
    $c1 = $p.TotalProcessorTime.TotalMilliseconds; $o1 = Length $err; $w1 = Get-Date
    [Drag]::StartCompositorWatch()
    $res = [Drag]::Run($h, $r.Right - $GrabXFromRight, $r.Top + $GrabY, $Seconds, $Hz)
    $dragFlush = [Drag]::StopCompositorWatch()
    $p.Refresh()
    $o2 = Length $err; $c2 = $p.TotalProcessorTime.TotalMilliseconds; $w2 = Get-Date
    $dragWall = ($w2 - $w1).TotalMilliseconds
    $dragFrames = Frames $err $o1 $o2

    # Animation on screen and resize: Dataman only (Ctrl+B is its sidebar), not on the reference.
    $anim = $null; $sized = $null
    if (-not $Reference -and $ExeArgs.Count) {
        Start-Sleep -Seconds 1
        [Drag]::GetWindowRect($h, [ref]$r) | Out-Null
        $midX = [int](($r.Left + $r.Right) / 2)
        $oA = Length $err; $wA = Get-Date; $cA = $p.TotalProcessorTime.TotalMilliseconds
        $anim = [Drag]::Anim($midX, $r.Top + 200, $r.Top + 400, 0x42, 12, 0.45)
        $p.Refresh()
        $animFrames = Frames $err $oA (Length $err)
        $animCpu = ($p.TotalProcessorTime.TotalMilliseconds - $cA) / ((Get-Date) - $wA).TotalMilliseconds * 100
        Start-Sleep -Seconds 1
        [Drag]::GetWindowRect($h, [ref]$r) | Out-Null
        $oR = Length $err; $wR = Get-Date; $cR = $p.TotalProcessorTime.TotalMilliseconds
        $sized = [Drag]::Resize($h, $Seconds, $Hz)
        $p.Refresh()
        $resizeWall = ((Get-Date) - $wR).TotalMilliseconds
        $resizeCpu = ($p.TotalProcessorTime.TotalMilliseconds - $cR) / $resizeWall * 100
        $resizeFrames = Frames $err $oR (Length $err)
    }
    if ($gpu -and -not $gpu.HasExited) { Stop-Process -Id $gpu.Id -Force; Start-Sleep -Milliseconds 300 }
    $report = [ordered]@{
        exe = $Exe; size = $size
        idle = [ordered]@{
            cpu_pct_of_one_core = [math]::Round(($cIdle - $c0) / $idleWall * 100, 2)
            gpu_pct = Gpu $w0 $wIdle
            frames_per_s = [math]::Round($idleFrames.Count / $idleWall * 1000, 1)
            frame = Dist $idleFrames
            compositor = Dist $restFlush
        }
        hover = [ordered]@{
            cpu_pct_of_one_core = [math]::Round(($c4 - $c3) / $hoverWall * 100, 2)
            gpu_pct = Gpu $w3 $w4
            frames_per_s = [math]::Round($hoverFrames.Count / $hoverWall * 1000, 1)
            frame = Dist $hoverFrames
        }
        drag = [ordered]@{
            cursor_positions = $res.Injected
            window_moves = $res.Updates
            first_move_after_press_ms = [math]::Round($res.StartMs, 1)
            moves_per_s = [math]::Round($res.Updates / $Seconds, 1)
            interval = Dist $res.IntervalMs
            latency = Dist $res.LatencyMs
            on_screen_moves_per_s = [math]::Round($res.ScreenMoves / $Seconds, 1)
            on_screen_interval = Dist $res.ScreenIntervalMs
            on_screen_latency = Dist $res.ScreenLatencyMs
            on_screen_stalls_over_25ms = Over $res.ScreenIntervalMs 25
            cpu_pct_of_one_core = [math]::Round(($c2 - $c1) / $dragWall * 100, 2)
            gpu_pct = Gpu $w1 $w2
            frames_per_s = [math]::Round($dragFrames.Count / $dragWall * 1000, 1)
            frame = Dist $dragFrames
            compositor = Dist $dragFlush
            compositor_misses_over_25ms = Over $dragFlush 25
        }
    }
    "size $size"
    "idle: cpu=$($report.idle.cpu_pct_of_one_core)% frames/s=$($report.idle.frames_per_s) gpu=$($report.idle.gpu_pct)%"
    Text "  frame" $report.idle.frame
    Text "  compositor, between frames" $report.idle.compositor
    "hover: cpu=$($report.hover.cpu_pct_of_one_core)% frames/s=$($report.hover.frames_per_s) gpu=$($report.hover.gpu_pct)%"
    Text "  frame" $report.hover.frame
    "drag: first move $($report.drag.first_move_after_press_ms)ms after moving, window moves/s=$($report.drag.moves_per_s) of $($res.Injected) cursor positions, cpu=$($report.drag.cpu_pct_of_one_core)% frames/s=$($report.drag.frames_per_s) gpu=$($report.drag.gpu_pct)%"
    Text "  between window moves" $report.drag.interval
    Text "  cursor to window" $report.drag.latency
    "  on screen: moves/s=$($report.drag.on_screen_moves_per_s), stalls>25ms=$($report.drag.on_screen_stalls_over_25ms), $($res.ScreenSamples) reads"
    Text "    between moves" $report.drag.on_screen_interval
    Text "    cursor to screen" $report.drag.on_screen_latency
    Text "  frame" $report.drag.frame
    (Text "  compositor, between frames" $report.drag.compositor) + " misses>25ms=$($report.drag.compositor_misses_over_25ms)"
    if ($anim) {
        $report.animation = [ordered]@{
            frames_on_screen_per_toggle = Dist $anim.FramesPerToggle
            between_pictures = Dist $anim.GapMs
            key_to_first_picture = Dist $anim.FirstMs
            app_frame = Dist $animFrames
        }
        $report.resize = [ordered]@{
            between_size_changes = Dist $sized
            cpu_pct_of_one_core = [math]::Round($resizeCpu, 2)
            frames_per_s = [math]::Round($resizeFrames.Count / $resizeWall * 1000, 1)
            frame = Dist $resizeFrames
        }
        "animation (sidebar, Ctrl+B): pictures on screen per toggle p50=$($report.animation.frames_on_screen_per_toggle.p50) min=$(Pct $anim.FramesPerToggle 0)"
        Text "  between pictures" $report.animation.between_pictures
        Text "  key to first picture" $report.animation.key_to_first_picture
        Text "  app frame" $report.animation.app_frame
        "resize: cpu=$($report.resize.cpu_pct_of_one_core)% frames/s=$($report.resize.frames_per_s)"
        Text "  between size changes" $report.resize.between_size_changes
        Text "  frame" $report.resize.frame
    }
    if ($Out) { $report | ConvertTo-Json -Depth 5 | Set-Content -Encoding ascii $Out }
} finally {
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    if (-not $back.HasExited) { Stop-Process -Id $back.Id -Force }
    if ($gpu -and -not $gpu.HasExited) { Stop-Process -Id $gpu.Id -Force }
}
