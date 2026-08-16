# qaqh-pack 端到端冒烟：基线发布 → 增量打包 → catalog 更新
$ErrorActionPreference = "Stop"
$t = Join-Path $env:TEMP "qaqh-pack-smoke"
Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path "$t/base/files", "$t/target/files", "$t/out" | Out-Null

Set-Content -Path "$t/base/files/a.txt" -Value "same" -NoNewline
Set-Content -Path "$t/base/files/b.txt" -Value "old" -NoNewline
Set-Content -Path "$t/base/files/gone.txt" -Value "del" -NoNewline
Set-Content -Path "$t/target/files/a.txt" -Value "same" -NoNewline
Set-Content -Path "$t/target/files/b.txt" -Value "new" -NoNewline
Set-Content -Path "$t/target/files/c.txt" -Value "added" -NoNewline

function Sha($p) { (Get-FileHash $p -Algorithm SHA256).Hash.ToLowerInvariant() }

$baseManifest = [ordered]@{
    formatVersion = 1; kind = "runtime"; buildId = "runtime-1.0.0"; appVersion = "1.0.0"
    releaseId = "rel-1.0.0"; channel = "local"
    components = [ordered]@{ runtime = [ordered]@{ buildId = "runtime-1.0.0"; version = "1.0.0"; controlProtocol = 1 } }
    requiresFullInstall = $false
    files = @(
        [ordered]@{ source = "files/a.txt"; target = "a.txt"; size = 4; sha256 = Sha "$t/base/files/a.txt" },
        [ordered]@{ source = "files/b.txt"; target = "b.txt"; size = 3; sha256 = Sha "$t/base/files/b.txt" },
        [ordered]@{ source = "files/gone.txt"; target = "gone.txt"; size = 3; sha256 = Sha "$t/base/files/gone.txt" }
    )
}
$baseManifest | ConvertTo-Json -Depth 6 | Set-Content "$t/base/bundle.json" -Encoding utf8

$targetManifest = [ordered]@{
    formatVersion = 1; kind = "runtime"; buildId = "runtime-1.0.1"; appVersion = "1.0.1"
    releaseId = "rel-1.0.1"; channel = "local"
    components = [ordered]@{ runtime = [ordered]@{ buildId = "runtime-1.0.1"; version = "1.0.1"; controlProtocol = 1 } }
    requiresFullInstall = $true
    files = @(
        [ordered]@{ source = "files/a.txt"; target = "a.txt"; size = 4; sha256 = Sha "$t/target/files/a.txt" },
        [ordered]@{ source = "files/b.txt"; target = "b.txt"; size = 3; sha256 = Sha "$t/target/files/b.txt" },
        [ordered]@{ source = "files/c.txt"; target = "c.txt"; size = 5; sha256 = Sha "$t/target/files/c.txt" }
    )
}
$targetManifest | ConvertTo-Json -Depth 6 | Set-Content "$t/target/bundle.json" -Encoding utf8

$catalog = [ordered]@{
    formatVersion = 1; releaseId = "local-1"; channel = "local"; publishedAt = "2026-08-10T00:00:00Z"
    components = [ordered]@{ runtime = [ordered]@{ buildId = "runtime-1.0.0"; version = "1.0.0"; controlProtocol = 1 } }
    artifacts = @(
        [ordered]@{
            id = "full-1.0.0"; kind = "full"; strategy = "component-full"
            targets = [ordered]@{ runtime = "runtime-1.0.0" }
            requires = [ordered]@{}; restartPolicy = "full"
            payload = [ordered]@{ path = "bundles/full-1.0.0.zip"; size = 1; sha256 = "a" * 64 }
        }
    )
}
$catalog | ConvertTo-Json -Depth 6 | Set-Content "$t/catalog.json" -Encoding utf8

$pack = Join-Path (Resolve-Path (Join-Path $PSScriptRoot "..")).Path "target\debug\qaqh-pack.exe"
Write-Host "=== baseline-publish ==="
& $pack baseline-publish --bundle "$t/base" --version 1.0.0 --out "$t"

Write-Host "=== delta ==="
& $pack delta --baseline "$t/baselines/1.0.0/manifest.json" --target "$t/target" `
    --baseline-version 1.0.0 --out "$t/out" --zip "$t/out/runtime-1.0.1.zip" `
    --catalog "$t/catalog.json" --restart-policy electron

Write-Host "=== out/files（应只有 b.txt + c.txt）==="
Get-ChildItem "$t/out/files" | Select-Object -ExpandProperty Name

Write-Host "=== zip 内容 ==="
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::OpenRead("$t/out/runtime-1.0.1.zip").Entries | ForEach-Object { $_.FullName }

Write-Host "=== catalog artifact ==="
$c = Get-Content "$t/catalog.json" -Raw | ConvertFrom-Json
$c.artifacts | ConvertTo-Json -Depth 5
Write-Host "SMOKE OK"
