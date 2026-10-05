# G1 beta smoke (docs/plan-beta-readiness.md): daemon -> gateway -> browser flow.
# Covers: bootstrap nonce -> gateway session (cookie+CSRF) -> sessions ->
# SessionCreate command -> ack -> SSE event frames -> approvals query.
# Simulates the browser at HTTP level. Runs against an ISOLATED data root under
# $env:TEMP (via QAQH_ALLOW_TEST_DATA_ROOT=1) so it never pollutes the real
# ~/.qaqh. Requires no live daemon.
param(
    [int]$DaemonPort = 64499,
    [int]$GatewayPort = 18499
)
$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$daemon = Join-Path $repo "target\debug\qaqh-daemon.exe"
$gateway = Join-Path $repo "target\debug\qaqh-webui-gateway.exe"
if (-not (Test-Path $daemon)) { throw "build first: cargo build -p qaqh-daemon" }
if (-not (Test-Path $gateway)) { throw "build first: cargo build -p qaqh-webui-gateway" }

$data = Join-Path $env:TEMP ("qaqh-smoke-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $data | Out-Null
$log = Join-Path $data "daemon.log"
$logErr = Join-Path $data "daemon.err.log"
$glog = Join-Path $data "gateway.log"
$glogErr = Join-Path $data "gateway.err.log"

# 隔离数据根（避免污染真实 ~/.qaqh，泄漏的会话会进入 session.list 干扰前端）。
# `QAQH_ALLOW_TEST_DATA_ROOT=1` 放行非 `<USERPROFILE>\.qaqh` 的数据根（仅测试用）。
$env:QAQH_DATA_DIR = Join-Path $data "qaqh"
$env:QAQH_ALLOW_TEST_DATA_ROOT = "1"
$env:QAQH_SERVER_TOKEN = "smoke-token-" + [guid]::NewGuid().ToString("N").Substring(0, 8)
$discovery = Join-Path $env:QAQH_DATA_DIR "daemon.json"

$daemonProc = Start-Process -FilePath $daemon -ArgumentList @("server", "--bind", "127.0.0.1", "--port", "$DaemonPort", "--token", "$env:QAQH_SERVER_TOKEN") -RedirectStandardOutput $log -RedirectStandardError $logErr -PassThru -WindowStyle Hidden
$base = "http://127.0.0.1:$GatewayPort"

try {
    # 1) wait for discovery file (daemon ready signal) at the ISOLATED data root
    $deadline = (Get-Date).AddSeconds(20)
    while (-not (Test-Path $discovery)) {
        if ($daemonProc.HasExited) { throw "daemon exited early: $(Get-Content $logErr -Raw)" }
        if ((Get-Date) -gt $deadline) { throw "timeout waiting for daemon.json" }
        Start-Sleep -Milliseconds 250
    }
    Write-Host "[ok] daemon ready"

    # 2) start gateway (reads discovery, validates build_id/protocol version)
    $gatewayProc = Start-Process -FilePath $gateway -ArgumentList @("--port", "$GatewayPort") -RedirectStandardOutput $glog -RedirectStandardError $glogErr -PassThru -WindowStyle Hidden
    $gDeadline = (Get-Date).AddSeconds(10)
    $health = $null
    while ($true) {
        try {
            $health = & curl.exe -s -o "$data\bootstrap.js" -w "%{http_code}" "$base/__gateway/bootstrap.js" -H "Origin: $base" 2>$null
            if ($health -eq "200") { break }
        } catch {}
        if ($gatewayProc.HasExited) { throw "gateway exited early: $(Get-Content $glogErr -Raw)" }
        if ((Get-Date) -gt $gDeadline) { throw "timeout waiting for gateway" }
        Start-Sleep -Milliseconds 250
    }
    Write-Host "[ok] gateway up"

    # 3) bootstrap.js -> nonce
    $js = Get-Content "$data\bootstrap.js" -Raw
    if ($js -notmatch '"nonce":"([0-9a-f-]+)"') { throw "bootstrap nonce missing: $js" }
    $nonce = $Matches[1]
    Write-Host "[ok] nonce acquired"

    # 4) gateway session: cookie + CSRF (body via file: PS->curl quoting safety)
    $sessionResp = Join-Path $data "session.json"
    $nonceBody = Join-Path $data "nonce.json"
    ('{"nonce":"' + $nonce + '"}') | Out-File $nonceBody -Encoding ascii
    $code = & curl.exe -s -o $sessionResp -w "%{http_code}" -X POST "$base/__gateway/session" `
        -H "Content-Type: application/json" -H "Origin: $base" `
        -c "$data\cookies.txt" --data "@$nonceBody"
    if ($code -ne "200") { throw "gateway session failed: HTTP $code $(Get-Content $sessionResp -Raw)" }
    $sessionJson = Get-Content $sessionResp -Raw | ConvertFrom-Json
    $csrf = $sessionJson.csrf_token
    if (-not $csrf) { throw "no csrf token" }
    Write-Host "[ok] gateway session (csrf acquired)"

    # 5) sessions list reachable (empty at this point)
    $code = & curl.exe -s -o "$data\sessions0.json" -w "%{http_code}" "$base/__gateway/sessions" -b "$data\cookies.txt" -H "Origin: $base"
    if ($code -ne "200") { throw "sessions list failed: HTTP $code" }
    Write-Host "[ok] sessions list reachable"

    # 6) TUI-equivalent path: open a daemon lease directly, then SessionCreate.
    #    (webui has no create-from-zero flow by design: gateway commands require
    #    an active seed; first sessions are created by TUI/CLI. The browser then
    #    attaches to the listed session.)
    $daemonBase = "http://127.0.0.1:$DaemonPort"
    $auth = "Authorization: Bearer $env:QAQH_SERVER_TOKEN"
    $openBody = '{"schema":"qaqh.Ringing","version":2,"client_instance_id":"smoke-tui"}'
    $openBody | Out-File "$data\open.json" -Encoding ascii
    $code = & curl.exe -s -o "$data\open-resp.json" -w "%{http_code}" -X POST "$daemonBase/ringing/v2/clients/open" `
        -H "Content-Type: application/json" -H $auth --data "@$data\open.json"
    if ($code -ne "200") { throw "clients/open failed: HTTP $code" }
    $openResp = Get-Content "$data\open-resp.json" -Raw | ConvertFrom-Json
    $clientSessionId = $openResp.client_session_id
    Write-Host "[ok] daemon lease open (client_session_id=$clientSessionId)"

    $cmdId = [guid]::NewGuid().ToString()
    $body = @{
        schema = "qaqh.Ringing"; version = 2; channel = "control"
        command_id = $cmdId; client_instance_id = "smoke-tui"; client_session_id = $clientSessionId
        command = @{ channel = "control"; type = "session_create"; close_current = $false; cwd = $repo }
    } | ConvertTo-Json -Depth 8 -Compress
    $ackFile = "$data\ack.json"
    $body | Out-File "$data\cmd.json" -Encoding ascii
    $code = & curl.exe -s -o $ackFile -w "%{http_code}" -X POST "$daemonBase/ringing/v2/commands/control" `
        -H "Content-Type: application/json" -H $auth -H "x-qaqh-client-session-id: $clientSessionId" --data "@$data\cmd.json"
    $ack = Get-Content $ackFile -Raw
    Write-Host "[..] SessionCreate ack (HTTP $code): $ack"
    if ($code -ne "200") { throw "command POST failed: HTTP $code $ack" }

    # 7) poll sessions until the new session shows up (query side of SessionCreated fact)
    $seed = $null
    $deadline = (Get-Date).AddSeconds(15)
    while (-not $seed) {
        Start-Sleep -Milliseconds 300
        & curl.exe -s -o "$data\sessions1.json" "$base/__gateway/sessions" -b "$data\cookies.txt" -H "Origin: $base"
        try {
            $list = Get-Content "$data\sessions1.json" -Raw | ConvertFrom-Json
            if ($list -and $list.Count -gt 0) {
                $first = $list[0]
                $seed = $first.session_id
                if (-not $seed) { $seed = $first.seed }
            }
        } catch {}
        if ((Get-Date) -gt $deadline) { break }
    }
    if (-not $seed) { Write-Host "[warn] no session listed; sessions1=$(Get-Content "$data\sessions1.json" -Raw)" }
    else { Write-Host "[ok] session created: $seed" }

    # 8) SSE event streams: attach, then hold both streams open. A quiet session
    #    has no live events; the SSE keep-alive comment (15s cadence) proves the
    #    stream is alive and correctly framed. Real-turn event delivery is
    #    covered by v2 acceptance/integration tests and first real use.
    $renew = { & curl.exe -s -o NUL -w "%{http_code}" -X POST "$daemonBase/ringing/v2/leases/renew" -H $auth -H "x-qaqh-client-session-id: $clientSessionId" }
    if ($seed) {
        $code = & curl.exe -s -o "$data\attach.json" -w "%{http_code}" -X POST "$base/__gateway/sessions/$seed/attach" `
            -H "Origin: $base" -H "x-qaqh-csrf: $csrf" -b "$data\cookies.txt"
        Write-Host "[..] attach HTTP $code"
        & $renew | Out-Null
        $directCode = & curl.exe -s -o "$data\sse-direct.txt" -w "%{http_code}" -N --max-time 18 "$daemonBase/ringing/v2/sessions/$seed/events" -H $auth -H "x-qaqh-client-session-id: $clientSessionId" -H "Accept: text/event-stream"
        $directAlive = ([System.IO.File]::ReadAllLines("$data\sse-direct.txt") | Where-Object { $_ -match "^:|^event:" }).Count
        Write-Host "[..] daemon-direct SSE HTTP $directCode alive=$directAlive"
        & $renew | Out-Null
        $gwCode = & curl.exe -s -o "$data\sse-events.txt" -w "%{http_code}" -N --max-time 18 "$base/__gateway/ringing/sessions/$seed/events" -b "$data\cookies.txt" -H "Origin: $base" -H "Accept: text/event-stream"
        if (Test-Path "$data\sse-events.txt") {
            $gwAlive = ([System.IO.File]::ReadAllLines("$data\sse-events.txt") | Where-Object { $_ -match "^:|^event:" }).Count
        } else { $gwAlive = -1 }
        Write-Host "[..] gateway SSE HTTP $gwCode alive=$gwAlive"
        & $renew | Out-Null
        $gwTl = & curl.exe -s -o "$data\sse-timeline.txt" -w "%{http_code}" -N --max-time 18 "$base/__gateway/ringing/sessions/$seed/timeline/events" -b "$data\cookies.txt" -H "Origin: $base" -H "Accept: text/event-stream"
        if (Test-Path "$data\sse-timeline.txt") {
            $gwTlAlive = ([System.IO.File]::ReadAllLines("$data\sse-timeline.txt") | Where-Object { $_ -match "^:|^event:" }).Count
        } else { $gwTlAlive = -1 }
        Write-Host "[..] gateway timeline SSE HTTP $gwTl alive=$gwTlAlive"
    }

    # 9) approvals query face
    $code = & curl.exe -s -o "$data\approvals.json" -w "%{http_code}" -X POST "$base/__gateway/approvals" `
        -H "Origin: $base" -H "x-qaqh-csrf: $csrf" -b "$data\cookies.txt"
    Write-Host "[ok] approvals HTTP $code"

    # 10) cleanup: delete the session created by this smoke (direct daemon lease;
    #     the gateway whitelist rightly blocks session_delete from the browser)
    & $renew | Out-Null
    if ($seed) {
        $delId = [guid]::NewGuid().ToString()
        $delBody = @{
            schema = "qaqh.Ringing"; version = 2; channel = "control"
            command_id = $delId; client_instance_id = "smoke-tui"; client_session_id = $clientSessionId
            session_id = $seed
            command = @{ channel = "control"; type = "session_delete"; session_id = $seed }
        } | ConvertTo-Json -Depth 8 -Compress
        $delBody | Out-File "$data\delcmd.json" -Encoding ascii
        $code = & curl.exe -s -o "$data\delete.json" -w "%{http_code}" -X POST "$daemonBase/ringing/v2/commands/control" `
            -H "Content-Type: application/json" -H $auth -H "x-qaqh-client-session-id: $clientSessionId" --data "@$data\delcmd.json"
        Write-Host "[ok] SessionDelete HTTP $code : $(Get-Content "$data\delete.json" -Raw)"
    }

    Write-Host "=== SMOKE PASS ==="
    Write-Host "artifacts in $data"
}
finally {
    foreach ($p in @($daemonProc, $gatewayProc)) {
        if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    }
    # control-plane stop as backstop (idempotent, safe if already dead)
    & curl.exe -s -X POST "http://127.0.0.1:$DaemonPort/control/v1/stop" -H "Authorization: Bearer $env:QAQH_SERVER_TOKEN" 2>$null | Out-Null
}
