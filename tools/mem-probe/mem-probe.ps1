# mem-probe.ps1 — 一键跑完整内存探测（只读）
#
# 用法：
#   pwsh -File tools\mem-probe\mem-probe.ps1                       # 自动发现 qaqh-daemon
#   pwsh -File tools\mem-probe\mem-probe.ps1 -TargetPid 14464      # 指定 PID
#   pwsh -File tools\mem-probe\mem-probe.ps1 -Name qaqh-tui        # 指定进程名
#   pwsh -File tools\mem-probe\mem-probe.ps1 -Watch -Minutes 10    # 每 30s 采样内存趋势
#
# 注意：参数名是 -TargetPid 而非 -Pid，因为 PowerShell 的 $PID 是只读自动变量，
#       $Pid 会与之冲突（大小写不敏感）导致脚本静默失效。
#
# 输出全部落在 tools\mem-probe\out\<pid>\ 下（已 gitignore，勿提交）。
# 全程只读：仅 OpenProcess(QUERY|VM_READ) + ReadProcessMemory，绝不写目标进程。

[CmdletBinding()]
param(
  [int]$TargetPid = 0,
  [string]$Name = "qaqh-daemon",
  [switch]$Watch,
  [int]$Minutes = 10,
  [int]$TopDumpKB = 64
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# ── 定位目标进程 ──────────────────────────────────────────────
if ($TargetPid -eq 0) {
  $proc = Get-Process -Name $Name -ErrorAction SilentlyContinue | Sort-Object WorkingSet64 -Descending | Select-Object -First 1
  if (-not $proc) { throw "未找到进程 '$Name'。用 -TargetPid 或 -Name 指定。" }
  $TargetPid = $proc.Id
} else {
  $proc = Get-Process -Id $TargetPid -ErrorAction SilentlyContinue
  if (-not $proc) { throw "PID $TargetPid 不存在。" }
}

$outDir = Join-Path $here "out\$TargetPid"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function Show-Proc {
  $p = Get-Process -Id $TargetPid -ErrorAction SilentlyContinue
  if (-not $p) { return $null }
  [pscustomobject]@{
    Time      = (Get-Date).ToString("HH:mm:ss")
    WS_MB     = [math]::Round($p.WorkingSet64/1MB, 1)
    PM_MB     = [math]::Round($p.PrivateMemorySize64/1MB, 1)
    Threads   = $p.Threads.Count
    Handles   = $p.HandleCount
  }
}

Write-Host "== mem-probe ==" -ForegroundColor Cyan
$snap = Show-Proc
$snap | Format-List
$uptime = ((Get-Date) - $proc.StartTime).TotalMinutes
Write-Host ("uptime = {0:N1} min  (started {1})" -f $uptime, $proc.StartTime)
if ($uptime -gt 1) {
  Write-Host ("avg growth since start = {0:N2} MB/min" -f ($snap.PM_MB / $uptime)) -ForegroundColor Yellow
}
Write-Host ""

# ── 编译探测器 ────────────────────────────────────────────────
$cs = Join-Path $here "MemProbe.cs"
if (-not (Test-Path $cs)) { throw "缺少 MemProbe.cs（应在 $here）" }
Add-Type -TypeDefinition (Get-Content $cs -Raw) -Language CSharp
Write-Host "MemProbe 编译完成" -ForegroundColor Green
Write-Host ""

# ── 跑五项探测 ────────────────────────────────────────────────
$steps = @(
  @{ n = "1/5 区域枚举 + 同尺寸聚集"; f = { [MemProbe]::Regions($TargetPid, (Join-Path $outDir "regions.txt")) } },
  @{ n = "2/5 字节构成";             f = { [MemProbe]::Composition($TargetPid, (Join-Path $outDir "composition.txt")) } },
  @{ n = "3/5 关键词计数";           f = { [MemProbe]::Markers($TargetPid, (Join-Path $outDir "markers.txt")) } },
  @{ n = "4/5 可打印串去重";         f = { [MemProbe]::Dup($TargetPid, (Join-Path $outDir "dedup.txt"), 64) } },
  @{ n = "5/5 大区域抽样";           f = { [MemProbe]::Dump($TargetPid, (Join-Path $outDir "dump-big.txt"), 8000, 12000, 4, $TopDumpKB) } }
)
foreach ($s in $steps) {
  Write-Host $s.n -ForegroundColor Cyan
  & $s.f
}

# ── 可选：趋势采样 ────────────────────────────────────────────
if ($Watch) {
  Write-Host ""
  Write-Host "== 趋势采样（每 30s，共 $Minutes 分钟）==" -ForegroundColor Cyan
  $csv = Join-Path $outDir "trend.csv"
  "time,ws_mb,pm_mb,threads,handles" | Set-Content $csv -Encoding UTF8
  $deadline = (Get-Date).AddMinutes($Minutes)
  while ((Get-Date) -lt $deadline) {
    $s = Show-Proc
    if (-not $s) { Write-Host "进程已退出"; break }
    "{0},{1},{2},{3},{4}" -f $s.Time, $s.WS_MB, $s.PM_MB, $s.Threads, $s.Handles | Add-Content $csv -Encoding UTF8
    Write-Host ("  {0}  WS={1,7} PM={2,7} Th={3,3} H={4}" -f $s.Time, $s.WS_MB, $s.PM_MB, $s.Threads, $s.Handles)
    Start-Sleep -Seconds 30
  }
  Write-Host "趋势已写入 $csv" -ForegroundColor Green
}

Write-Host ""
Write-Host "== 完成，输出目录 ==" -ForegroundColor Green
Write-Host $outDir
Get-ChildItem $outDir | ForEach-Object { "  {0,10} B  {1}" -f $_.Length, $_.Name }
