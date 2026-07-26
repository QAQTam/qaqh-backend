# status.ps1 — 查询当前产物状态

Write-Host "=== 产物状态 ==="

$items = @(
    @{Label="安装器 exe";     Path="target/release/DeepXInstaller.exe"},
    @{Label="单文件 SFX";     Path="dist/DeepXInstaller-Setup.exe"},
    @{Label="payload/ 目录";  Path="payload"}
)

foreach ($item in $items) {
    if (Test-Path $item.Path) {
        Write-Host "  ✓ $($item.Label)"
    } else {
        Write-Host "  ✗ $($item.Label)"
    }
}

if (Test-Path "payload") {
    Write-Host ""
    Write-Host "  payload/ 内容:"
    Get-ChildItem -Recurse -File payload | ForEach-Object {
        $rel = $_.FullName.Replace((Get-Location).Path + "\", "")
        Write-Host "    $rel"
    }
}
