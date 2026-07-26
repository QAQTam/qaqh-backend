# sync-version.ps1 — 从 version.txt 同步版本号到所有配置文件
param(
    [string]$VersionFile = "version.txt",
    [string]$CargoToml   = "Cargo.toml",
    [string]$PkgJson     = "apps/desktop/package.json",
    [string]$LockJson    = "apps/desktop/deepx-backend.lock.json",
    [string]$RootPkgJson = "package.json"
)

$v = (Get-Content $VersionFile).Trim()
Write-Host "Syncing version: $v"

# Cargo.toml: 替换 [workspace.package] 下的 version
$cargo = Get-Content $CargoToml -Raw
$cargo = $cargo -replace '(?<=\[workspace\.package\][\s\S]*?version\s*=\s*)".*?"', "`"$v`""
Set-Content $CargoToml -Value $cargo -NoNewline

# package.json
$pkg = Get-Content $PkgJson -Raw | ConvertFrom-Json
$pkg.version = $v
$pkg | ConvertTo-Json -Depth 16 | Set-Content $PkgJson -NoNewline

# deepx-backend.lock.json
$lock = Get-Content $LockJson -Raw | ConvertFrom-Json
$lock.version = $v
$lock | ConvertTo-Json -Depth 4 | Set-Content $LockJson -NoNewline

# root package.json
$rp = Get-Content $RootPkgJson -Raw | ConvertFrom-Json
$rp.version = $v
$rp | ConvertTo-Json -Depth 4 | Set-Content $RootPkgJson -NoNewline

Write-Host "Done — $v synced to Cargo.toml, desktop/package.json, deepx-backend.lock.json, root package.json"
