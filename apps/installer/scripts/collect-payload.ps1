# collect-payload.ps1 — 收集前后端产物到 payload/
# 所有路径相对于 workspace 根 (D:\DeepX)
param(
    [string]$FrontendRoot = "apps\desktop",
    [string]$PayloadDir   = "apps\installer\payload"
)

Write-Host "=== 收集安装文件到 $PayloadDir/ ==="

# 只清理会被重建的子目录，保护已归档的模板文件 (config/)
$rebuilds = @("$PayloadDir/desktop")
foreach ($d in $rebuilds) {
    if (Test-Path $d) { Remove-Item -Recurse -Force $d }
    New-Item -Force -ItemType Directory -Path $d | Out-Null
}

# 桌面应用 (Electron)
$src = "$FrontendRoot/release/win-unpacked"
if (Test-Path $src) {
    Write-Host "  → 复制 Electron 应用"
    Copy-Item -Recurse -Force "$src/*" "$PayloadDir/desktop/"
} else {
    Write-Warning "未找到 Electron 构建产物: $src"
}

# 卸载器二进制
$uninstaller = "target/release/deepx-uninstaller.exe"
if (Test-Path $uninstaller) {
    Write-Host "  → 复制卸载器 deepx-uninstaller.exe"
    Copy-Item -Force $uninstaller "$PayloadDir/deepx-uninstaller.exe"
} else {
    Write-Warning "未找到卸载器: $uninstaller"
}

Write-Host "  ✓ payload 收集完成"
