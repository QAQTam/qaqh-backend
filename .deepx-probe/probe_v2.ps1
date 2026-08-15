# 实验 v2：打印原始响应体
$ErrorActionPreference = 'Stop'
$cfgText = Get-Content "$env:USERPROFILE\.deepx\config.toml" -Raw
function Get-Cfg($key) {
    if ($cfgText -match "(?m)^$key\s*=\s*`"([^`"]*)`"") { return $Matches[1] }
    return $null
}
$apiKey = Get-Cfg 'api_key'
$baseUrl = Get-Cfg 'base_url'
$model = Get-Cfg 'model'
$url = "$baseUrl/chat/completions"

function Send-Probe($name, $messages) {
    $body = @{ model = $model; messages = $messages; stream = $false; max_tokens = 64 } | ConvertTo-Json -Depth 10
    Write-Output "=== $name ==="
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $resp = Invoke-WebRequest -Uri $url -Method POST `
            -Headers @{ Authorization = "Bearer $apiKey" } `
            -ContentType 'application/json' -Body $body `
            -TimeoutSec 180 -SkipHttpErrorCheck
        $sw.Stop()
        Write-Output ("HTTP {0} | {1}ms" -f $resp.StatusCode, $sw.ElapsedMilliseconds)
        $text = $resp.Content
        if ($text.Length -gt 600) { $text = $text.Substring(0, 600) + '...[truncated]' }
        Write-Output ("BODY: {0}" -f ($text -replace "`n", ' '))
    } catch {
        $sw.Stop()
        Write-Output ("ERROR {0}ms: {1}" -f $sw.ElapsedMilliseconds, $_.Exception.Message)
    }
    Write-Output ''
}

# 1) tool_result 后紧跟 system（skills 当轮形状）
Send-Probe 'A: tool then system (tail)' @(
    @{ role = 'user'; content = '1+1=?' },
    @{ role = 'assistant'; content = $null; tool_calls = @(@{ id = 'call_1'; type = 'function'; function = @{ name = 'calc'; arguments = '{}' } }) },
    @{ role = 'tool'; tool_call_id = 'call_1'; content = '2' },
    @{ role = 'system'; content = "[SUBAGENT 'x' COMPLETED]`n`nok" }
)

# 2) tool 后 system 再 user（subagent turn 通道形状）
Send-Probe "A': tool then system then user" @(
    @{ role = 'user'; content = '1+1=?' },
    @{ role = 'assistant'; content = $null; tool_calls = @(@{ id = 'call_1'; type = 'function'; function = @{ name = 'calc'; arguments = '{}' } }) },
    @{ role = 'tool'; tool_call_id = 'call_1'; content = '2' },
    @{ role = 'system'; content = "[SUBAGENT 'x' COMPLETED]`n`nok" },
    @{ role = 'user'; content = '继续' }
)

# 3) 单独 system
Send-Probe 'B: system alone' @(
    @{ role = 'system'; content = "[SUBAGENT 'x' COMPLETED]`n`nok" }
)
