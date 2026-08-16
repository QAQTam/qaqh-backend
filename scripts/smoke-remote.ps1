# Smoke test: qaqh-daemon --server (LAN/headless mode) + qaqh-client remote direct connect.
#
# Non-interactive. Usage:
#   powershell -ExecutionPolicy Bypass -File .\scripts\smoke-remote.ps1
#   (or: pwsh -File .\scripts\smoke-remote.ps1)
#
# What it verifies:
#   1. daemon builds, then starts in `server` mode on 127.0.0.1:0 with a fixed token
#   2. SDK connects remotely (skips local daemon.json discovery / spawn)
#   3. fs.list / fs.read queries return daemon-side files
#   4. `//ip/...` display-path round-trip assertion inside the example
#
# It uses an isolated temp home + .deepx data dir, so the real
# %USERPROFILE%\.deepx daemon is never touched.

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$base = Join-Path $env:TEMP "qaqh-remote-smoke"
$dataDir = Join-Path $base ".deepx"
$daemonExe = Join-Path $repo "target\debug\qaqh-daemon.exe"
$exampleExe = Join-Path $repo "target\debug\examples\remote_fs.exe"
$token = "smoke-secret-token"

function Fail($msg) {
    Write-Host "[SMOKE] FAIL: $msg" -ForegroundColor Red
    exit 1
}

# ── 0. Kill leftover smoke processes (only binaries under this repo's target\debug) ──
Get-CimInstance Win32_Process -Filter "Name='qaqh-daemon.exe' OR Name='qaqh-workspace.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith((Join-Path $repo "target\debug"), [System.StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }

if (Test-Path $base) {
    Remove-Item -Recurse -Force $base -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
}
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null

# ── 1. Build (daemon must be up-to-date; example contains the assertions) ──
Write-Host "[SMOKE] building daemon + remote_fs example..." -ForegroundColor Cyan
Push-Location $repo
try {
    & cargo build -p qaqh-daemon -p qaqh-client --example remote_fs 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
} finally {
    Pop-Location
}

# ── 2. Start isolated daemon in server mode ──
# Windows platform guard requires the data root to be <home>\.deepx, so we
# point USERPROFILE/HOME at the temp base for the daemon child only.
$savedUserProfile = $env:USERPROFILE
$savedHome = $env:HOME
$savedDataDir = $env:QAQH_DATA_DIR
$env:USERPROFILE = $base
$env:HOME = $base
$env:QAQH_DATA_DIR = $dataDir

Write-Host "[SMOKE] starting qaqh-daemon server..." -ForegroundColor Cyan
$daemon = Start-Process -FilePath $daemonExe `
    -ArgumentList 'server','--bind','127.0.0.1','--port','0','--token',$token `
    -WorkingDirectory $repo -PassThru -WindowStyle Hidden `
    -RedirectStandardOutput (Join-Path $base "daemon.out.log") `
    -RedirectStandardError (Join-Path $base "daemon.err.log")

# ── 3. Wait for discovery (published only after HTTP is ready) ──
$discPath = Join-Path $dataDir "daemon.json"
$deadline = (Get-Date).AddSeconds(30)
while (-not (Test-Path $discPath) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
}
if (-not (Test-Path $discPath)) {
    $err = if (Test-Path (Join-Path $base "daemon.err.log")) { Get-Content (Join-Path $base "daemon.err.log") -Raw } else { "" }
    Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
    Fail "discovery file not published. daemon stderr: $err"
}
if ($daemon.HasExited) {
    $err = if (Test-Path (Join-Path $base "daemon.err.log")) { Get-Content (Join-Path $base "daemon.err.log") -Raw } else { "" }
    Fail "daemon exited early (code $($daemon.ExitCode)): $err"
}

$disc = Get-Content $discPath -Raw | ConvertFrom-Json
$port = ([uri]$disc.endpoint).Port
Write-Host ("[SMOKE] daemon ready: port={0} pid={1} channel={2}" -f $port, $disc.pid, $disc.channel) -ForegroundColor Green
if ($disc.token -ne $token) { Fail "discovery token mismatch" }

# ── 4. Drive the SDK remote path: connect -> open lease -> fs.list -> fs.read ──
$env:QAQH_REMOTE_URL = "http://127.0.0.1:$port"
$env:QAQH_REMOTE_TOKEN = $token

$dirArg = (($repo -replace '\\','/') + "/crates")
$fileArg = (($repo -replace '\\','/') + "/version.txt")
$outFile = Join-Path $base "remote_fs.out.txt"
$errFile = Join-Path $base "remote_fs.err.txt"

Write-Host "[SMOKE] running remote_fs (fs.list: $dirArg)" -ForegroundColor Cyan
& $exampleExe $dirArg $fileArg 1> $outFile 2> $errFile
$exitCode = $LASTEXITCODE

$out = if (Test-Path $outFile) { Get-Content $outFile -Raw } else { "" }
$err = if (Test-Path $errFile) { Get-Content $errFile -Raw } else { "" }
Write-Host "----- remote_fs stdout -----"
Write-Host $out
if ($err) { Write-Host "----- remote_fs stderr -----"; Write-Host $err }

# ── 5. Cleanup + verdict ──
Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Remove-Item -Recurse -Force $base -ErrorAction SilentlyContinue

$env:USERPROFILE = $savedUserProfile
$env:HOME = $savedHome
$env:QAQH_DATA_DIR = $savedDataDir

if ($exitCode -ne 0) { Fail "remote_fs exited with code $exitCode" }
if ($out -notmatch "fs\.list") { Fail "fs.list output missing" }
if ($out -notmatch "1\.0\.0-rc\.6") { Fail "fs.read content missing (expected version text)" }
if ($out -notmatch "-> display: //127\.0\.0\.1:$port/") { Fail "//ip/ display-path form missing" }

Write-Host "[SMOKE] PASS: server mode + remote connect + fs.list + fs.read + //ip/ path bridge" -ForegroundColor Green
exit 0
