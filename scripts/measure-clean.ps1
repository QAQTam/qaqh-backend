# Regenerate physical source-size and direct workspace dependency measurements.
param(
    [string]$OutputPath = 'docs/metrics/clean-baseline.json',
    [string]$MeasuredDate = (Get-Date -Format 'yyyy-MM-dd')
)

$ErrorActionPreference = 'Stop'
$taskRoot = (Get-Location).Path
if (-not (Test-Path -LiteralPath (Join-Path $taskRoot 'Cargo.toml'))) {
    throw 'Run this script from the backend workspace root.'
}
$taskMetadataText = & cargo metadata --no-deps --locked --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$taskMetadata = ($taskMetadataText -join "`n") | ConvertFrom-Json
$taskNames = @($taskMetadata.packages.name)
$taskRows = foreach ($taskPackage in $taskMetadata.packages) {
    $taskCrateDir = Split-Path $taskPackage.manifest_path
    $taskFiles = @(Get-ChildItem -LiteralPath (Join-Path $taskCrateDir 'src') -Recurse -Filter '*.rs' -ErrorAction SilentlyContinue)
    $taskCounts = @($taskFiles | ForEach-Object {
        [pscustomobject]@{
            path = [System.IO.Path]::GetRelativePath($taskRoot, $_.FullName).Replace('\', '/')
            lines = [System.IO.File]::ReadAllLines($_.FullName).Length
        }
    })
    [pscustomobject]@{
        name = $taskPackage.name
        version = $taskPackage.version
        src_files = $taskFiles.Count
        src_lines = [int](($taskCounts | Measure-Object lines -Sum).Sum)
        largest_files = @($taskCounts | Sort-Object lines -Descending | Select-Object -First 3)
        normal_workspace_dependencies = @($taskPackage.dependencies |
            Where-Object { $_.name -in $taskNames -and $null -eq $_.kind } |
            ForEach-Object { $_.name } | Sort-Object -Unique)
        default_features = @($taskPackage.features.default)
    }
}
$taskCommit = & git rev-parse HEAD
if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve baseline commit.' }
$taskBaseline = [ordered]@{
    schema_version = 1
    measured_date = $MeasuredDate
    method = 'Cargo metadata --no-deps; physical lines of crates/*/src/**/*.rs, including inline tests, comments and blank lines; excludes tests/ and build.rs. Local normal dependency edges include target-specific edges, exclude dev/build edges; transitive dependency weight is not measured.'
    baseline_commit = $taskCommit.Trim()
    crates = @($taskRows | Sort-Object src_lines -Descending)
}
$taskOutput = [System.IO.Path]::GetFullPath($OutputPath, $taskRoot)
[System.IO.Directory]::CreateDirectory((Split-Path $taskOutput)) | Out-Null
[System.IO.File]::WriteAllText($taskOutput, ($taskBaseline | ConvertTo-Json -Depth 8) + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
Write-Output "Measured $($taskRows.Count) crates -> $taskOutput"
