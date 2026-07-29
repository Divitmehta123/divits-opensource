param(
    [int]$Port = 4546
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root 'app'
$exe = Join-Path $app 'target\debug\divit.exe'
$database = Join-Path $root 'tests\smoke-state.sqlite3'
$stdout = Join-Path $root 'tests\smoke-server.stdout.log'
$stderr = Join-Path $root 'tests\smoke-server.stderr.log'
$server = "http://127.0.0.1:$Port"
$process = $null

try {
    $process = Start-Process -FilePath $exe `
        -ArgumentList @('serve', '--bind', "127.0.0.1:$Port", '--database', "`"$database`"") `
        -WorkingDirectory $app `
        -WindowStyle Hidden `
        -RedirectStandardOutput $stdout `
        -RedirectStandardError $stderr `
        -PassThru

    $healthy = $false
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        try {
            $null = Invoke-RestMethod -Uri "$server/v1/health"
            $healthy = $true
            break
        } catch {
            Start-Sleep -Milliseconds 200
        }
    }
    if (-not $healthy) {
        throw "Server did not become healthy. See $stderr"
    }

    $conversation = Invoke-RestMethod -Method Post `
        -Uri "$server/v1/conversations" `
        -ContentType 'application/json' `
        -Body (@{ project_root = $root; title = 'Runtime smoke' } | ConvertTo-Json)
    $run = Invoke-RestMethod -Method Post `
        -Uri "$server/v1/runs" `
        -ContentType 'application/json' `
        -Body (@{
            conversation_id = $conversation.id
            request = 'Validate the provider-independent runtime lifecycle'
            mode = 'focused'
        } | ConvertTo-Json)
    $tools = Invoke-RestMethod -Uri "$server/v1/tools"
    $skills = Invoke-RestMethod -Uri "$server/v1/skills"
    $activatedSkill = Invoke-RestMethod -Method Post `
        -Uri "$server/v1/skills/focused-validation/activate"
    $metrics = Invoke-RestMethod -Uri "$server/v1/metrics?run_id=$($run.id)"
    if (
        $tools.tools.Count -lt 6 -or
        $skills.skills.Count -lt 3 -or
        [string]::IsNullOrWhiteSpace($activatedSkill.instructions) -or
        $metrics.agents -ne 0
    ) {
        throw 'Tools, lazy Skills, or metric projections failed smoke validation.'
    }

    $events = Invoke-RestMethod -Uri "$server/v1/events?after=0&limit=200"
    [pscustomobject]@{
        Health = 'ok'
        Conversation = $conversation.id
        Run = $run.id
        Mode = $run.mode
        Tools = $tools.tools.Count
        Skills = $skills.skills.Count
        MetricAgents = $metrics.agents
        EventCount = $events.events.Count
    } | ConvertTo-Json
} finally {
    if ($null -ne $process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id
        Wait-Process -Id $process.Id -ErrorAction SilentlyContinue
    }
    foreach ($path in @($database, "$database-wal", "$database-shm")) {
        for ($attempt = 0; $attempt -lt 20 -and (Test-Path -LiteralPath $path); $attempt++) {
            try {
                Remove-Item -LiteralPath $path -Force -ErrorAction Stop
            } catch {
                if ($attempt -eq 19) {
                    throw
                }
                Start-Sleep -Milliseconds 100
            }
        }
    }
}
