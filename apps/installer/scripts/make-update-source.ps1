# make-update-source.ps1 — 将已收集的 Bundle 组织成本地 UpdateSource
param(
    [ValidateSet("full", "frontend", "backend")]
    [string[]]$Kinds = @("full", "frontend", "backend"),
    [string]$StagingDir = "apps/installer/staging",
    [string]$OutDir = "packages/update-source",
    [string]$LauncherExe = "target/release/DeepXInstaller.exe"
)

$ErrorActionPreference = "Stop"

$workspaceRoot = (Resolve-Path ".").Path
$packagesRoot = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot "packages"))
$outputRoot = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $OutDir))
$packagesPrefix = $packagesRoot.TrimEnd(
    [System.IO.Path]::DirectorySeparatorChar,
    [System.IO.Path]::AltDirectorySeparatorChar
) + [System.IO.Path]::DirectorySeparatorChar
$pathComparison = if ([System.OperatingSystem]::IsWindows()) {
    [System.StringComparison]::OrdinalIgnoreCase
} else {
    [System.StringComparison]::Ordinal
}
if ([string]::Equals($outputRoot, $packagesRoot, $pathComparison)) {
    throw "OutDir 不能直接指向 packages 根目录"
}
if (-not $outputRoot.StartsWith($packagesPrefix, $pathComparison)) {
    throw "OutDir 必须位于仓库根目录的 packages 内: $outputRoot"
}

$manifests = [ordered]@{}
$bundleRoots = [ordered]@{}
foreach ($kind in $Kinds) {
    $pointerPath = Join-Path $StagingDir "$kind.latest.json"
    if (Test-Path -LiteralPath $pointerPath -PathType Leaf) {
        $bundleRoot = (Get-Content -LiteralPath $pointerPath -Raw | ConvertFrom-Json).payloadPath
    } else {
        $bundleRoot = Join-Path $StagingDir $kind
    }
    $manifestPath = Join-Path $bundleRoot "bundle.json"
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        $recipe = switch ($kind) {
            "frontend" { "package-update-frontend" }
            "backend" { "package-update-backend" }
            default { "package-update" }
        }
        throw "缺少 $kind Bundle，请先运行 just ${recipe}：$manifestPath"
    }
    $manifests[$kind] = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    $bundleRoots[$kind] = $bundleRoot
}

if (Test-Path -LiteralPath $outputRoot) {
    Remove-Item -LiteralPath $outputRoot -Recurse -Force
}
$bundlesRoot = Join-Path $outputRoot "bundles"
New-Item -ItemType Directory -Path $bundlesRoot -Force | Out-Null

$components = [ordered]@{}
$artifacts = [System.Collections.Generic.List[object]]::new()
foreach ($kind in $Kinds) {
    $manifest = $manifests[$kind]
    foreach ($property in $manifest.components.PSObject.Properties) {
        $components[$property.Name] = $property.Value
    }

    $safeBuildId = ([string]$manifest.buildId) -replace '[^A-Za-z0-9._-]', '-'
    $zipName = "$kind-$safeBuildId.zip"
    $zipPath = Join-Path $bundlesRoot $zipName
    $bundleRoot = $bundleRoots[$kind]
    $bundleItems = Get-ChildItem -LiteralPath $bundleRoot -Force | ForEach-Object { $_.FullName }
    Compress-Archive -LiteralPath $bundleItems -DestinationPath $zipPath -CompressionLevel Optimal -Force

    $targets = [ordered]@{}
    foreach ($property in $manifest.components.PSObject.Properties) {
        $targets[$property.Name] = $property.Value.buildId
    }
    $requires = [ordered]@{}
    if ($kind -eq "frontend") {
        if ($components.runtime) {
            $requires.runtime = $components.runtime.buildId
        }
        $requires.controlProtocol = $manifest.components.frontend.controlProtocol
    } elseif ($kind -eq "backend") {
        $requires.controlProtocol = $manifest.components.backend.controlProtocol
    }
    $restartPolicy = switch ($kind) {
        "frontend" { "electron" }
        "backend" { "daemon" }
        default { "full" }
    }
    $zip = Get-Item -LiteralPath $zipPath
    $artifacts.Add([ordered]@{
        id = "$kind-$safeBuildId"
        kind = $kind
        strategy = "component-full"
        targets = $targets
        requires = $requires
        restartPolicy = $restartPolicy
        payload = [ordered]@{
            path = "bundles/$zipName"
            size = $zip.Length
            sha256 = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    })
}

$identity = ($components.GetEnumerator() | ForEach-Object {
    "$($_.Key)=$($_.Value.buildId)"
}) -join ";"
$identityBytes = [System.Text.Encoding]::UTF8.GetBytes($identity)
$identityHash = [System.Security.Cryptography.SHA256]::HashData($identityBytes)
$releaseSuffix = [Convert]::ToHexString($identityHash).ToLowerInvariant().Substring(0, 16)
$channels = @($manifests.Values | ForEach-Object { $_.channel } | Where-Object { $_ })
$channel = if ($channels.Count -gt 0) { $channels[0] } else { "local" }

$catalog = [ordered]@{
    formatVersion = 1
    releaseId = "local-$releaseSuffix"
    channel = $channel
    publishedAt = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    components = $components
    artifacts = $artifacts
}
$catalogPath = Join-Path $outputRoot "catalog.json"
$catalog | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $catalogPath -Encoding utf8
if (-not (Test-Path -LiteralPath $LauncherExe -PathType Leaf)) {
    throw "缺少更新源启动器，请先 build-installer：$LauncherExe"
}
$launcherPath = Join-Path $outputRoot "DeepXUpdate.exe"
Copy-Item -LiteralPath $LauncherExe -Destination $launcherPath -Force

Write-Host "=== 本地更新源已生成 ==="
Write-Host "  ✓ $catalogPath"
Write-Host "  ✓ $launcherPath"
Write-Host "  ✓ $($artifacts.Count) 个更新包：$($Kinds -join ', ')"
