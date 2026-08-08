# Downstream 本地开发切换（windows-rs fork）
#
# DeepX 正式依赖 QAQTam/deepx-winui-rs 的 immutable snapshot commit（git rev）。
# 开发下游代码时，把三个 manifest 临时切到本地仓库 F:\DeepX-windows-rs，
# 验证完再切回 git rev。
#
# 用法（PowerShell 7）：
#   pwsh scripts/dev-downstream.ps1 on       # 切到本地 path 依赖
#   pwsh scripts/dev-downstream.ps1 off      # 恢复 git rev 依赖
#   pwsh scripts/dev-downstream.ps1 status   # 查看当前指向 + 本地仓库状态
#
# 注意：切换后 Cargo.lock 会随下次 cargo 调用自动更新；提交前务必 off。

param(
    [Parameter(Position = 0)]
    [ValidateSet("on", "off", "status")]
    [string]$Action = "status"
)

$ErrorActionPreference = "Stop"

# ---- 配置（改 rev 时同步更新这里）------------------------------------------
$Rev      = "608258e1e62cd84c2fed4a7bf39f4d805c6a0785"
$GitUrl   = "https://github.com/QAQTam/deepx-winui-rs.git"
$Local    = "F:/DeepX-windows-rs"                      # downstream 工作树
# -----------------------------------------------------------------------------

$root = Split-Path -Parent $PSScriptRoot
$manifests = @(
    (Join-Path $root "apps/winui/Cargo.toml"),
    (Join-Path $root "crates/deepx-fluent/Cargo.toml"),
    (Join-Path $root "crates/markdown-winui/Cargo.toml")
)

function Git-Line([string]$crate) { "git = `"$GitUrl`", rev = `"$Rev`"" }
function Path-Line([string]$crate) { "path = `"$Local/crates/libs/$crate`"" }

$crateFor = @{
    "windows-reactor"        = "reactor"
    "windows-reactor-setup"  = "reactor-setup"
    "windows"                = "windows"
}

function Test-PointingLocal([string]$text) {
    $text -match "windows-reactor\s*=\s*\{[^}]*path\s*=\s*`"$([regex]::Escape($Local))"
}

function Switch-ToLocal {
    foreach ($m in $manifests) {
        $lines = Get-Content $m
        $out = foreach ($l in $lines) {
            $hit = $false
            foreach ($k in $crateFor.Keys) {
                if ($l -match "^\s*$([regex]::Escape($k))\s*=\s*\{.*git\s*=\s*`"$([regex]::Escape($GitUrl))`"") {
                    $out_line = $l -replace '\{\s*git\s*=\s*"[^"]*",\s*rev\s*=\s*"[^"]*"', "{ $(Path-Line $crateFor[$k])"
                    $out_line
                    $hit = $true
                    break
                }
            }
            if (-not $hit) { $l }
        }
        Set-Content $m $out
    }
    Write-Host "已切换到本地 path 依赖：$Local（记得跑 cargo check 验证）"
}

function Switch-ToRev {
    foreach ($m in $manifests) {
        $lines = Get-Content $m
        $out = foreach ($l in $lines) {
            $hit = $false
            foreach ($k in $crateFor.Keys) {
                if ($l -match "^\s*$([regex]::Escape($k))\s*=\s*\{.*path\s*=\s*`"$([regex]::Escape($Local))") {
                    $out_line = $l -replace '\{\s*path\s*=\s*"[^"]*"', "{ $(Git-Line $crateFor[$k])"
                    $out_line
                    $hit = $true
                    break
                }
            }
            if (-not $hit) { $l }
        }
        Set-Content $m $out
    }
    Write-Host "已恢复 git rev 依赖：$GitUrl @ $Rev"
}

function Show-Status {
    $anyLocal = $false
    foreach ($m in $manifests) {
        $text = Get-Content $m -Raw
        $state = if (Test-PointingLocal $text) { "LOCAL (path)" } else { "git rev" }
        if ($state -eq "LOCAL (path)") { $anyLocal = $true }
        Write-Host ("{0,-45} {1}" -f $m.Replace($root, "."), $state)
    }
    Write-Host ""
    Write-Host "本地 downstream: $Local"
    if (Test-Path $Local) {
        git -C $Local log --oneline -1 2>&1
        git -C $Local branch --list "deepx/snapshot-*" 2>&1 | ForEach-Object { "  $_" }
    } else {
        Write-Host "  (不存在！)"
    }
    Write-Host ""
    if ($anyLocal) {
        Write-Host "⚠ 当前指向本地 —— 提交代码前先运行: pwsh scripts/dev-downstream.ps1 off"
    } else {
        Write-Host "当前指向 git rev $Rev（正式形态，可提交）"
    }
}

switch ($Action) {
    "on"     { Switch-ToLocal }
    "off"    { Switch-ToRev }
    "status" { Show-Status }
}
