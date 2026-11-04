# Packages the release build (this machine's architecture): a portable .zip and an Inno Setup
# installer in dist\windows.
#   scripts\windows\package.ps1 [-NoBuild]
param(
    [string]$Arch = $(if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }),
    [switch]$NoBuild
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\..\.."
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $NoBuild) {
    cargo build --release -p dataman -p dataman-mcp
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    # The DuckDB engine (scripts/duckdb-engine.sh does the same; `bash` here may be WSL's),
    # unless CI restored it from its cache (DATAMAN_DUCKDB_CACHED=true).
    if (-not ($env:DATAMAN_DUCKDB_CACHED -eq "true" -and (Test-Path target\duckdb\dataman_duckdb.dll))) {
        cargo build --release --locked --manifest-path engines\duckdb\Cargo.toml --target-dir target\duckdb-engine
        if ($LASTEXITCODE -ne 0) { throw "the DuckDB engine did not build" }
        New-Item -ItemType Directory -Force target\duckdb | Out-Null
        Copy-Item target\duckdb-engine\release\dataman_duckdb.dll target\duckdb\
    }
}
$bin = "target\release"

$dist = "dist\windows"
$stage = "$dist\Dataman"
Remove-Item -Recurse -Force $dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
# DuckDB: one engine beside both binaries, loaded when a DuckDB database is opened.
Copy-Item "$bin\dataman.exe", "$bin\dataman-mcp.exe", "target\duckdb\dataman_duckdb.dll" $stage
Copy-Item packaging\windows\README.txt "$stage\README.txt"
Copy-Item LICENSE "$stage\LICENSE.txt"

# The binaries say who they are before anything is packed.
$v = (& "$stage\dataman-mcp.exe" --version) -join ""
if ($v -ne "dataman-mcp $version") { throw "dataman-mcp.exe says '$v', expected $version" }

$zip = "$dist\Dataman-$version-windows-$Arch.zip"
Compress-Archive -Path $stage -DestinationPath $zip
Write-Host "zip: $zip"

$iscc = (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) { $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" }
if (-not (Test-Path $iscc)) { $iscc = "$env:ProgramFiles\Inno Setup 6\ISCC.exe" }
if (-not (Test-Path $iscc)) { $iscc = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" }
if (-not (Test-Path $iscc)) { throw "Inno Setup 6 (ISCC.exe) is not installed" }
& $iscc /Qp "/DVersion=$version" "/DArch=$Arch" "/DSourceDir=$((Resolve-Path $stage).Path)" "/DOutputDir=$((Resolve-Path $dist).Path)" packaging\windows\dataman.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
Get-ChildItem $dist -File | ForEach-Object { Write-Host ("{0}  {1:N1} MB" -f $_.Name, ($_.Length / 1MB)) }
