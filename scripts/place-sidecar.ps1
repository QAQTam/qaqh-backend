# 把 daemon 构建产物放置为 Tauri sidecar(目标三元组命名,带存在性断言)。
# 用法: pwsh -File scripts/place-sidecar.ps1 [debug|release]
param(
    [Parameter(Mandatory = $false)][ValidateSet('debug', 'release')][string]$Mode = 'debug'
)

$ErrorActionPreference = 'Stop'

$triple = (rustc --print host-tuple).Trim()
$daemonName = if ($IsWindows) { 'qaqh-daemon.exe' } else { 'qaqh-daemon' }
$sidecarName = if ($IsWindows) { "qaqh-daemon-$triple.exe" } else { "qaqh-daemon-$triple" }
$src = Join-Path "target/$Mode" $daemonName

if (-not (Test-Path $src)) {
    throw "daemon binary not found: $src (先运行 cargo build -p qaqh-daemon)"
}

New-Item -ItemType Directory -Force -Path 'webui/src-tauri/binaries' | Out-Null
Copy-Item $src (Join-Path 'webui/src-tauri/binaries' $sidecarName) -Force
Write-Output "sidecar: $src -> webui/src-tauri/binaries/$sidecarName"
