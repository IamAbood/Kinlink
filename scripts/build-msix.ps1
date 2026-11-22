# Builds dist\Kinlink.msix for the Microsoft Store (unsigned: the Store signs it on upload).
# Usage: powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1 [-Exe path\to\Kinlink.exe] [-Register]
#   -Register  installs the unpacked folder for a local test instead (needs Developer Mode; no signing).
# Identity values: from Partner Center > the app > Product identity. Placeholders work for local tests.
param(
    [string]$Exe = "",
    [string]$Version = "",  # e.g. 0.29.8.0; default: from native/Cargo.toml
    [string]$Name = "AlsaouServices.KinLinkRemote",
    [string]$Publisher = "CN=22BDAD11-4BA0-483F-8AF0-6138E28D0E8C",
    [string]$PublisherDisplay = "Alsaou Services",
    [switch]$Register
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $Exe) { $Exe = "$root\native\target\release\pclone.exe" }

# Store versions are 4 numbers and the last must be 0: 0.29.5-beta -> 0.29.5.0
$ver = (Select-String -Path "$root\native\Cargo.toml" -Pattern '^version = "(\d+)\.(\d+)\.(\d+)').Matches[0].Groups
$version = if ($Version) { $Version } else { "$($ver[1].Value).$($ver[2].Value).$($ver[3].Value).0" }

$layout = "$root\dist\msix"
if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
New-Item -ItemType Directory -Force "$layout\Assets" | Out-Null
Copy-Item $Exe "$layout\Kinlink.exe"

# Logos from the 256 px app icon.
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile("$root\native\assets\kinlink-256.png")
function Save-Logo($file, $w, $h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = 'HighQualityBicubic'
    $s = [Math]::Min($w, $h)
    $g.DrawImage($src, [int](($w - $s) / 2), [int](($h - $s) / 2), $s, $s)
    $bmp.Save("$layout\Assets\$file", [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
}
Save-Logo "StoreLogo.png" 50 50
Save-Logo "Square44x44Logo.png" 44 44
Save-Logo "Square150x150Logo.png" 150 150
Save-Logo "Wide310x150Logo.png" 310 150
$src.Dispose()

$m = Get-Content "$root\installer\msix\AppxManifest.xml" -Raw
$m = $m.Replace('$NAME$', $Name).Replace('$PUBLISHER$', $Publisher).Replace('$PUBLISHER_DISPLAY$', $PublisherDisplay).Replace('$VERSION$', $version)
[IO.File]::WriteAllText("$layout\AppxManifest.xml", $m)  # UTF-8 without BOM

if ($Register) {
    Add-AppxPackage -Register "$layout\AppxManifest.xml"
    Write-Host "Registered Kinlink $version for testing (remove: Get-AppxPackage $Name | Remove-AppxPackage)"
    return
}

$makeappx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" | Sort-Object FullName | Select-Object -Last 1
if (-not $makeappx) { throw "makeappx.exe not found (install the Windows SDK)" }
& $makeappx.FullName pack /o /d $layout /p "$root\dist\Kinlink.msix"
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }
Write-Host "Built dist\Kinlink.msix ($version)"
