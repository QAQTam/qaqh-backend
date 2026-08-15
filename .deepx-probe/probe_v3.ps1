# 实验 v3：assistant 回传 reasoning_content（对齐引擎真实形状）
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
    $body = @{ model = $model; messages = $messages; stream = $false; max_tokens = 120 } | ConvertTo-Json -Depth 10
    Write-Output "=== $name ==="
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $resp = Invoke-WebRequest -Uri $url -Method POST `
            -Headers @{ Authorization = "Bearer $apiKey" } `
            -ContentType 'application/json' -Body $body `
            -TimeoutSec 180 -SkipHttpErrorCheck
        $sw.Stop()
        Write-Output ("HTTP {0} | {1}ms" -f $resp.StatusCode, $sw.ElapsedMilliseconds)
        $j = $resp.Content | ConvertFrom-Json
        $c = $j.choices[0].message.content
        $r = $j.choices[0].message.reasoning_content
        Write-Output ("content: [{0}]" -f ($c -replace "`n", ' '))
        if ($r) { Write-Output ("reasoning_head: [{0}]" -f (($r -replace "`n", ' ').Substring(0, [Math]::Min(150, $r.Length)))) }
    } catch {
        $sw.Stop()
        Write-Output ("ERROR {0}ms: {1}" -f $sw.ElapsedMilliseconds, $_.Exception.Message)
    }
    Write-Output ''
}

$asstTool = @{ role = 'assistant'; content = $null; reasoning_content = 'Let me calculate 1+1.'; tool_calls = @(@{ id = 'call_1'; type = 'function'; function = @{ name = 'calc'; arguments = '{}' } }) }

# 1) tool 后紧跟 system（带 reasoning_content）
Send-Probe 'A: tool then system (tail)' @(
    @{ role = 'user'; content = '1+1=?' },
    $asstTool,
    @{ role = 'tool'; tool_call_id = 'call_1'; content = '2' },
    @{ role = 'system'; content = "[SUBAGENT 'x' COMPLETED]`n`nok" }
)

# 2) tool 后 system 再 user
Send-Probe "A': tool then system then user" @(
    @{ role = 'user'; content = '1+1=?' },
    $asstTool,
    @{ role = 'tool'; tool_call_id = 'call_1'; content = '2' },
    @{ role = 'system'; content = "[SUBAGENT 'x' COMPLETED]`n`nok" },
    @{ role = 'user'; content = '继续' }
)

# 3) tool 后 user（无 system 的对照）
Send-Probe 'D: tool then user (control)' @(
    @{ role = 'user'; content = '1+1=?' },
    $asstTool,
    @{ role = 'tool'; tool_call_id = 'call_1'; content = '2' },
    @{ role = 'user'; content = '继续' }
)
