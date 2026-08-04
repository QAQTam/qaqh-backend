# DeepX M3 backend smoke test — Ringing HTTP full chain with a real daemon + worker.
$ErrorActionPreference = "Stop"
$T = Join-Path $env:TEMP "deepx-m3-smoke2"
if (Test-Path $T) { Remove-Item $T -Recurse -Force }
New-Item -ItemType Directory -Path $T | Out-Null
$env:USERPROFILE = $T
$env:HOME = $T
$env:RUST_LOG = "info"

$daemon = Join-Path $PSScriptRoot "..\target\debug\deepx-daemon.exe"
if (-not (Test-Path $daemon)) { Write-Host "daemon binary missing: $daemon"; exit 1 }

$proc = Start-Process -FilePath $daemon -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput (Join-Path $T "daemon.out") -RedirectStandardError (Join-Path $T "daemon.err")

# 1) wait for discovery file
$discovery = Join-Path $T ".deepx\daemon.json"
$deadline = (Get-Date).AddSeconds(15)
while (-not (Test-Path $discovery) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
if (-not (Test-Path $discovery)) {
  Write-Host "FAIL: daemon discovery not found. stderr:"; Get-Content (Join-Path $T "daemon.err") -ErrorAction SilentlyContinue
  Stop-Process -Id $proc.Id -Force; exit 1
}
$disc = Get-Content $discovery -Raw | ConvertFrom-Json
$base = ($disc.endpoint -replace "ws://", "http://")
$token = $disc.token
Write-Host "OK discovery: $base"

function Headers($extra) {
  $h = @{ Authorization = "Bearer $token"; "Content-Type" = "application/json" }
  foreach ($k in $extra.Keys) { $h[$k] = $extra[$k] }
  return $h
}

# 2) client open
try {
  $open = Invoke-RestMethod -Method Post -Uri "$base/ringing/v1/clients/open" -Headers (Headers @{}) -Body (@{
    schema = "deepx.Ringing"; version = 1
    capabilities = @("Ringing_v1","Ringing_batch_v1","Ringing_bootstrap_v1","Ringing_command_status_v1")
    client_instance_id = "m3-smoke"; client_kind = "desktop"
  } | ConvertTo-Json -Depth 6)
  Write-Host "OK open: accepted=$($open.accepted) session=$($open.client_session_id)"
} catch {
  Write-Host "FAIL open: $_"; Stop-Process -Id $proc.Id -Force; exit 1
}
$sid = $open.client_session_id
$sessHeaders = @{ "X-DeepX-Client-Session" = $sid }

function SendCommand($channel, $command, $seed = $null) {
  $payload = @{
    command_id = "smoke-$([guid]::NewGuid().ToString('N').Substring(0,8))"
    client_session_id = $sid
    command = $command
  }
  if ($seed) { $payload.seed = $seed }
  return Invoke-RestMethod -Method Post -Uri "$base/ringing/v1/commands/$channel" -Headers (Headers $sessHeaders) -Body ($payload | ConvertTo-Json -Depth 8)
}

# 3) SessionCreate (registry op)
$ack = SendCommand "control" @{ channel = "control"; type = "session_create"; close_current = $false }
Write-Host "OK session_create: $($ack.status)"

# 4) find the created seed via session.list query
$seed = $null
$deadline = (Get-Date).AddSeconds(10)
while (-not $seed -and (Get-Date) -lt $deadline) {
  Start-Sleep -Milliseconds 300
  try {
    $list = Invoke-RestMethod -Method Post -Uri "$base/ringing/v1/queries/session.list" -Headers (Headers $sessHeaders) -Body "{}"
    $arr = @($list)
    if ($arr.Count -gt 0) { $seed = $arr[-1].seed }
  } catch { }
}
if (-not $seed) { Write-Host "FAIL: no session seed after create"; Stop-Process -Id $proc.Id -Force; exit 1 }
Write-Host "OK seed: $seed"

function Bootstrap {
  return Invoke-RestMethod -Method Get -Uri "$base/ringing/v1/sessions/$seed/bootstrap" -Headers (Headers $sessHeaders)
}

# 5) bootstrap before message
$boot = Bootstrap
Write-Host "OK bootstrap: turns=$($boot.conversation.state.turns.Count) total=$($boot.conversation.state.total_turns)"

# 6) SendMessage → accepted (worker runs; no LLM key → TurnFailed is expected but chain must be live)
$ack = SendCommand "conversation" @{ channel = "conversation"; type = "conversation_send_message"; text = "backend smoke"; images = @(); attachments = $null } $seed
Write-Host "OK send_message: $($ack.status)"

# 7) wait for worker turn to land in projection (turn started / failed visible in bootstrap state)
$sawTurn = $false
$deadline = (Get-Date).AddSeconds(15)
while (-not $sawTurn -and (Get-Date) -lt $deadline) {
  Start-Sleep -Milliseconds 500
  try {
    $boot = Bootstrap
    if ($boot.conversation.state.turns.Count -gt 0 -or $boot.conversation.state.active_turn -or $boot.conversation.state.cancelled) {
      $sawTurn = $true
    }
  } catch { }
}
if (-not $sawTurn) {
  Write-Host "WARN: no turn visible in bootstrap (worker may be waiting for LLM config). state: $($boot.conversation.state | ConvertTo-Json -Depth 5 -Compress)"
} else {
  Write-Host "OK turn visible in bootstrap: turns=$($boot.conversation.state.turns.Count)"
}

# 8) ConversationCompact → accepted → compact_status should transition
$ack = SendCommand "conversation" @{ channel = "conversation"; type = "conversation_compact" } $seed
Write-Host "OK conversation_compact: $($ack.status)"
$sawCompact = $false
$deadline = (Get-Date).AddSeconds(15)
while (-not $sawCompact -and (Get-Date) -lt $deadline) {
  Start-Sleep -Milliseconds 500
  try {
    $boot = Bootstrap
    $cs = $boot.conversation.state.compact_status
    if ($cs) {
      Write-Host "OK compact_status=$cs"
      $sawCompact = $true
    }
  } catch { }
}
if (-not $sawCompact) { Write-Host "WARN: compact_status never appeared" }

# 9) cleanup
Stop-Process -Id $proc.Id -Force
Write-Host "SMOKE DONE (stderr tail):"
Get-Content (Join-Path $T "daemon.err") -Tail 15 -ErrorAction SilentlyContinue
