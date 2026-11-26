param([string]$Font, [string]$Out)
# Regenerates app\assets\app.ico and app.png. Run with Windows PowerShell 5.1:
#   powershell -File scripts\make-icon.ps1 -Font crates\core\assets\Phosphor.ttf -Out app\assets\app.ico
# Renders the Phosphor wrench glyph (U+E5D4) white on a rounded accent-blue square and
# writes a multi-size .ico (PNG-compressed entries).
Add-Type -AssemblyName System.Drawing
$pfc = New-Object System.Drawing.Text.PrivateFontCollection
$pfc.AddFontFile($Font)
$family = $pfc.Families[0]
$glyph = [string][char]0xE5D4
$sizes = 16, 24, 32, 48, 64, 128, 256
$pngs = @()
foreach ($s in $sizes) {
  $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'; $g.TextRenderingHint = 'AntiAliasGridFit'; $g.Clear([System.Drawing.Color]::Transparent)
  $r = [Math]::Max(2, [int]($s * 0.22)); $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $path.AddArc(0, 0, 2*$r, 2*$r, 180, 90); $path.AddArc($s-2*$r-1, 0, 2*$r, 2*$r, 270, 90)
  $path.AddArc($s-2*$r-1, $s-2*$r-1, 2*$r, 2*$r, 0, 90); $path.AddArc(0, $s-2*$r-1, 2*$r, 2*$r, 90, 90); $path.CloseFigure()
  $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 56, 122, 222))), $path)
  $fmt = New-Object System.Drawing.StringFormat; $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'
  $gp = New-Object System.Drawing.Drawing2D.GraphicsPath; $gp.AddString($glyph, $family, 0, [single]($s * 0.68), [System.Drawing.RectangleF]::new(0, [single]($s*0.03), $s, $s), $fmt); $g.FillPath([System.Drawing.Brushes]::White, $gp)
  $g.Dispose()
  $ms = New-Object System.IO.MemoryStream; $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  if ($s -eq 256) { $bmp.Save([System.IO.Path]::ChangeExtension($Out, ".png"), [System.Drawing.Imaging.ImageFormat]::Png) }
  $pngs += ,($ms.ToArray()); $bmp.Dispose()
}
$fs = [System.IO.File]::Create($Out); $w = New-Object System.IO.BinaryWriter $fs
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
  $s = $sizes[$i]; $dim = if ($s -ge 256) { 0 } else { $s }
  $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
  $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$w.Close()
"wrote $Out"


