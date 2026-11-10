<#
.SYNOPSIS
    Measures the private bytes (commit) and working set of a release build.

.DESCRIPTION
    Private bytes show how much memory the process really committed; working set
    also counts shared pages and follows activity, so both are reported. Only a
    `--release` build is meaningful: a debug build has more code, debug info and
    a different allocator.

    Without switches the script starts the launcher, waits for the window,
    samples for `-Seconds` and prints min/median/p95/max. With `-Scenarios` it
    runs the battery from the memory checklist against a single process: idle,
    the largest view (maximised window), the way back to the idle number, and
    twenty minimise/restore cycles to catch a leak.

.EXAMPLE
    ./tools/Measure-Memory.ps1 -Label idle -Seconds 30
    ./tools/Measure-Memory.ps1 -Scenarios -IdleSeconds 30
#>
param(
    [string]$Exe = "target\release\OdiumLauncher.exe",
    [int]$Seconds = 30,
    [int]$IntervalMs = 500,
    [string]$Label = "idle",
    [int]$ProcessId = 0,
    [int]$WarmupSeconds = 3,
    [switch]$KeepRunning,
    [switch]$Scenarios,
    [int]$IdleSeconds = 30,
    [int]$PeakSeconds = 15,
    [int]$Cycles = 20
)

$ErrorActionPreference = 'Stop'
$root = (Get-Item -LiteralPath (Join-Path $PSScriptRoot '..')).FullName
$exePath = if ([System.IO.Path]::IsPathRooted($Exe)) { $Exe } else { Join-Path $root $Exe }

if ($ProcessId -gt 0) {
    $process = Get-Process -Id $ProcessId
} else {
    if (-not (Test-Path -LiteralPath $exePath)) {
        throw "release build not found: $exePath (run cargo build --release)"
    }
    $process = Start-Process -FilePath $exePath -PassThru
    Write-Host "Started $($process.Id) from $exePath"
}

if ($Seconds -lt 1) { throw '-Seconds must be at least 1' }

function Get-Percentile {
    param([double[]]$Sorted, [double]$Fraction)
    if ($Sorted.Count -eq 0) { return 0 }
    $index = [math]::Min($Sorted.Count - 1, [math]::Ceiling($Fraction * $Sorted.Count) - 1)
    return $Sorted[[int][math]::Max(0, $index)]
}

function Get-Stats {
    param([System.Collections.Generic.List[double]]$Values)
    $sorted = [double[]]$Values.ToArray()
    [array]::Sort($sorted)
    return [pscustomobject]@{
        Samples = $sorted.Count
        Min     = [math]::Round($sorted[0], 1)
        Median  = [math]::Round((Get-Percentile -Sorted $sorted -Fraction 0.5), 1)
        P95     = [math]::Round((Get-Percentile -Sorted $sorted -Fraction 0.95), 1)
        Max     = [math]::Round($sorted[$sorted.Count - 1], 1)
    }
}

function Measure-Window {
    param([int]$SampleSeconds)
    $private = [System.Collections.Generic.List[double]]::new()
    $working = [System.Collections.Generic.List[double]]::new()
    $deadline = (Get-Date).AddSeconds($SampleSeconds)
    while ((Get-Date) -lt $deadline) {
        $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
        if (-not $current) { break }
        $private.Add($current.PrivateMemorySize64 / 1MB)
        $working.Add($current.WorkingSet64 / 1MB)
        Start-Sleep -Milliseconds $IntervalMs
    }
    if ($private.Count -eq 0) {
        throw "process $($process.Id) exited before any sample was taken"
    }
    return [pscustomobject]@{
        Private = Get-Stats -Values $private
        Working = Get-Stats -Values $working
    }
}

function Write-Stats {
    param([string]$Name, [pscustomobject]$Stats)
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f $Name, $Stats.Samples, $Stats.Min, $Stats.Median, $Stats.P95, $Stats.Max)
}

# The first frames allocate the GPU buffers and the font atlas, so a sample
# taken before the window exists would be misleadingly small. Wait for the
# window handle, then give the settled UI its own warm-up.
$warmupDeadline = (Get-Date).AddSeconds([math]::Max(1, $WarmupSeconds))
while ((Get-Date) -lt $warmupDeadline) {
    $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
    if (-not $current) { throw "process $($process.Id) exited during warm-up" }
    if ($current.MainWindowHandle -ne 0) { break }
    Start-Sleep -Milliseconds 200
}
# Let the first frames finish (font atlas, GPU buffers) before sampling.
Start-Sleep -Seconds 2

if (-not $Scenarios) {
    Write-Host "Sampling '$Label' for $Seconds s (every $IntervalMs ms)..."
    $result = Measure-Window -SampleSeconds $Seconds
    Write-Host ""
    Write-Host "== $Label (MiB) =="
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f 'metric', 'samples', 'min', 'median', 'p95', 'max')
    Write-Stats -Name 'private bytes' -Stats $result.Private
    Write-Stats -Name 'working set' -Stats $result.Working
} else {
    Add-Type -Namespace OdiumMeasure -Name Window -MemberDefinition @'
public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

// The process owns small helper windows; the launcher is the largest visible one.
public static IntPtr LargestVisibleWindow(uint pid) {
    IntPtr best = IntPtr.Zero;
    long bestArea = 0;
    EnumWindows((hWnd, lParam) => {
        uint owner;
        GetWindowThreadProcessId(hWnd, out owner);
        if (owner != pid || !IsWindowVisible(hWnd)) return true;
        RECT rect;
        if (!GetWindowRect(hWnd, out rect)) return true;
        long area = (long)(rect.Right - rect.Left) * (rect.Bottom - rect.Top);
        if (area > bestArea) { bestArea = area; best = hWnd; }
        return true;
    }, IntPtr.Zero);
    return best;
}
'@
    $hwnd = [OdiumMeasure.Window]::LargestVisibleWindow([uint32]$process.Id)
    if ($hwnd -eq [IntPtr]::Zero) { throw 'launcher window not found' }
    [void][OdiumMeasure.Window]::SetForegroundWindow($hwnd)

    Write-Host ""
    Write-Host "== Odium Launcher memory scenarios (MiB) =="
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f 'scenario', 'samples', 'min', 'median', 'p95', 'max')

    Write-Host "idle, $IdleSeconds s ..."
    $idle = Measure-Window -SampleSeconds $IdleSeconds
    Write-Stats -Name 'idle' -Stats $idle.Private
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f '  idle working set', $idle.Working.Samples, $idle.Working.Min, $idle.Working.Median, $idle.Working.P95, $idle.Working.Max)

    # Largest view: the window fills the screen, so egui tessellates and paints
    # the biggest surface it ever produces in this app.
    Write-Host "maximised window, $PeakSeconds s ..."
    [void][OdiumMeasure.Window]::ShowWindow($hwnd, 3)
    Start-Sleep -Seconds 1
    $peak = Measure-Window -SampleSeconds $PeakSeconds
    Write-Stats -Name 'maximised' -Stats $peak.Private
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f '  maximised working set', $peak.Working.Samples, $peak.Working.Min, $peak.Working.Median, $peak.Working.P95, $peak.Working.Max)

    # Back to the normal window: the number must fall back to idle + 20%.
    Write-Host "restored window, $PeakSeconds s ..."
    [void][OdiumMeasure.Window]::ShowWindow($hwnd, 9)
    Start-Sleep -Seconds 1
    $restored = Measure-Window -SampleSeconds $PeakSeconds
    Write-Stats -Name 'restored' -Stats $restored.Private
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f '  restored working set', $restored.Working.Samples, $restored.Working.Min, $restored.Working.Median, $restored.Working.P95, $restored.Working.Max)

    # Leak check: the window is hidden and shown many times. A launcher that
    # rebuilt its buffers per show would grow here.
    Write-Host "$Cycles minimise/restore cycles ..."
    for ($i = 0; $i -lt $Cycles; $i++) {
        [void][OdiumMeasure.Window]::ShowWindow($hwnd, 6)
        Start-Sleep -Milliseconds 120
        [void][OdiumMeasure.Window]::ShowWindow($hwnd, 9)
        Start-Sleep -Milliseconds 180
    }
    Start-Sleep -Seconds 1
    $afterCycles = Measure-Window -SampleSeconds 10
    Write-Stats -Name "after $Cycles cycles" -Stats $afterCycles.Private
    Write-Host ("{0,-22} {1,8} {2,8} {3,8} {4,8} {5,8}" -f '  after cycles working set', $afterCycles.Working.Samples, $afterCycles.Working.Min, $afterCycles.Working.Median, $afterCycles.Working.P95, $afterCycles.Working.Max)

    $allowed = [math]::Round($idle.Private.Median * 1.2, 1)
    Write-Host ""
    Write-Host ("Return budget: {0} MiB (idle median + 20%); restored median {1} MiB" -f $allowed, $restored.Private.Median)
    Write-Host ("Leak budget:   {0} MiB after {1} cycles; measured {2} MiB" -f $allowed, $Cycles, $afterCycles.Private.Median)
}

if (-not $KeepRunning -and $ProcessId -eq 0) {
    Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
}
