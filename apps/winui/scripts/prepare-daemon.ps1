# prepare-daemon.ps1 — 预置 daemon/workspace sidecar（原生打包，替代旧 node 脚本）
#
# 职责（对齐原 apps/winui/renderer/scripts/prepare-daemon.mjs）：
#   1. 读根目录 deepx-backend.lock.json（版本锁：version/protocol_version/git_commit）
#   2. 本地后端（默认）：校验 Cargo.toml version 与 deepx-proto 协议版本与锁一致，
#      复制 target/release/{deepx-daemon,deepx-workspace}.exe 到 sidecar 目录
#   3. 发布后端（-ReleaseArtifact）：按锁下载 release manifest 对应 artifact
#   4. 校验 daemon 二进制实际嵌入 build_id 与清单一致（防 git 缺失回退版本号）
#   5. 写 daemon-manifest.json（installer/updater 消费）
#
# 用法: pwsh -File apps/winui/scripts/prepare-daemon.ps1 [-BackendRoot <repo>]
#                [-ReleaseArtifact] [-SidecarDir apps/winui/out/sidecar]
# 注意：Windows-only（DeepX 桌面端定位），与 justfile [windows] 约束一致。

param(
    [string]$BackendRoot = "",
    [switch]$ReleaseArtifact,
    [string]$SidecarDir = "apps/winui/out/sidecar"
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path ".").Path

# ── 版本锁 ────────────────────────────────────────────────
$lockPath = Join-Path $workspaceRoot "deepx-backend.lock.json"
if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
    throw "缺少版本锁: $lockPath"
}
$lock = Get-Content $lockPath -Raw | ConvertFrom-Json
$versionFile = Join-Path $workspaceRoot "version.txt"
if (Test-Path -LiteralPath $versionFile -PathType Leaf) {
    $desktopVersion = (Get-Content $versionFile).Trim()
    if ($desktopVersion -ne $lock.version) {
        throw "version.txt ($desktopVersion) 与 deepx-backend.lock.json ($($lock.version)) 不一致"
    }
}
if ($env:GITHUB_REF_NAME -and $env:GITHUB_REF_NAME.StartsWith("v") -and $env:GITHUB_REF_NAME -ne "v$($lock.version)") {
    throw "Release tag $($env:GITHUB_REF_NAME) does not match version v$($lock.version)"
}

$sidecarFull = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $SidecarDir))
New-Item -ItemType Directory -Path $sidecarFull -Force | Out-Null
$daemonDest = Join-Path $sidecarFull "deepx-daemon.exe"
$workspaceDest = Join-Path $sidecarFull "deepx-workspace.exe"

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-GitCommit([string]$RepoRoot) {
    $out = & git -C $RepoRoot rev-parse HEAD 2>$null
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($out)) {
        throw "Unable to resolve git commit at $RepoRoot"
    }
    $out.Trim()
}

function Get-ProtocolVersion([string]$RepoRoot) {
    $control = Get-Content (Join-Path $RepoRoot "crates/deepx-proto/src/control.rs") -Raw
    if ($control -notmatch 'CONTROL_PROTOCOL_VERSION\s*:\s*u16\s*=\s*(\d+)') {
        throw "Unable to read CONTROL_PROTOCOL_VERSION from deepx-proto"
    }
    [int]$Matches[1]
}

function Get-WorkspaceVersion([string]$RepoRoot) {
    $cargo = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
    if ($cargo -notmatch '(?s)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        throw "Unable to read workspace version from Cargo.toml"
    }
    $Matches[1]
}

# ── 校验 daemon 二进制实际嵌入 build_id（防陈旧产物/无 git 回退）─────────
function Test-DaemonBuildId([string]$ExePath, [string]$Expected) {
    if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
        throw "缺少 daemon 二进制: $ExePath（先跑 just build-daemon）"
    }
    # 字节级扫描（Latin1 映射保留原字节序列，无需解码 UTF-8）
    $bytes = [System.IO.File]::ReadAllBytes($ExePath)
    $latin1 = [System.Text.Encoding]::GetEncoding("ISO-8859-1")
    $content = $latin1.GetString($bytes)
    if (-not $content.Contains($Expected)) {
        throw "staged daemon does not embed build $Expected; " +
            "daemon 二进制陈旧或构建时无 git（build.rs 回退 CARGO_PKG_VERSION）。" +
            "请重跑 just build-daemon 后重试。"
    }
}

$stagedBuildId = ""
if ($ReleaseArtifact) {
    # ── 发布后端：按锁下载 release manifest 对应 artifact ──────────────
    $target = "windows-x86_64"
    $manifestUrl = $lock.release_manifest_url
    if ([string]::IsNullOrWhiteSpace($manifestUrl)) {
        throw "release_manifest_url 缺失（本地打包请用 -BackendRoot 或默认路径）"
    }
    $manifest = Invoke-RestMethod -Uri $manifestUrl -Method Get
    foreach ($field in @("version", "protocol_version", "git_commit")) {
        if ($manifest.$field -ne $lock.$field) {
            throw "Backend manifest $field does not match deepx-backend.lock.json"
        }
    }
    foreach ($pair in @(
            @{ Artifact = $manifest.artifacts.$target; Dest = $daemonDest },
            @{ Artifact = $manifest.artifacts."$target-workspace"; Dest = $workspaceDest }
        )) {
        $art = $pair.Artifact
        if (-not $art -or [string]::IsNullOrWhiteSpace($art.url) -or
            [string]::IsNullOrWhiteSpace($art.sha256) -or [string]::IsNullOrWhiteSpace($art.name)) {
            throw "Backend release has no $target artifact (need daemon + workspace)"
        }
        $cacheDir = Join-Path $workspaceRoot ".cache/deepx/$($art.sha256)"
        New-Item -ItemType Directory -Path $cacheDir -Force | Out-Null
        $cached = Join-Path $cacheDir $art.name
        if (-not (Test-Path -LiteralPath $cached -PathType Leaf) -or
            (Get-Sha256 $cached) -ne $art.sha256) {
            Invoke-WebRequest -Uri $art.url -OutFile $cached -UseBasicParsing
            if ((Get-Sha256 $cached) -ne $art.sha256) {
                throw "Checksum mismatch for $($art.name)"
            }
        }
        Copy-Item -LiteralPath $cached -Destination $pair.Dest -Force
    }
    $stagedBuildId = $lock.git_commit
    Write-Host "Staged locked backend $($lock.version) ($($lock.git_commit.Substring(0, 12))) for $target"
} else {
    # ── 本地后端（默认）：从 target/release 预置 ───────────────────────
    if ([string]::IsNullOrWhiteSpace($BackendRoot)) {
        $BackendRoot = $workspaceRoot
    }
    $BackendRoot = (Resolve-Path $BackendRoot).Path
    $backendVersion = Get-WorkspaceVersion $BackendRoot
    $backendProtocol = Get-ProtocolVersion $BackendRoot
    if ($backendVersion -ne $lock.version -or $backendProtocol -ne $lock.protocol_version) {
        throw "本地后端 $backendVersion/protocol $backendProtocol 与锁 $($lock.version)/$($lock.protocol_version) 不一致"
    }
    $releaseDir = Join-Path $BackendRoot "target/release"
    $daemonSrc = Join-Path $releaseDir "deepx-daemon.exe"
    $workspaceSrc = Join-Path $releaseDir "deepx-workspace.exe"
    if (-not (Test-Path -LiteralPath $daemonSrc -PathType Leaf)) {
        throw "缺少预构建 daemon: $daemonSrc（先跑 just build-daemon）"
    }
    if (-not (Test-Path -LiteralPath $workspaceSrc -PathType Leaf)) {
        throw "缺少预构建 workspace: $workspaceSrc（just build-daemon 同时产出两者）"
    }
    Copy-Item -LiteralPath $daemonSrc -Destination $daemonDest -Force
    Copy-Item -LiteralPath $workspaceSrc -Destination $workspaceDest -Force
    $stagedBuildId = Get-GitCommit $BackendRoot
    Write-Host "Staged local backend $daemonSrc -> $daemonDest"
}

Test-DaemonBuildId $daemonDest $stagedBuildId

# ── daemon-manifest.json ─────────────────────────────────
$workspaceSha256 = Get-Sha256 $workspaceDest
$manifest = [ordered]@{
    version            = $lock.version
    protocol_version   = $lock.protocol_version
    build_id           = $stagedBuildId
    channel            = "stable"
    workspace          = if ($ReleaseArtifact) { "release" } else { "bundled" }
    workspace_sha256   = $workspaceSha256
}
$manifestPath = Join-Path $sidecarFull "daemon-manifest.json"
$manifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $manifestPath -Encoding utf8
Write-Host "  ✓ daemon-manifest.json -> $manifestPath"
