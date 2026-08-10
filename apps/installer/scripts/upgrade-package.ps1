# upgrade-package.ps1 — 发布增量更新包（基于最近检查点基线）
#
# 基线累积模型（docs/winui-update-design.md）：
#   - 基线 = 检查点完整包 manifest（package/update/baselines/<version>/manifest.json）
#   - 增量 = 相对基线变化文件的 bundle（strategy=file-level-delta）
#   - 客户端按 sha256 自适应：未变化本地复用，变化才下载
#
# 用法：
#   just upgrade-package kind=runtime    # 或 backend
#
# 前置：目标构建已 collect-payload（staging/<kind>.latest.json → bundle.json + files/）
# 产物：package/update/bundles/<kind>-<buildId>.zip + catalog.json 更新
param(
    [ValidateSet("runtime", "backend")]
    [string]$Kind = "runtime",
    [string]$StagingDir = "apps/installer/staging",
    [string]$UpdateDir = "package/update",
    # 可选：手动指定基线版本；默认取 baselines/ 下语义最高版本。
    [string]$BaselineVersion = ""
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path ".").Path
$stagingRoot = Join-Path $workspaceRoot $StagingDir
$updateRoot = Join-Path $workspaceRoot $UpdateDir
$pack = Join-Path $workspaceRoot "target/release/deepx-pack.exe"

if (-not (Test-Path -LiteralPath $pack -PathType Leaf)) {
    throw "缺少 deepx-pack，请先运行 just build-pack：$pack"
}

# 1) 定位目标 bundle（collect-payload 产物，经 <kind>.latest.json 指针）。
$pointerPath = Join-Path $stagingRoot "$Kind.latest.json"
if (Test-Path -LiteralPath $pointerPath -PathType Leaf) {
    $bundleRoot = (Get-Content -LiteralPath $pointerPath -Raw | ConvertFrom-Json).payloadPath
} else {
    $bundleRoot = Join-Path $stagingRoot $Kind
}
$manifestPath = Join-Path $bundleRoot "bundle.json"
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "缺少目标 Bundle，请先运行 collect-payload：$manifestPath"
}
$targetManifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$buildId = [string]$targetManifest.buildId

# 2) 定位基线（最近检查点）。
$baselinesRoot = Join-Path $updateRoot "baselines"
if (-not (Test-Path -LiteralPath $baselinesRoot -PathType Container)) {
    throw "缺少基线目录（检查点完整包 manifest）：$baselinesRoot"
}
$baselineDirs = Get-ChildItem -LiteralPath $baselinesRoot -Directory |
    Where-Object { [version]::TryParse($_.Name, [ref]([version]::new(0, 0))) } |
    Sort-Object { [version]$_.Name } -Descending
if ($baselineDirs.Count -eq 0) {
    throw "baselines/ 下没有检查点版本，请先发布完整包（just baseline-release version=<v>）"
}
if ($BaselineVersion) {
    $baselineDir = Join-Path $baselinesRoot $BaselineVersion
    if (-not (Test-Path -LiteralPath $baselineDir -PathType Container)) {
        throw "指定的基线版本不存在：$BaselineVersion"
    }
} else {
    $baselineDir = $baselineDirs[0].FullName
    $BaselineVersion = $baselineDirs[0].Name
}
$baselineManifest = Join-Path $baselineDir "manifest.json"
if (-not (Test-Path -LiteralPath $baselineManifest -PathType Leaf)) {
    throw "基线清单缺失：$baselineManifest"
}

# 3) 增量打包（deepx-pack delta → 增量 bundle 根 + zip + catalog 更新）。
$safeBuildId = $buildId -replace '[^A-Za-z0-9._-]', '-'
$outRoot = Join-Path $updateRoot "staging/$Kind-$safeBuildId"
$zipPath = Join-Path $updateRoot "bundles/$Kind-$safeBuildId.zip"
$catalogPath = Join-Path $updateRoot "catalog.json"
if (Test-Path -LiteralPath $outRoot) { Remove-Item -LiteralPath $outRoot -Recurse -Force }
New-Item -ItemType Directory -Path (Split-Path $zipPath) -Force | Out-Null

Write-Host "=== upgrade-package: $Kind $buildId (基线 $BaselineVersion) ==="
& $pack delta `
    --baseline $baselineManifest `
    --target $bundleRoot `
    --baseline-version $BaselineVersion `
    --out $outRoot `
    --zip $zipPath `
    --catalog $catalogPath `
    --restart-policy $(if ($Kind -eq "backend") { "daemon" } else { "electron" })
if ($LASTEXITCODE -ne 0) { throw "deepx-pack delta 失败（exit $LASTEXITCODE）" }

$delta = Get-Content -LiteralPath (Join-Path $outRoot "bundle.json") -Raw | ConvertFrom-Json
Write-Host "=== 完成：$($delta.files.Count) 个变化文件 → $zipPath ==="
