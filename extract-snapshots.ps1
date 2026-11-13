# Merge ALL read_file ranges per source path; honest gap markers; coverage manifest.
$ErrorActionPreference = 'Stop'
$exportDir = 'C:/Users/QAQTam/Downloads/xs/exported'
$recon = 'D:/Projects/deepx-desktop-recon'
$srcPrefix = 'D:\project\DeepX-Desktop\'

$files = Get-ChildItem $exportDir -Filter '*-read_file-*.txt' | Sort-Object Name
$byPath = @{}
foreach ($f in $files) {
  $head = Get-Content $f.FullName -TotalCount 3
  $inputLine = ($head | Select-String '^INPUT:').Line
  if ($inputLine -match '"path":"([^"]+)"') {
    $p = $Matches[1] -replace '\\\\', '\'
    if (-not $byPath.ContainsKey($p)) { $byPath[$p] = @() }
    $byPath[$p] += $f.FullName
  }
}

$manifest = @()
foreach ($p in ($byPath.Keys | Sort-Object)) {
  $lineMap = @{}
  $total = 0
  $sources = @()
  foreach ($export in $byPath[$p]) {
    $lines = Get-Content $export
    $idx = [Array]::IndexOf($lines, '---- RESULT ----')
    if ($idx -lt 0) { continue }
    $body = $lines[($idx+1)..($lines.Count-1)]
    $header = $body | Where-Object { $_ -match '^\[(OK|PARTIAL|FAIL)\]' } | Select-Object -First 1
    if ($header -match '(\d+)/(\d+)') { $total = [Math]::Max($total, [int]$Matches[2]) }
    elseif ($header -match '^\[PARTIAL\] (\d+) lines') { $total = [Math]::Max($total, [int]$Matches[1]) }
    $expected = -1
    foreach ($ln in $body) {
      if ($ln -match '^\s*(\d{1,6})  (.*)$') {
        $num = [int]$Matches[1]
        if ($expected -lt 0 -or $num -eq $expected) {
          $lineMap[$num] = $Matches[2]
          $expected = $num + 1
        } elseif ($num -gt $expected) {
          # new range block starting mid-file
          $lineMap[$num] = $Matches[2]
          $expected = $num + 1
        } else {
          if ($lineMap.ContainsKey($expected-1)) { $lineMap[$expected-1] += $ln }
        }
      } elseif ($ln -match '^(read_file:|\[OK\]|\[PARTIAL\]|\[FAIL\])') { continue }
      else {
        if ($expected -gt 0 -and $lineMap.ContainsKey($expected-1)) { $lineMap[$expected-1] += $ln }
      }
    }
    $sources += (Split-Path $export -Leaf)
  }
  if ($total -eq 0) { $total = ($lineMap.Keys | Measure-Object -Maximum).Maximum }
  $out = New-Object System.Collections.Generic.List[string]
  $gaps = @()
  $gapStart = 0
  for ($i = 1; $i -le $total; $i++) {
    if ($lineMap.ContainsKey($i)) {
      if ($gapStart -gt 0) { $gaps += "$gapStart-$($i-1)"; $out.Add("// __GAP__ lines $gapStart-$($i-1) not recovered"); $gapStart = 0 }
      $out.Add($lineMap[$i])
    } else {
      if ($gapStart -eq 0) { $gapStart = $i }
    }
  }
  if ($gapStart -gt 0) { $gaps += "$gapStart-$total"; $out.Add("// __GAP__ lines $gapStart-$total not recovered") }
  $rel = $p
  if ($p.StartsWith($srcPrefix)) { $rel = $p.Substring($srcPrefix.Length) }
  $dest = Join-Path $recon ($rel -replace '\\', '/')
  New-Item -ItemType Directory -Force -Path (Split-Path $dest -Parent) | Out-Null
  Set-Content -Path $dest -Value ($out -join "`n") -Encoding UTF8 -NoNewline
  $manifest += [pscustomobject]@{
    path = $rel; total_lines = $total; recovered = $lineMap.Count
    coverage_pct = [Math]::Round(100.0 * $lineMap.Count / [Math]::Max(1,$total), 1)
    gaps = ($gaps -join ','); sources = ($sources -join ',')
  }
}
$manifest | ConvertTo-Json | Set-Content (Join-Path $recon 'RECOVERY-MANIFEST.json') -Encoding UTF8
"recovered files: $($manifest.Count)"
$manifest | Sort-Object coverage_pct -Descending | Format-Table path, total_lines, recovered, coverage_pct, gaps -AutoSize
