# finalize.ps1 — 将指定 Bundle 生成 DeepX 自解压安装器
param(
    [ValidateSet("full", "frontend", "backend")]
    [string]$Kind = "full",
    [string]$PayloadDir = "",
    [string]$ExePath = "target/release/DeepXInstaller.exe",
    [string]$OutDir = "packages"
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($PayloadDir)) {
    $pointerPath = "apps/installer/staging/$Kind.latest.json"
    if (Test-Path -LiteralPath $pointerPath -PathType Leaf) {
        $PayloadDir = (Get-Content -LiteralPath $pointerPath -Raw | ConvertFrom-Json).payloadPath
    } else {
        $PayloadDir = "apps/installer/staging/$Kind"
    }
}

$manifestPath = Join-Path $PayloadDir "bundle.json"
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "未找到 Bundle manifest: $manifestPath"
}
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "未找到安装器 EXE: $ExePath"
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.kind -ne $Kind) {
    throw "Bundle 类型不匹配: 期望 $Kind，实际 $($manifest.kind)"
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$safeBuildId = $manifest.buildId -replace '[^A-Za-z0-9._-]', '-'
$displayKind = (Get-Culture).TextInfo.ToTitleCase($Kind)
$outputPath = Join-Path $OutDir "DeepXInstaller-$displayKind-$safeBuildId.exe"
$zipPath = Join-Path $OutDir ".$Kind-$safeBuildId.payload.zip"

Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $outputPath -Force -ErrorAction SilentlyContinue

Write-Host "=== 生成 $Kind 自解压安装器 ==="
Write-Host "  → 压缩 payload ..."
$payloadItems = Get-ChildItem -LiteralPath $PayloadDir -Force | ForEach-Object { $_.FullName }
Compress-Archive -LiteralPath $payloadItems -DestinationPath $zipPath -CompressionLevel Optimal -Force

Write-Host "  → 拼接安装器 ..."
$exeStream = [System.IO.File]::OpenRead((Resolve-Path -LiteralPath $ExePath))
$zipStream = [System.IO.File]::OpenRead((Resolve-Path -LiteralPath $zipPath))
$outStream = [System.IO.File]::Create([System.IO.Path]::GetFullPath($outputPath))
try {
    $exeStream.CopyTo($outStream)
    $zipStream.CopyTo($outStream)
} finally {
    $exeStream.Close()
    $zipStream.Close()
    $outStream.Close()
}

Remove-Item -LiteralPath $zipPath -Force
$sizeMb = [Math]::Round((Get-Item -LiteralPath $outputPath).Length / 1MB, 2)
Write-Host "  ✓ $outputPath ($sizeMb MB)"
