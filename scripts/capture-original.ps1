param(
    [Parameter(Mandatory)] [string]$Exe,
    [string]$OutDir = $env:TEMP,
    [string]$Prefix = "",
    # Window-relative points ("x,y") clicked one after another; a capture follows each click.
    # Use them only on tabs and navigation entries, never on action buttons.
    [string[]]$Clicks = @(),
    [int]$Wait = 6,
    [int]$ClickWait = 2,
    # Keep the process running afterwards (to capture more pages in another call).
    [switch]$Keep,
    [int]$AttachPid = 0
)
# Dev helper for comparing with the original Yamicsoft Windows Manager: starts one of its tool
# exes, captures its windows with PrintWindow (never the screen), optionally posts clicks to
# tabs (the real mouse is not moved) and captures again, then closes it.
Add-Type -AssemblyName System.Drawing
if (-not ("OrigCap" -as [type])) {
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class OrigCap {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern IntPtr ChildWindowFromPointEx(IntPtr h, POINT p, uint flags);
  [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  public struct RECT { public int L, T, R, B; }
  public struct POINT { public int X, Y; }
  public static List<IntPtr> Top(uint pid) { var l = new List<IntPtr>(); EnumWindows((h,x)=>{uint p; GetWindowThreadProcessId(h,out p); if(p==pid && IsWindowVisible(h)) l.Add(h); return true;}, IntPtr.Zero); return l; }
  // Deepest visible, enabled child under a screen point, without relying on z-order on screen.
  public static IntPtr Deepest(IntPtr top, int sx, int sy) {
    IntPtr cur = top;
    for (int i = 0; i < 32; i++) {
      POINT p; p.X = sx; p.Y = sy; ScreenToClient(cur, ref p);
      IntPtr c = ChildWindowFromPointEx(cur, p, 1 | 2 | 4); // skip invisible, disabled, transparent
      if (c == IntPtr.Zero || c == cur) break;
      cur = c;
    }
    return cur;
  }
  public static void Click(IntPtr top, int sx, int sy) {
    IntPtr t = Deepest(top, sx, sy);
    POINT p; p.X = sx; p.Y = sy; ScreenToClient(t, ref p);
    IntPtr l = (IntPtr)((p.Y << 16) | (p.X & 0xFFFF));
    PostMessage(t, 0x0200, IntPtr.Zero, l); // WM_MOUSEMOVE
    PostMessage(t, 0x0201, (IntPtr)1, l);   // WM_LBUTTONDOWN
    PostMessage(t, 0x0202, IntPtr.Zero, l); // WM_LBUTTONUP
  }
}
'@
}

$name = [IO.Path]::GetFileNameWithoutExtension($Exe)
if ($Prefix -eq "") { $Prefix = $name }
if ($Exe -match '^(LiveUpdate|LiveUpdateCopy|LaunchTaskCommand|CMMultipleFiles)\.exe$') { throw "$Exe is not captured (it acts on its own)" }
New-Item -ItemType Directory -Force $OutDir | Out-Null

if ($AttachPid -ne 0) {
    $p = Get-Process -Id $AttachPid
} else {
    $p = Start-Process (Join-Path "C:\Program Files\Yamicsoft\Windows Manager" $Exe) -PassThru
    Start-Sleep $Wait
}

function Get-Main {
    $best = [IntPtr]::Zero; $area = 0
    foreach ($h in [OrigCap]::Top([uint32]$p.Id)) {
        $r = New-Object OrigCap+RECT; [OrigCap]::GetWindowRect($h, [ref]$r) | Out-Null
        $a = ($r.R - $r.L) * ($r.B - $r.T)
        if ($a -gt $area) { $area = $a; $best = $h }
    }
    $best
}

function Save-Windows([string]$tag) {
    $i = 0
    foreach ($h in [OrigCap]::Top([uint32]$p.Id)) {
        $r = New-Object OrigCap+RECT; [OrigCap]::GetWindowRect($h, [ref]$r) | Out-Null
        $w = $r.R - $r.L; $hh = $r.B - $r.T
        if ($w -lt 120 -or $hh -lt 80) { continue }
        $bmp = New-Object System.Drawing.Bitmap $w, $hh
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $dc = $g.GetHdc(); [OrigCap]::PrintWindow($h, $dc, 2) | Out-Null; $g.ReleaseHdc($dc); $g.Dispose()
        $suffix = if ($i -eq 0) { "" } else { "-w$i" }
        $file = Join-Path $OutDir "$Prefix-$tag$suffix.png"
        $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
        $sb = New-Object System.Text.StringBuilder 256; [OrigCap]::GetWindowText($h, $sb, 256) | Out-Null
        "$file  ($w x $hh, '$($sb.ToString())', window at $($r.L),$($r.T))"
        $i++
    }
}

Save-Windows "0"
$n = 1
foreach ($c in $Clicks) {
    $main = Get-Main
    $r = New-Object OrigCap+RECT; [OrigCap]::GetWindowRect($main, [ref]$r) | Out-Null
    $xy = $c.Split(',') | ForEach-Object { [int]$_ }
    [OrigCap]::Click($main, $r.L + $xy[0], $r.T + $xy[1])
    Start-Sleep $ClickWait
    Save-Windows "$n"
    $n++
}
if ($Keep) {
    "kept running: pid $($p.Id)"
} else {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
}
