<#
.SYNOPSIS
  Build and package Rust Cut Pro for Windows x64.

.DESCRIPTION
  1. cargo build --release (unless -SkipBuild)
  2. stage exe + VC++ runtime DLLs + README/CHANGELOG/licenses/notices
  3. optionally code-sign the exe (see "Code signing" below)
  4. dist\RustCutPro-v<ver>-win64-portable.zip
  5. dist\RustCutPro-v<ver>-win64-setup.exe if Inno Setup 6 (ISCC.exe) is installed
  6. dist\SHA256SUMS.txt

  Code signing (skipped when none of these are set):
    RCP_SIGN_PFX + RCP_SIGN_PFX_PASSWORD   .pfx file
    RCP_SIGN_CERT_THUMBPRINT               certificate already in the user store
    RCP_TRUSTED_SIGNING_DLIB + RCP_TRUSTED_SIGNING_METADATA
                                           Azure Trusted Signing (Azure.CodeSigning.Dlib.dll
                                           and its metadata.json)
  RCP_SIGN_TIMESTAMP_URL overrides the RFC 3161 timestamp server.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package_windows.ps1
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$Features = "",
    # package this exe instead of target\release\rust-cut-pro.exe (implies -SkipBuild)
    [string]$Exe = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# ---- version ---------------------------------------------------------------
$cargoToml = Get-Content (Join-Path $Root "Cargo.toml") -Raw
if ($cargoToml -notmatch '(?m)^\s*version\s*=\s*"([^"]+)"') { throw "workspace version not found in Cargo.toml" }
$Version = $Matches[1]
$Name = "RustCutPro-v$Version-win64"
$Dist = Join-Path $Root "dist"
$Stage = Join-Path $Dist "stage"
Step "Rust Cut Pro $Version"

# ---- build -----------------------------------------------------------------
if ($Exe) { $SkipBuild = $true; $Exe = (Resolve-Path $Exe).Path }
if (-not $SkipBuild) {
    Step "cargo build --release"
    $cargoArgs = @("build", "--release", "-p", "nle-ui", "--bin", "rust-cut-pro")
    if ($Features) { $cargoArgs += @("--no-default-features", "--features", $Features) }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
if (-not $Exe) { $Exe = Join-Path $Root "target\release\rust-cut-pro.exe" }
if (-not (Test-Path $Exe)) { throw "missing $Exe (build first or drop -SkipBuild)" }

# ---- VC++ runtime (app-local, redistributable per Microsoft's redist list) --
function Find-VcRuntimeDir {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    $roots = @()
    if (Test-Path $vswhere) {
        $roots += & $vswhere -products * -property installationPath
    }
    $roots += Get-ChildItem (Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\2022") -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName
    $roots += Get-ChildItem (Join-Path $env:ProgramFiles "Microsoft Visual Studio\2022") -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName
    foreach ($r in ($roots | Select-Object -Unique)) {
        $redist = Join-Path $r "VC\Redist\MSVC"
        if (-not (Test-Path $redist)) { continue }
        $crt = Get-ChildItem $redist -Directory | Where-Object { $_.Name -match '^\d' } | Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Get-ChildItem (Join-Path $_.FullName "x64") -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue } |
            Select-Object -First 1
        if ($crt) { return $crt.FullName }
    }
    return $null
}
$VcDlls = @("msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll")

# ---- stage -----------------------------------------------------------------
Step "Staging files"
if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force $Stage | Out-Null
Copy-Item $Exe $Stage
foreach ($f in @("README.md", "CHANGELOG.md", "LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_NOTICES.md")) {
    $p = Join-Path $Root $f
    if (-not (Test-Path $p)) { throw "missing $f" }
    Copy-Item $p $Stage
}
# Discord art asset: Help > Discord Rich Presence > "Open Icon Folder" looks for <exe dir>\assets
$StageAssets = Join-Path $Stage "assets"
New-Item -ItemType Directory -Force $StageAssets | Out-Null
Copy-Item (Join-Path $Root "assets\rust-cut-pro-discord.png") $StageAssets
$crtDir = Find-VcRuntimeDir
if ($crtDir) {
    foreach ($d in $VcDlls) { Copy-Item (Join-Path $crtDir $d) $Stage }
    Write-Host "    VC++ runtime from $crtDir"
} else {
    Write-Warning "VC++ redist folder not found: the package will need the 'Microsoft Visual C++ 2015-2022 Redistributable (x64)' installed on the target PC."
}

# ---- code signing ----------------------------------------------------------
# scripts\sign_file.ps1 reads the RCP_SIGN_* variables (it is also Inno Setup's
# SignTool, so the uninstaller inside setup.exe gets signed too).
$SignScript = Join-Path $PSScriptRoot "sign_file.ps1"
$SignConfigured = [bool](($env:RCP_TRUSTED_SIGNING_DLIB -and $env:RCP_TRUSTED_SIGNING_METADATA) -or $env:RCP_SIGN_PFX -or $env:RCP_SIGN_CERT_THUMBPRINT)
function Invoke-Sign([string]$File) {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $SignScript -File $File
    if ($LASTEXITCODE -ne 0) { throw "code signing failed for $File" }
}
Step "Code signing"
Invoke-Sign (Join-Path $Stage "rust-cut-pro.exe")

# ---- portable zip ----------------------------------------------------------
Step "Portable zip"
$Zip = Join-Path $Dist "$Name-portable.zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
# the zip holds one top-level folder so "Extract All" does not scatter files
$ZipRoot = Join-Path $Dist "zip-root"
if (Test-Path $ZipRoot) { Remove-Item $ZipRoot -Recurse -Force }
$Inner = Join-Path $ZipRoot "RustCutPro-v$Version"
New-Item -ItemType Directory -Force $Inner | Out-Null
Copy-Item (Join-Path $Stage "*") $Inner -Recurse
Compress-Archive -Path $Inner -DestinationPath $Zip -CompressionLevel Optimal
Remove-Item $ZipRoot -Recurse -Force
Write-Host "    $Zip"

# ---- installer -------------------------------------------------------------
Step "Installer (Inno Setup)"
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source)
if (-not $iscc) {
    $iscc = @(
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}
$Setup = Join-Path $Dist "$Name-setup.exe"
if (Test-Path $Setup) { Remove-Item $Setup -Force }
if ($iscc) {
    $isccArgs = @("/Q", "/DAppVersion=$Version", "/DStageDir=$Stage", "/DOutputDir=$Dist")
    if ($SignConfigured) {
        # ISCC runs this for setup.exe and the embedded uninstaller; $q is a
        # quote and $f the quoted file name (Inno Setup expands both)
        $isccArgs += ('/Srcp=powershell.exe -NoProfile -ExecutionPolicy Bypass -File $q' + $SignScript + '$q -File $f')
        $isccArgs += "/DSignSetup"
    }
    & $iscc @isccArgs (Join-Path $Root "installer\rust_cut_pro.iss")
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
    Write-Host "    $Setup"
} else {
    Write-Host "    Inno Setup 6 not found - skipped setup.exe (install it: winget install JRSoftware.InnoSetup)" -ForegroundColor Yellow
}

# ---- checksums -------------------------------------------------------------
Step "SHA256SUMS.txt"
$sums = Get-ChildItem $Dist -File | Where-Object { $_.Name -like "$Name*" } | Sort-Object Name | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
$sums | Set-Content (Join-Path $Dist "SHA256SUMS.txt") -Encoding ascii
$sums | ForEach-Object { Write-Host "    $_" }
Remove-Item $Stage -Recurse -Force
Step "Done"
