param(
    [int]$ProcessId,
    [string]$OutFile,
    [int]$Margin = 0
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
public static class Win32Glass {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();

    // The process owns helper windows (an off-screen 16x16 winit placeholder,
    // tooltips); the launcher is the largest visible one.
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
}
"@

$proc = Get-Process -Id $ProcessId -ErrorAction Stop
$hwnd = [Win32Glass]::LargestVisibleWindow([uint32]$ProcessId)
if ($hwnd -eq [IntPtr]::Zero) { throw "no visible window for process $ProcessId" }
# The launcher may sit behind the developer's browser/editor. Raise it above
# every window for the capture, then return it to the normal z-order; otherwise
# CopyFromScreen photographs whatever happens to be on top.
$previousForeground = [Win32Glass]::GetForegroundWindow()
[void][Win32Glass]::ShowWindow($hwnd, 9)
[void][Win32Glass]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0040)
[void][Win32Glass]::BringWindowToTop($hwnd)
[void][Win32Glass]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 900

$rect = New-Object RECT
[Win32Glass]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$left = [Math]::Max(0, $rect.Left - $Margin)
$top = [Math]::Max(0, $rect.Top - $Margin)
$width = [Math]::Min($screen.Width - $left, ($rect.Right - $rect.Left) + 2 * $Margin)
$height = [Math]::Min($screen.Height - $top, ($rect.Bottom - $rect.Top) + 2 * $Margin)
$bmp = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bmp)
$graphics.CopyFromScreen($left, $top, 0, 0, (New-Object System.Drawing.Size $width, $height))
$bmp.Save($OutFile, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bmp.Dispose()

# Back to the normal z-order and, when possible, to the window the user had.
[void][Win32Glass]::SetWindowPos($hwnd, [IntPtr](-2), 0, 0, 0, 0, 0x0001 -bor 0x0002)
if ($previousForeground -ne [IntPtr]::Zero) {
    [void][Win32Glass]::SetForegroundWindow($previousForeground)
}
"captured window {0},{1} {2}x{3} -> {4}" -f $rect.Left, $rect.Top, $width, $height, $OutFile
