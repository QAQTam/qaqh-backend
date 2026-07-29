param(
    [string]$LogFile = (Join-Path $env:USERPROFILE '.deepx\agent.log'),
    [int]$TailLines = 0,
    [switch]$Follow,
    [string]$Filter
)

$colorMap = @{
    'DEBUG' = 'DarkGray'
    'INFO'  = 'White'
    'WARN'  = 'Yellow'
    'ERROR' = 'Red'
}

$levelOrder = @('DEBUG','INFO','WARN','ERROR')

if (-not (Test-Path $LogFile)) {
    Write-Host "Log file not found: $LogFile" -ForegroundColor Red
    exit 1
}

$fileInfo = Get-Item $LogFile
$lastPos = $fileInfo.Length

if ($TailLines -gt 0) {
    Get-Content $LogFile -Tail $TailLines
    Write-Host ""
    Write-Host "--- following (Ctrl+C to stop) ---" -ForegroundColor DarkGray
    Start-Sleep -Milliseconds 300
}

$running = $true
trap { $running = $false }

try {
    while ($running) {
        $currentSize = (Get-Item $LogFile).Length
        if ($currentSize -lt $lastPos) {
            $lastPos = 0
        }
        $stream = [System.IO.File]::Open($LogFile, 'Open', 'Read', 'ReadWrite')
        try {
            $stream.Position = $lastPos
            $reader = New-Object System.IO.StreamReader($stream)
            while (-not $reader.EndOfStream) {
                $line = $reader.ReadLine()
                if ($null -ne $line -and $line.Length -gt 0) {
                    $level = 'INFO'
                    foreach ($lvl in $levelOrder) {
                        if ($line -match "\[$lvl\]") { $level = $lvl; break }
                    }
                    $color = $colorMap[$level]
                    if (-not $color) { $color = 'White' }
                    if ($line -match '^\[(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\]') {
                        Write-Host "[$($Matches[1])]" -NoNewline -ForegroundColor DarkGray
                        $rest = $line.Substring($Matches[0].Length)
                    } else {
                        $rest = $line
                    }
                    switch -Wildcard ($rest) {
                        '*dispatch_one*'     { Write-Host $rest -ForegroundColor Cyan }
                        '*received Ui2Agent*' { Write-Host $rest -ForegroundColor Green }
                        '*[INPUT]*'           { Write-Host $rest -ForegroundColor Yellow }
                        '*[TURN]*'            { Write-Host $rest -ForegroundColor Magenta }
                        '*[ERROR]*'           { Write-Host $rest -ForegroundColor Red }
                        default                { Write-Host $rest -ForegroundColor $color }
                    }
                }
            }
            $lastPos = $stream.Position
        } finally {
            $stream.Close()
        }
        if (-not $Follow) { break }
        Start-Sleep -Milliseconds 250
    }
} catch {
    Write-Host "Error: $_" -ForegroundColor Red
}

Write-Host ""
Write-Host "--- stopped ---" -ForegroundColor DarkGray
