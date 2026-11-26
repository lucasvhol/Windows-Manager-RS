param([string[]]$Open, [int]$Wait = 8, [string]$Prefix = "cap", [string]$OutDir = $env:TEMP)
# Dev helper: launches our own debug build with --open <tool> for each tool and captures only its windows
# with PrintWindow (never the screen).
Set-Location (Split-Path -Parent $PSScriptRoot)
Add-Type -AssemblyName System.Drawing
if (-not ("PWX" -as [type])) {
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class PWX {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  public struct RECT { public int L, T, R, B; }
  public static List<IntPtr> Top(uint pid) { var l = new List<IntPtr>(); EnumWindows((h,x)=>{uint p; GetWindowThreadProcessId(h,out p); if(p==pid && IsWindowVisible(h)) l.Add(h); return true;}, IntPtr.Zero); return l; }
}
'@
}
$scratch = $OutDir
$args2 = @(); foreach ($o in $Open) { $args2 += "--open"; $args2 += $o }
$p = Start-Process target\debug\win-manager.exe -ArgumentList $args2 -PassThru
Start-Sleep $Wait
foreach ($h in [PWX]::Top([uint32]$p.Id)) {
  $sb = New-Object System.Text.StringBuilder 256; [PWX]::GetWindowText($h, $sb, 256) | Out-Null; $t = $sb.ToString()
  $r = New-Object PWX+RECT; [PWX]::GetWindowRect($h, [ref]$r) | Out-Null
  $w = $r.R - $r.L; $hh = $r.B - $r.T
  if ($w -lt 200 -or $hh -lt 200 -or $t -notmatch " - ") { continue }
  $name = ($t -replace '^.* - ', '') -replace '[^A-Za-z0-9]', ''
  $bmp = New-Object System.Drawing.Bitmap $w, $hh
  $g = [System.Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); [PWX]::PrintWindow($h, $hdc, 2) | Out-Null
  $g.ReleaseHdc($hdc); $out = "$scratch\$Prefix-$name.png"; $bmp.Save($out); $g.Dispose(); $bmp.Dispose(); $out
}
Stop-Process -Id $p.Id -Force
