param(
    [string]$SourceDirectory = $PSScriptRoot
)

$ErrorActionPreference = 'Stop'

$source = Join-Path $SourceDirectory 'divit.exe'
if (-not (Test-Path -LiteralPath $source)) {
    throw "divit.exe was not found in $SourceDirectory. Extract the complete release archive first."
}

$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\DivitsOpenSource\bin'
New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
Copy-Item -LiteralPath $source -Destination (Join-Path $installDirectory 'divit.exe') -Force

$wrapper = "@echo off`r`n`"%~dp0divit.exe`" %*`r`n"
Set-Content -LiteralPath (Join-Path $installDirectory 'divit-opensource.cmd') -Value $wrapper -Encoding ascii
Set-Content -LiteralPath (Join-Path $installDirectory 'divits-opensource.cmd') -Value $wrapper -Encoding ascii
Set-Content -LiteralPath (Join-Path $installDirectory 'opensource.cmd') -Value $wrapper -Encoding ascii

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';' | Where-Object { $_ })
if ($installDirectory -notin $entries) {
    [Environment]::SetEnvironmentVariable(
        'Path',
        (@($entries) + $installDirectory) -join ';',
        'User'
    )
}

Write-Host ''
Write-Host "Divit's OpenSource Tool was installed."
Write-Host 'Open a new Command Prompt in any folder and run:'
Write-Host '  divit'
