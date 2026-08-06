# collect-payload-winui.ps1 — 按组件收集 winui 壳安装文件并生成 bundle.json
#
# 与 collect-payload.ps1 输出同构的 bundle（installer/updater 消费无感知），
# 但 full 包的 desktop 组件来自 winui 运行目录而非 Electron win-unpacked。
# 目前仅支持 -Kind full（frontend/backend 更新源后续接入）。

param(
    [ValidateSet("full")]
    [string]$Kind = "full",
    [string]$PayloadDir = "",
    [string]$BuildId = "",
    [string]$WinuiRoot = "apps/winui/release/winui-app"
)

$ErrorActionPreference = "Stop"

$workspaceRoot = (Resolve-Path ".").Path
$stagingRoot = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot "apps/installer/staging"))
$package = Get-Content "apps/desktop/package.json" -Raw | ConvertFrom-Json
$backendLock = Get-Content "apps/desktop/deepx-backend.lock.json" -Raw | ConvertFrom-Json
$daemonManifestPath = Join-Path $workspaceRoot "apps/desktop/build/sidecar/daemon-manifest.json"
$daemonManifest = if (Test-Path -LiteralPath $daemonManifestPath -PathType Leaf) {
    Get-Content $daemonManifestPath -Raw | ConvertFrom-Json
} else {
    $null
}
if ([string]::IsNullOrWhiteSpace($BuildId)) {
    $gitCommit = (git rev-parse --short=12 HEAD).Trim()
    $timestamp = (Get-Date).ToUniversalTime().ToString("yyyyMMddHHmmss")
    $BuildId = "$($package.version)-$gitCommit-$timestamp"
}

$usesLatestPointer = [string]::IsNullOrWhiteSpace($PayloadDir)
if ($usesLatestPointer) {
    $safePayloadBuildId = $BuildId -replace '[^A-Za-z0-9._-]', '-'
    $PayloadDir = "apps/installer/staging/builds/$Kind/$safePayloadBuildId"
}
$payloadFullPath = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $PayloadDir))
if (-not $payloadFullPath.StartsWith($stagingRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "PayloadDir 必须位于 apps/installer/staging 内: $payloadFullPath"
}

if (Test-Path -LiteralPath $payloadFullPath) {
    Remove-Item -LiteralPath $payloadFullPath -Recurse -Force
}
$filesRoot = Join-Path $payloadFullPath "files"
New-Item -ItemType Directory -Path $filesRoot -Force | Out-Null

$manifestFiles = [System.Collections.Generic.List[object]]::new()

function Add-BundleFile {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Target
    )
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "缺少打包文件: $Source"
    }
    $normalizedTarget = $Target.Replace("\", "/").TrimStart("/")
    if ($normalizedTarget -match '(^|/)\.\.(/|$)') {
        throw "非法目标路径: $Target"
    }
    $payloadRelative = "files/$normalizedTarget"
    $destination = Join-Path $payloadFullPath $payloadRelative.Replace("/", "\")
    $parent = Split-Path -Parent $destination
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    Copy-Item -LiteralPath $Source -Destination $destination -Force
    $copied = Get-Item -LiteralPath $destination
    $manifestFiles.Add([ordered]@{
        source = $payloadRelative
        target = $normalizedTarget
        size = $copied.Length
        sha256 = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    })
}

function Add-BundleTree {
    param(
        [Parameter(Mandatory = $true)][string]$SourceRoot,
        [string]$TargetRoot = ""
    )
    if (-not (Test-Path -LiteralPath $SourceRoot -PathType Container)) {
        throw "缺少打包目录: $SourceRoot"
    }
    $resolvedSource = (Resolve-Path -LiteralPath $SourceRoot).Path
    Get-ChildItem -LiteralPath $resolvedSource -File -Recurse | ForEach-Object {
        # 排除 WebView2 运行时数据目录（安装包内不需要，且可能被占用）
        if ($_.FullName -match '\\DeepX\.exe\.WebView2\\') {
            return
        }
        $relative = [System.IO.Path]::GetRelativePath($resolvedSource, $_.FullName)
        $target = if ($TargetRoot) { Join-Path $TargetRoot $relative } else { $relative }
        Add-BundleFile -Source $_.FullName -Target $target
    }
}

$components = [ordered]@{}
$backendComponent = [ordered]@{
    buildId = if ($daemonManifest) { "backend-$($daemonManifest.build_id)" } else { "backend-$BuildId" }
    version = if ($daemonManifest) { $daemonManifest.version } else { $package.version }
    controlProtocol = if ($daemonManifest) { [int]$daemonManifest.protocol_version } else { [int]$backendLock.protocol_version }
}

Write-Host "=== 收集 $Kind 安装包（winui 壳）==="

switch ($Kind) {
    "full" {
        $components.runtime = [ordered]@{
            buildId = "winui-shell-$BuildId"
            version = $package.version
        }
        $components.frontend = [ordered]@{
            buildId = "frontend-$BuildId"
            version = $package.version
            controlProtocol = [int]$backendLock.protocol_version
        }
        $components.backend = $backendComponent
        $components.updater = [ordered]@{
            buildId = "updater-$((Get-FileHash -LiteralPath 'target/release/deepx-updater.exe' -Algorithm SHA256).Hash.ToLowerInvariant().Substring(0, 32))"
            version = $package.version
        }

        $winuiFull = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $WinuiRoot))
        if (-not (Test-Path -LiteralPath $winuiFull -PathType Container)) {
            throw "缺少 winui 运行目录: $winuiFull（先跑 just package-winui-desktop）"
        }
        Add-BundleTree -SourceRoot $winuiFull
        Add-BundleFile -Source "target/release/deepx-updater.exe" -Target "deepx-updater.exe"
        if (Test-Path -LiteralPath "apps/installer/payload/config/default.toml" -PathType Leaf) {
            Add-BundleFile -Source "apps/installer/payload/config/default.toml" -Target "config/config.toml"
        }
    }
}

$manifest = [ordered]@{
    formatVersion = 1
    kind = $Kind
    buildId = $BuildId
    appVersion = $package.version
    releaseId = $BuildId
    channel = if ($daemonManifest) { $daemonManifest.channel } else { "local" }
    components = $components
    requiresFullInstall = $Kind -ne "full"
    files = $manifestFiles
}

$manifestPath = Join-Path $payloadFullPath "bundle.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8
if ($usesLatestPointer) {
    New-Item -ItemType Directory -Path $stagingRoot -Force | Out-Null
    $pointerPath = Join-Path $stagingRoot "$Kind.latest.json"
    [ordered]@{
        formatVersion = 1
        kind = $Kind
        buildId = $BuildId
        payloadPath = $payloadFullPath
    } | ConvertTo-Json | Set-Content -LiteralPath $pointerPath -Encoding utf8
}

Write-Host "  ✓ $($manifestFiles.Count) 个文件"
Write-Host "  ✓ $manifestPath"
