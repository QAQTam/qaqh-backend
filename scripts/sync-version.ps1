# sync-version.ps1 — qaqh-winui-app：从 version.txt 同步版本号
# 仅覆盖本仓库配置：Cargo.toml（[workspace.package]）与 package.json。
# 后端版本锁（qaqh-backend.lock.json）由 QAQ-Harness 后端仓库维护，不在此同步。

param(
    [string]$VersionFile = "version.txt",
    [string]$CargoToml   = "Cargo.toml",
    [string]$RootPkgJson = "package.json"
)

$ErrorActionPreference = "Stop"

$v = (Get-Content $VersionFile -Raw).Trim()
if (-not $v) { throw "version.txt is empty" }

# Cargo.toml: 替换 [workspace.package] 下的 version
$cargo = Get-Content $CargoToml -Raw
$cargo = [regex]::Replace(
    $cargo,
    '(?ms)(\[workspace\.package\][^\[]*?version\s*=\s*")[^"]*(")',
    ('$1' + $v + '$2')
)
Set-Content $CargoToml -Value $cargo -NoNewline

# 根 package.json
$pkg = Get-Content $RootPkgJson -Raw | ConvertFrom-Json
$pkg.version = $v
($pkg | ConvertTo-Json -Depth 8) + [Environment]::NewLine |
    Set-Content $RootPkgJson -Encoding UTF8

Write-Host "Done — $v synced to Cargo.toml and package.json (backend lock lives in ../QAQ-Harness)"
