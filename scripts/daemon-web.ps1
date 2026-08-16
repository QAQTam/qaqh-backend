# daemon-web.ps1 — 启动 QAQ-Harness 局域网 Web 服务（Windows）
# 用法: pwsh -NoProfile -File scripts/daemon-web.ps1 [-Port 64413]
param(
    [int]$Port = 64413
)
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dist = Join-Path $root 'apps/web/dist'
$debugDaemon = Join-Path $root 'target/debug/qaqh-daemon.exe'
$releaseDaemon = Join-Path $root 'target/release/qaqh-daemon.exe'
$daemon = if (Test-Path $debugDaemon) { $debugDaemon } else { $releaseDaemon }

# ── 前置检查 ──────────────────────────────────────────────────────────────
if (-not (Test-Path (Join-Path $dist 'index.html'))) {
    Write-Error "web 产物缺失: $dist`n请先构建: cd apps/web && npm install && npm run build"
    exit 1
}
if (-not (Test-Path $daemon)) {
    Write-Error "daemon 未构建: $daemon`n请先构建: cargo build -p qaqh-daemon"
    exit 1
}

# 单实例检查（WinUI 应用会占用 daemon 单实例锁）
$lock = Join-Path $env:USERPROFILE '.deepx/daemon.lock'
if (Test-Path $lock) {
    $lockPid = [int]((Get-Content $lock -Raw).Trim())
    if (Get-Process -Id $lockPid -ErrorAction SilentlyContinue) {
        Write-Host "错误: 已有 daemon 在运行 (pid $lockPid)。" -ForegroundColor Red
        Write-Host "如果是 QAQ-Harness 应用占用，请先退出应用再重试。" -ForegroundColor Red
        exit 1
    }
    Remove-Item $lock -Force
}

# ── 局域网 IP ─────────────────────────────────────────────────────────────
# 优先取默认路由网卡（真实局域网 IP），避免选中虚拟网卡（WSL/VM）
$ip = $null
try {
    $ifIndex = (Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue |
        Sort-Object RouteMetric |
        Select-Object -First 1).InterfaceIndex
    if ($ifIndex) {
        $ip = (Get-NetIPAddress -AddressFamily IPv4 -InterfaceIndex $ifIndex -ErrorAction SilentlyContinue).IPAddress
    }
} catch { }
if (-not $ip) {
    try {
        $ip = (Get-NetIPAddress -AddressFamily IPv4 |
            Where-Object {
                $_.IPAddress -notlike '127.*' -and
                $_.IPAddress -notlike '169.254.*' -and
                $_.IPAddress -ne '0.0.0.0'
            } |
            Sort-Object -Property InterfaceMetric |
            Select-Object -First 1).IPAddress
    } catch { }
}
if (-not $ip) { $ip = '127.0.0.1' }

# ── Token ─────────────────────────────────────────────────────────────────
$token = -join ((1..16) | ForEach-Object { '{0:x2}' -f (Get-Random -Max 256) })

Write-Host ''
Write-Host '══════════════════════════════════════════════════' -ForegroundColor Cyan
Write-Host '  QAQ-Harness · 局域网 Web' -ForegroundColor Cyan
Write-Host "  访问地址 : http://$ip`:$Port/debug/" -ForegroundColor Green
Write-Host "  token    : $token" -ForegroundColor Yellow
Write-Host '  按 Ctrl+C 停止' -ForegroundColor DarkGray
Write-Host '══════════════════════════════════════════════════' -ForegroundColor Cyan
Write-Host ''

# ── 启动 daemon（前台，Ctrl+C 停止）────────────────────────────────────────
$env:QAQH_DEBUG_RENDERER_DIR = $dist
$env:QAQH_SERVER_TOKEN = $token
& $daemon server --bind 0.0.0.0 --port $Port
