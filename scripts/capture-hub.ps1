param([int[]]$Category = @(0), [int]$Wait = 8, [string]$Prefix = "hub", [string]$OutDir = $env:TEMP, [int]$Width = 0, [int]$Height = 0)
# Dev helper: starts our debug build on each rail entry (--category <n>, 0 = Home) and
# captures only the hub window with PrintWindow (never the screen). -Width/-Height
# resize the window first, to check narrow layouts.
Set-Location (Split-Path -Parent $PSScriptRoot)
Add-Type -AssemblyName System.Drawing
if (-not ("PWH" -as [type])) {
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class PWH {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int hh, bool repaint);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  public struct RECT { public int L, T, R, B; }
  public static List<IntPtr> Top(uint pid) { var l = new List<IntPtr>(); EnumWindows((h,x)=>{uint p; GetWindowThreadProcessId(h,out p); if(p==pid && IsWindowVisible(h)) l.Add(h); return true;}, IntPtr.Zero); return l; }
}
'@
}
foreach ($c in $Category) {
  $p = Start-Process target\debug\win-manager.exe -ArgumentList "--category", "$c" -PassThru
  Start-Sleep $Wait
  foreach ($h in [PWH]::Top([uint32]$p.Id)) {
    $sb = New-Object System.Text.StringBuilder 256; [PWH]::GetWindowText($h, $sb, 256) | Out-Null
    if ($sb.ToString() -ne "Win Manager RS") { continue }
    $r = New-Object PWH+RECT; [PWH]::GetWindowRect($h, [ref]$r) | Out-Null
    if ($Width -gt 0) { [PWH]::MoveWindow($h, $r.L, $r.T, $Width, $Height, $true) | Out-Null; Start-Sleep 2; [PWH]::GetWindowRect($h, [ref]$r) | Out-Null }
    $w = $r.R - $r.L; $hh = $r.B - $r.T
    if ($w -lt 200 -or $hh -lt 200) { continue }
    $bmp = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [PWH]::PrintWindow($h, $hdc, 2) | Out-Null
    $g.ReleaseHdc($hdc); $out = Join-Path $OutDir "$Prefix-$c.png"; $bmp.Save($out); $g.Dispose(); $bmp.Dispose(); $out
  }
  Stop-Process -Id $p.Id -Force
}
