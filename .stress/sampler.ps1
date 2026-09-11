# 内存 + 文件采样器：每 5 秒记录 daemon 工作集/私有内存与 timeline 目录大小到 CSV
param()
$ErrorActionPreference = "SilentlyContinue"
$pidDaemon = Get-Content "D:\project\QAQ-Harness\.stress\daemon.pid" -ErrorAction SilentlyContinue
$timelineDir = "D:\project\QAQ-Harness\.stress\qaqh-home\.qaqh\ringing\ringing-timeline"
$csv = "D:\project\QAQ-Harness\.stress\samples.csv"
"ts,ws_mb,private_mb,timeline_mb" | Set-Content $csv
while ($true) {
  $p = Get-Process -Id $pidDaemon -ErrorAction SilentlyContinue
  if (-not $p) { "daemon gone" | Add-Content $csv; break }
  $size = 0
  if (Test-Path $timelineDir) {
    Get-ChildItem $timelineDir -File -ErrorAction SilentlyContinue | ForEach-Object { $size += $_.Length }
  }
  "{0},{1:N1},{2:N1},{3:N2}" -f (Get-Date -Format "HH:mm:ss"), ($p.WorkingSet64/1MB), ($p.PrivateMemorySize64/1MB), ($size/1MB) | Add-Content $csv
  Start-Sleep -Seconds 5
}
