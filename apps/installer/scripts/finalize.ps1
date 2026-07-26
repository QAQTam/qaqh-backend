# finalize.ps1 — 生成 SFX 单文件成品
# 所有路径相对于 workspace 根 (D:\DeepX)
param(
    [string]$PayloadDir = "apps\installer\payload",
    [string]$ExePath    = "target\release\DeepXInstaller.exe",
    [string]$OutDir     = "apps\installer\dist"
)

Write-Host "=== 生成成品 (SFX 单文件模式) ==="

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -Force -ItemType Directory -Path $OutDir | Out-Null

# 1) 构建 payload.zip
if (-not (Test-Path $PayloadDir)) {
    Write-Warning "payload/ 不存在，跳过压缩"
} else {
    Write-Host "  → 压缩 payload.zip ..."
    Compress-Archive -Path "$PayloadDir/*" -DestinationPath "$OutDir/payload.zip" -Force
}

# 2) 追加 ZIP 到 EXE 尾部 → 单文件 SFX
#    用 PowerShell 流式拼接，避免 cmd copy /b 把 "/" 路径当开关
if (Test-Path $ExePath) {
    Write-Host "  → 生成单文件安装器 (流式拼接) ..."
    $sfx = "$OutDir/DeepXInstaller-Setup.exe"
    $exeStream = [System.IO.File]::OpenRead($ExePath)
    $zipStream = [System.IO.File]::OpenRead("$OutDir/payload.zip")
    $outStream = [System.IO.File]::Create($sfx)
    try {
        $exeStream.CopyTo($outStream)
        $zipStream.CopyTo($outStream)
    } finally {
        $exeStream.Close()
        $zipStream.Close()
        $outStream.Close()
    }
    Remove-Item -Force "$OutDir/payload.zip" -ErrorAction SilentlyContinue
} else {
    Write-Warning "未找到安装器 EXE: $ExePath"
}

# 3) 保留 payload 目录版本（调试用）
if (Test-Path $PayloadDir) {
    Copy-Item -Recurse -Force $PayloadDir "$OutDir/payload/" -ErrorAction SilentlyContinue
    Write-Host "  → dist/payload/ (调试用)"
}

Write-Host "  → dist/DeepXInstaller-Setup.exe  (单文件，可直接分发)"
Write-Host "  ✓ 成品生成完毕"
