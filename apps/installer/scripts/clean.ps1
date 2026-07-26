# clean.ps1 — 清理构建与 payload 产物

$targets = @("payload/desktop")
foreach ($d in $targets) {
    if (Test-Path $d) { Remove-Item -Recurse -Force $d }
}

if (Test-Path "dist") { Remove-Item -Recurse -Force "dist" }
Write-Host "已清理 payload 子目录 和 dist/"
