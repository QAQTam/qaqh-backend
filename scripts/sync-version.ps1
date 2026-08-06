# sync-version.ps1 — 从 version.txt 同步版本号到所有配置文件
param(
    [string]$VersionFile = "version.txt",
    [string]$CargoToml   = "Cargo.toml",
    [string]$PkgJson     = "apps/winui/renderer/package.json",
    [string]$LockJson    = "apps/winui/renderer/deepx-backend.lock.json",
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
$lock.release_manifest_url = $lock.release_manifest_url -replace '/download/v[^/]+/', "/download/v$v/"
# 锁定当前 HEAD：发布新版本时后端 release 应从该 commit 构建，
# prepare-daemon.mjs 会校验 release manifest 的 git_commit 与 lock 一致。
$gitCommit = & git rev-parse HEAD 2>$null
if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($gitCommit)) {
    $lock.git_commit = $gitCommit.Trim()
    Write-Host "  git_commit -> $($lock.git_commit)"
} else {
    Write-Host "  WARN: git rev-parse failed; keeping existing git_commit $($lock.git_commit)"
}
$lock | ConvertTo-Json -Depth 4 | Set-Content $LockJson -NoNewline

# root package.json
$rp = Get-Content $RootPkgJson -Raw | ConvertFrom-Json
$rp.version = $v
$rp | ConvertTo-Json -Depth 4 | Set-Content $RootPkgJson -NoNewline

Write-Host "Done — $v synced to Cargo.toml, winui/renderer/package.json, deepx-backend.lock.json (including release URL), and root package.json"
