param(
    [int]$ProcessId,
    [string]$OutFile,
    [int]$Margin = 0
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
public static class Win32Glass {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
}
"@

$proc = Get-Process -Id $ProcessId -ErrorAction Stop
$hwnd = $proc.MainWindowHandle
if ($hwnd -eq 0) { throw "window handle is not available" }
[Win32Glass]::ShowWindow($hwnd, 9) | Out-Null
[Win32Glass]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 700

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
"captured window {0},{1} {2}x{3} -> {4}" -f $rect.Left, $rect.Top, $width, $height, $OutFile
