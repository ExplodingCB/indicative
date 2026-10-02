# Builds indicative.exe and the installer into dist\ (same steps as CI).
#   .\scripts\package.ps1            # version from Cargo.toml
#   .\scripts\package.ps1 -Version 0.2.0
#
# Needs MinGW-w64 GCC (winget install BrechtSanders.WinLibs.POSIX.MSVCRT)
# and Inno Setup 6 (winget install JRSoftware.InnoSetup).
param([string]$Version)
# Native tools write progress to stderr; rely on exit codes, not error records
# (Windows PowerShell 5.1 turns redirected stderr into errors).
$ErrorActionPreference = "Continue"
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
}

# gcc + dlltool must be on PATH for the GNU toolchain; use winget's WinLibs if needed.
if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
    $bin = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs*\mingw64\bin" -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $bin) { throw "MinGW-w64 not found. Install: winget install BrechtSanders.WinLibs.POSIX.MSVCRT" }
    $env:PATH = "$($bin.FullName);$env:PATH"
}

cargo build --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found. Install: winget install JRSoftware.InnoSetup" }

& $iscc /Q "/DAppVersion=$Version" installer\indicative.iss
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Copy-Item target\x86_64-pc-windows-gnu\release\indicative.exe dist\indicative.exe -Force
Get-ChildItem dist | Format-Table Name, Length
