# Signs one file with signtool for the packages (cargo-packager calls it for the executable,
# the installer and its uninstaller). The certificate comes from the environment:
#
#   BOWYER_SIGN_PFX           path to a .pfx file, with BOWYER_SIGN_PFX_PASSWORD
#   BOWYER_SIGN_THUMBPRINT    or the SHA-1 thumbprint of a certificate in the user's store
#   BOWYER_SIGN_TIMESTAMP     RFC 3161 timestamp server (default http://timestamp.digicert.com),
#                             `none` for no timestamp (tests: the signature dies with the certificate)
#
# With neither, nothing is signed and the build goes on.
param([Parameter(Mandatory = $true)][string]$Path)
$ErrorActionPreference = 'Stop'

if (-not $env:BOWYER_SIGN_PFX -and -not $env:BOWYER_SIGN_THUMBPRINT) {
    Write-Host "sign.ps1: no certificate (BOWYER_SIGN_PFX or BOWYER_SIGN_THUMBPRINT), $Path stays unsigned"
    exit 0
}

$signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source
if (-not $signtool) {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $signtool = Get-ChildItem $kits -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.DirectoryName -like '*\x64' } |
        Sort-Object { $_.Directory.Parent.Name } -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $signtool) { throw 'sign.ps1: signtool.exe not found (Windows SDK)' }

$timestamp = if ($env:BOWYER_SIGN_TIMESTAMP) { $env:BOWYER_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }
$arguments = @('sign', '/fd', 'sha256', '/d', 'Bowyer')
if ($timestamp -ne 'none') { $arguments += @('/tr', $timestamp, '/td', 'sha256') }
if ($env:BOWYER_SIGN_PFX) {
    $arguments += @('/f', $env:BOWYER_SIGN_PFX)
    if ($env:BOWYER_SIGN_PFX_PASSWORD) { $arguments += @('/p', $env:BOWYER_SIGN_PFX_PASSWORD) }
} else {
    $arguments += @('/sha1', $env:BOWYER_SIGN_THUMBPRINT)
}
& $signtool @arguments $Path
if ($LASTEXITCODE -ne 0) { throw "sign.ps1: signtool failed on $Path ($LASTEXITCODE)" }
