<#
.SYNOPSIS
  Authenticode-sign one file with signtool, configured by environment variables.

.DESCRIPTION
  Used by package_windows.ps1 for the exe, and by Inno Setup (SignTool=) for
  setup.exe and the uninstaller it embeds. Certificates and passwords come from
  the environment only, so they never appear on a command line ISCC logs.

    RCP_SIGN_PFX + RCP_SIGN_PFX_PASSWORD   .pfx file
    RCP_SIGN_CERT_THUMBPRINT               certificate already in the user store
    RCP_TRUSTED_SIGNING_DLIB + RCP_TRUSTED_SIGNING_METADATA
                                           Azure Trusted Signing
    RCP_SIGN_TIMESTAMP_URL                 RFC 3161 timestamp server override

  With none of them set it prints "skipping" and exits 0.
  Exit code 0 = signed or skipped, 1 = failed.
#>
param([Parameter(Mandatory = $true)][string]$File)

$ErrorActionPreference = "Stop"

function Find-SignTool {
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $kits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    return Get-ChildItem $kits -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^10\.' } |
        Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { Join-Path $_.FullName "x64\signtool.exe" } | Where-Object { Test-Path $_ } | Select-Object -First 1
}

$ts = if ($env:RCP_SIGN_TIMESTAMP_URL) { $env:RCP_SIGN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
$mode = $null
$signArgs = @("sign", "/fd", "SHA256", "/td", "SHA256")
if ($env:RCP_TRUSTED_SIGNING_DLIB -and $env:RCP_TRUSTED_SIGNING_METADATA) {
    $mode = "Azure Trusted Signing"
    if (-not $env:RCP_SIGN_TIMESTAMP_URL) { $ts = "http://timestamp.acs.microsoft.com" }
    $signArgs += @("/dlib", $env:RCP_TRUSTED_SIGNING_DLIB, "/dmdf", $env:RCP_TRUSTED_SIGNING_METADATA)
} elseif ($env:RCP_SIGN_PFX) {
    $mode = "PFX certificate"
    $signArgs += @("/f", $env:RCP_SIGN_PFX)
    if ($env:RCP_SIGN_PFX_PASSWORD) { $signArgs += @("/p", $env:RCP_SIGN_PFX_PASSWORD) }
} elseif ($env:RCP_SIGN_CERT_THUMBPRINT) {
    $mode = "certificate store"
    $signArgs += @("/sha1", $env:RCP_SIGN_CERT_THUMBPRINT)
}
$leaf = Split-Path -Leaf $File
if (-not $mode) {
    Write-Host "    Skipping code signing for $leaf (no certificate configured)"
    exit 0
}
if (-not (Test-Path $File)) {
    Write-Host "    sign_file: $File does not exist"
    exit 1
}
$signtool = Find-SignTool
if (-not $signtool) {
    Write-Host "    code signing requested ($mode) but signtool.exe was not found (install the Windows SDK)"
    exit 1
}
$signArgs += @("/tr", $ts, $File)
Write-Host "    Signing $leaf with $mode"
# timestamp servers fail now and then: retry before failing the build
for ($try = 1; $try -le 3; $try++) {
    & $signtool @signArgs | Out-Host
    if ($LASTEXITCODE -eq 0) { exit 0 }
    if ($try -lt 3) { Start-Sleep -Seconds (5 * $try) }
}
Write-Host "    signtool failed for $leaf"
exit 1
