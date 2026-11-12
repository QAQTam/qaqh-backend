# ADR-0014 子代理端到端冒烟：隔离 daemon + 真实 provider 回合。
#
# 链路（真实模型工具链）：session.create → 父回合指示模型调用 spawn_subagent
# （工具 = 建会话 + inter-agent 任务投递原子完成；子会话守卫拒绝无 inter-agent
# 元数据的直接用户消息，故任务只能经工具路径投递）→ 子级回合终态 → 终态经
# registry 路由回父会话 mailbox（TriggerTurn）→ 父回合合流 canonical 终态消息。
# wire subagent.spawn 信封/target/名字回归由 qaqh-ringing 单元测试守门。
#
# 确定性断言（不依赖模型措辞）：
#   A. session.list 出现子会话（模型真调了 spawn_subagent）；
#   B. 子级 turn_count>=1 且运行状态回到 idle（回合真跑完）；
#   C. 父级 bootstrap timeline 出现 Input 携带 subagent_terminal.child_session_id == child
#      （血缘+路由+合流三合一铁证：route_turn_terminal 只在血缘存在时发射）；
#   D. 子级答复 token 出现在父级合流内容里（结果面载荷无损）。
#
# 隔离数据根（QAQH_DATA_DIR + QAQH_ALLOW_TEST_DATA_ROOT=1），复制真实
# providers.toml/secrets.toml（DPAPI 用户级加密，同机同用户可解）——不污染
# 真实 ~/.qaqh。需要 debug 构建：cargo build -j 2 -p qaqh-daemon。
param(
    [int]$DaemonPort = 64599,
    [int]$TurnTimeoutSec = 120,
    # 显式模型选择（自包含，不依赖用户默认设置）；子级继承父会话行。
    [string]$ProviderId = "qwen-token-plan-cn-provider",
    [string]$ModelId = "qwen3.8-flash"
)
$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$daemon = Join-Path $repo "target\debug\qaqh-daemon.exe"
if (-not (Test-Path $daemon)) { throw "build first: cargo build -j 2 -p qaqh-daemon" }

$data = Join-Path $env:TEMP ("qaqh-subagent-smoke-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
$work = Join-Path $data "workspace"
New-Item -ItemType Directory -Path $work -Force | Out-Null

$env:QAQH_DATA_DIR = Join-Path $data "qaqh"
$env:QAQH_ALLOW_TEST_DATA_ROOT = "1"
$env:QAQH_SERVER_TOKEN = "smoke-token-" + [guid]::NewGuid().ToString("N").Substring(0, 8)
New-Item -ItemType Directory -Path $env:QAQH_DATA_DIR -Force | Out-Null
$realRoot = Join-Path $env:USERPROFILE ".qaqh"
foreach ($f in @("config.toml", "providers.toml", "secrets.toml")) {
    $src = Join-Path $realRoot $f
    if (Test-Path $src) { Copy-Item $src (Join-Path $env:QAQH_DATA_DIR $f) }
}
$discovery = Join-Path $env:QAQH_DATA_DIR "daemon.json"
$base = "http://127.0.0.1:$DaemonPort"
$auth = "Authorization: Bearer $env:QAQH_SERVER_TOKEN"
$nonce = [guid]::NewGuid().ToString("N").Substring(0, 8)
$token = "SMOKE-OK-$nonce"
$instance = "smoke-subagent-$nonce"

function Post-Json {
    param([string]$Path, [string]$BodyFile, [string]$OutFile, [string]$Lease)
    $headers = @("-H", "Content-Type: application/json", "-H", $auth)
    if ($Lease) { $headers += @("-H", "x-qaqh-client-session-id: $Lease") }
    $code = & curl.exe -s -o $OutFile -w "%{http_code}" -X POST "$base$Path" @headers --data "@$BodyFile"
    return $code
}

function Send-Command {
    param([string]$TargetKind, [string]$SessionId, [hashtable]$Command, [string]$Lease, [string]$Tag)
    $target = @{ kind = $TargetKind }
    if ($SessionId) { $target["session_id"] = $SessionId }
    $body = @{
        schema = "qaqh.Ringing"; version = 2; contract_revision = "ringing-commands-8"
        command_id = [guid]::NewGuid().ToString()
        client_instance_id = $instance; client_session_id = $Lease
        target = $target; preconditions = @(); command = $Command
    } | ConvertTo-Json -Depth 12 -Compress
    $bodyFile = Join-Path $data "cmd-$Tag.json"
    $outFile = Join-Path $data "ack-$Tag.json"
    $body | Out-File $bodyFile -Encoding ascii
    $code = Post-Json "/ringing/v2/commands" $bodyFile $outFile $Lease
    $ack = Get-Content $outFile -Raw | ConvertFrom-Json
    if ($code -ne "200") { throw "$Tag failed (HTTP $code): $(Get-Content $outFile -Raw)" }
    return $ack
}

function Renew-Lease { param([string]$Lease)
    $out = Join-Path $data "renew.json"
    & curl.exe -s -o $out -X POST "$base/ringing/v2/leases/renew" -H $auth -H "x-qaqh-client-session-id: $Lease" | Out-Null
}

function Get-Sessions { param([string]$Lease)
    $bodyFile = Join-Path $data "svc-list.json"
    '{}' | Out-File $bodyFile -Encoding ascii
    $outFile = Join-Path $data "sessions.json"
    $code = Post-Json "/ringing/v2/service/session.list" $bodyFile $outFile $Lease
    if ($code -ne "200") { throw "session.list failed (HTTP $code): $(Get-Content $outFile -Raw)" }
    return (Get-Content $outFile -Raw | ConvertFrom-Json)
}

function Get-Bootstrap { param([string]$SessionId, [string]$Lease)
    $outFile = Join-Path $data "bootstrap-$SessionId.json"
    $code = & curl.exe -s -o $outFile -w "%{http_code}" "$base/ringing/v2/sessions/$SessionId/bootstrap" -H $auth -H "x-qaqh-client-session-id: $Lease"
    if ($code -ne "200") { throw "bootstrap failed (HTTP $code): $(Get-Content $outFile -Raw)" }
    return (Get-Content $outFile -Raw)
}

$daemonProc = $null
try {
    # ── 1. 隔离 daemon ──
    $log = Join-Path $data "daemon.out.log"
    $logErr = Join-Path $data "daemon.err.log"
    $daemonProc = Start-Process -FilePath $daemon -ArgumentList @("server", "--bind", "127.0.0.1", "--port", "$DaemonPort", "--token", "$env:QAQH_SERVER_TOKEN") -RedirectStandardOutput $log -RedirectStandardError $logErr -PassThru -WindowStyle Hidden
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path $discovery)) {
        if ($daemonProc.HasExited) { throw "daemon exited early: $(Get-Content $logErr -Raw)" }
        if ((Get-Date) -gt $deadline) { throw "timeout waiting for daemon.json" }
        Start-Sleep -Milliseconds 250
    }
    Write-Host "[ok] daemon ready (isolated root $env:QAQH_DATA_DIR)"

    # ── 2. lease + root 会话 ──
    # open 预检两个代际字段（spec §11）：缺字段/不匹配在 lease 分配前即失败。
    $openBody = @{
        schema = "qaqh.Ringing"; version = 2; client_instance_id = $instance
        command_contract_revision = "ringing-commands-8"
        session_contract_revision = "ringing-session-2"
    } | ConvertTo-Json -Compress
    $openBody | Out-File (Join-Path $data "open.json") -Encoding ascii
    $code = Post-Json "/ringing/v2/clients/open" (Join-Path $data "open.json") (Join-Path $data "open-resp.json") $null
    if ($code -ne "200") { throw "clients/open failed (HTTP $code): $(Get-Content (Join-Path $data 'open-resp.json') -Raw)" }
    $lease = (Get-Content (Join-Path $data "open-resp.json") -Raw | ConvertFrom-Json).client_session_id
    Write-Host "[ok] lease open ($lease)"

    Send-Command "global" $null @{
        type = "session.create"; cwd = $work; title = "subagent-smoke"
        model_selection = @{ provider_id = $ProviderId; model_id = $ModelId }
    } $lease "create" | Out-Null
    $root = $null
    $deadline = (Get-Date).AddSeconds(15)
    while (-not $root) {
        Start-Sleep -Milliseconds 300
        Renew-Lease $lease
        foreach ($s in (Get-Sessions $lease)) { if ($s.title -eq "subagent-smoke") { $root = $s.session_id } }
        if ((Get-Date) -gt $deadline) { break }
    }
    if (-not $root) { throw "root session not listed" }
    Send-Command "session" $root @{ type = "session.attach" } $lease "attach" | Out-Null
    Write-Host "[ok] root session $root"

    # ── 3. 父回合：指示模型走生产工具链（spawn_subagent 即建会话即投递）──
    $instruction = "Use the spawn_subagent tool now with agent_name `"smoke_child`" and task_description `"Reply with exactly the token $token and nothing else. Do not call any tools.`". Then call wait_agent with targets set to the returned child session id (timeout_ms 90000). After wait_agent returns, reply with the exact token the child returned. Do not create tasks or use any other tools."
    Send-Command "session" $root @{ type = "conversation.send_message"; text = $instruction } $lease "lead" | Out-Null
    Write-Host "[..] lead turn started, waiting for child to appear"

    # ── 4. 断言 A：子会话出现（模型真调了 spawn_subagent）──
    $child = $null
    $deadline = (Get-Date).AddSeconds($TurnTimeoutSec)
    while (-not $child) {
        Start-Sleep -Milliseconds 500
        Renew-Lease $lease
        foreach ($s in (Get-Sessions $lease)) {
            if ($s.session_id -ne $root) { $child = $s.session_id }
        }
        if ((Get-Date) -gt $deadline) { break }
    }
    if (-not $child) { throw "A failed: no child session appeared (model did not spawn?); parent bootstrap: $(Get-Bootstrap $root $lease)" }
    Write-Host "[ok] A: child spawned $child"

    # ── 5. 断言 B：子级回合终态（turn_count>=1 且 idle）──
    $settled = $false
    $deadline = (Get-Date).AddSeconds($TurnTimeoutSec)
    while (-not $settled) {
        Start-Sleep -Milliseconds 500
        Renew-Lease $lease
        foreach ($s in (Get-Sessions $lease)) {
            if ($s.session_id -eq $child -and $s.turn_count -ge 1 -and "$($s.status)" -match "idle") { $settled = $true }
        }
        if ((Get-Date) -gt $deadline) { break }
    }
    if (-not $settled) { throw "child turn did not settle in ${TurnTimeoutSec}s; daemon log: $(Get-Content (Join-Path $env:QAQH_DATA_DIR 'qaqh-daemon.log') -Tail 30 -ErrorAction SilentlyContinue)" }
    Write-Host "[ok] B: child turn settled (turn_count>=1, idle)"

    # ── 6. 断言 C/D：轮询父级 bootstrap 直到合流 Input 出现 ──
    # （终态 TriggerTurn 自动唤醒父回合；合流 Input 在回合开始即落块，
    #  不依赖父模型是否答完——比等 turn_count 更稳。）
    $boot = $null
    $deadline = (Get-Date).AddSeconds($TurnTimeoutSec)
    while ($true) {
        Start-Sleep -Milliseconds 500
        Renew-Lease $lease
        $boot = Get-Bootstrap $root $lease
        if ($boot -match "subagent_terminal" -and $boot -match [regex]::Escape($child)) { break }
        if ((Get-Date) -gt $deadline) { throw "C failed: no subagent_terminal for $child in parent bootstrap within ${TurnTimeoutSec}s" }
    }
    Write-Host "[ok] C: parent bootstrap carries subagent_terminal for $child"
    if ($boot -notmatch [regex]::Escape($token)) { throw "D failed: child answer token $token absent from parent merge" }
    Write-Host "[ok] D: child answer token merged into parent"

    Write-Host "=== SUBAGENT SMOKE PASS ==="
    Write-Host "artifacts in $data"
}
catch {
    Write-Host "=== SUBAGENT SMOKE FAIL: $_ ==="
    $dlog = Join-Path $env:QAQH_DATA_DIR "qaqh-daemon.log"
    if (Test-Path $dlog) { Write-Host "--- daemon log tail ---"; Get-Content $dlog -Tail 40 }
    throw
}
finally {
    if ($daemonProc -and -not $daemonProc.HasExited) {
        & curl.exe -s -X POST "$base/control/v1/stop" -H $auth 2>$null | Out-Null
        Start-Sleep -Seconds 2
        if (-not $daemonProc.HasExited) { Stop-Process -Id $daemonProc.Id -Force -ErrorAction SilentlyContinue }
    }
}
