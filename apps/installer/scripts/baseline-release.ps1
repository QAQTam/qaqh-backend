# baseline-release.ps1 — 发布检查点（完整包）并建立新基线
#
# 检查点语义：1.0.0 / 1.1.0 这类版本 = 完整包 + 新基线。发布后：
#   - bundles/ 重建（完整包 artifact，旧增量包清空——新基线的增量基于新检查点重新生成）
#   - baselines/<version>/manifest.json 落盘（后续 just upgrade-package 以此为基线）
#   - catalog.json 重建（components/artifacts 指向新检查点构建）
#
# 用法：
#   just baseline-release version=1.0.0
# 前置：目标构建已 collect-payload（staging/full.latest.json 等）。
param(
    [Parameter(Mandatory)]
    [string]$Version,
    [string]$StagingDir = "apps/installer/staging",
    [string]$UpdateDir = "package/update",
    [string]$LauncherExe = "target/release/DeepXInstaller.exe",
    [string[]]$Kinds = @("full", "runtime", "backend")
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path ".").Path
$stagingRoot = Join-Path $workspaceRoot $StagingDir
$updateRoot = Join-Path $workspaceRoot $UpdateDir
$pack = Join-Path $workspaceRoot "target/release/deepx-pack.exe"

# 1) 备份现有 baselines（make-update-source 会重建 updateRoot）。
$backup = Join-Path $env:TEMP "deepx-baselines-backup-$Version"
if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Recurse -Force }
if (Test-Path -LiteralPath (Join-Path $updateRoot "baselines")) {
    Copy-Item -LiteralPath (Join-Path $updateRoot "baselines") -Destination $backup -Recurse
}

# 2) 完整更新源重建（full + runtime + backend 完整包 + catalog）。
Write-Host "=== make-update-source（完整包重建）==="
& (Join-Path $PSScriptRoot "make-update-source.ps1") `
    -Kinds $Kinds -StagingDir $StagingDir -OutDir $UpdateDir -LauncherExe $LauncherExe
if ($LASTEXITCODE -ne 0) { throw "make-update-source 失败（exit $LASTEXITCODE）" }

# 3) 恢复旧基线目录（保留历史检查点，供回退/对比）。
if (Test-Path -LiteralPath $backup) {
    Copy-Item -LiteralPath $backup -Destination (Join-Path $updateRoot "baselines") -Recurse
    Remove-Item -LiteralPath $backup -Recurse -Force
}

# 4) 发布新基线：full bundle 的 manifest → baselines/<version>/manifest.json。
$pointerPath = Join-Path $stagingRoot "full.latest.json"
if (Test-Path -LiteralPath $pointerPath -PathType Leaf) {
    $bundleRoot = (Get-Content -LiteralPath $pointerPath -Raw | ConvertFrom-Json).payloadPath
} else {
    $bundleRoot = Join-Path $stagingRoot "full"
}
if (-not (Test-Path -LiteralPath (Join-Path $bundleRoot "bundle.json") -PathType Leaf)) {
    throw "缺少 full Bundle，请先 collect-payload -Kind full：$bundleRoot"
}
Write-Host "=== baseline-publish: $Version ==="
& $pack baseline-publish --bundle $bundleRoot --version $Version --out $updateRoot
if ($LASTEXITCODE -ne 0) { throw "baseline-publish 失败（exit $LASTEXITCODE）" }

Write-Host "=== 检查点 $Version 已发布（完整包 + 新基线）==="
Write-Host "  ✓ $updateRoot/catalog.json"
Write-Host "  ✓ $updateRoot/baselines/$Version/manifest.json"
Write-Host "下一步：改代码后 just upgrade-package kind=runtime（或 backend）生成增量包"
