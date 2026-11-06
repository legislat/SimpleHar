# Generates docs/assets/social-preview.png: a 1280x300 social preview image.
# Run it under Windows PowerShell 5.1. System.Drawing needs the .NET Framework
# desktop runtime, which pwsh 7 may lack; the script then stops with a clear
# message instead of a throw.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-social-preview.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-social-preview.ps1 -OutFile <path.png>

#Requires -Version 5

[CmdletBinding()]
param(
    [string]$OutFile
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($OutFile)) {
    $OutFile = Join-Path $PSScriptRoot '..\docs\assets\social-preview.png'
}

try {
    Add-Type -AssemblyName System.Drawing
} catch {
    Write-Output 'run under Windows PowerShell'
    exit 1
}

$width = 1280
$height = 300
$bgColor = [System.Drawing.Color]::FromArgb(255, 14, 19, 24)
$titleColor = [System.Drawing.Color]::FromArgb(255, 232, 238, 244)
$taglineColor = [System.Drawing.Color]::FromArgb(255, 142, 155, 168)
$wordmark = 'SimpleHarn'
$tagline = 'Many local engines. One GPU. One door.'

$bmp = New-Object System.Drawing.Bitmap(
    $width, $height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias
$g.Clear($bgColor)

$titleFont = New-Object System.Drawing.Font('Segoe UI', 86,
    [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
$taglineFont = New-Object System.Drawing.Font('Segoe UI', 33,
    [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$titleBrush = New-Object System.Drawing.SolidBrush($titleColor)
$taglineBrush = New-Object System.Drawing.SolidBrush($taglineColor)
$sf = New-Object System.Drawing.StringFormat
$sf.Alignment = [System.Drawing.StringAlignment]::Center
$sf.LineAlignment = [System.Drawing.StringAlignment]::Center

$titleRect = New-Object System.Drawing.RectangleF(0, 42, $width, 140)
$taglineRect = New-Object System.Drawing.RectangleF(0, 186, $width, 74)
$g.DrawString($wordmark, $titleFont, $titleBrush, $titleRect, $sf)
$g.DrawString($tagline, $taglineFont, $taglineBrush, $taglineRect, $sf)

$outPath = [System.IO.Path]::GetFullPath($OutFile)
$outDir = Split-Path -Parent $outPath
if (-not (Test-Path -LiteralPath $outDir)) {
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
}
if (Test-Path -LiteralPath $outPath) {
    Remove-Item -LiteralPath $outPath -Force
}
$bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)

$g.Dispose()
$bmp.Dispose()
$sf.Dispose()
$taglineBrush.Dispose()
$titleBrush.Dispose()
$taglineFont.Dispose()
$titleFont.Dispose()

$check = [System.Drawing.Image]::FromFile($outPath)
$readback = '{0}x{1}' -f $check.Width, $check.Height
$check.Dispose()
$bytes = (Get-Item -LiteralPath $outPath).Length
Write-Output ("wrote {0}: {1}, {2} bytes" -f $outPath, $readback, $bytes)
