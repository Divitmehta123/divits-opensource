$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$packagePath = Join-Path $projectRoot 'app\opensrc-cli'
$cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue

if ($cargoCommand) {
    $cargoPath = $cargoCommand.Source
} else {
    $cargoPath = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path -LiteralPath $cargoPath)) {
        throw 'Rust is required. Install it from https://rustup.rs, then run this script again.'
    }
}

& $cargoPath install --path $packagePath --locked --force
if ($LASTEXITCODE -ne 0) {
    throw "Installation failed with exit code $LASTEXITCODE."
}

$cargoBin = Split-Path -Parent $cargoPath
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$userPathEntries = @($userPath -split ';' | Where-Object { $_ })
if ($cargoBin -notin $userPathEntries) {
    $updatedUserPath = (@($userPathEntries) + $cargoBin) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updatedUserPath, 'User')
}
if ($cargoBin -notin ($env:Path -split ';')) {
    $env:Path = "$env:Path;$cargoBin"
}

$completionDirectory = Join-Path $env:LOCALAPPDATA 'divits-opensource\completions'
New-Item -ItemType Directory -Path $completionDirectory -Force | Out-Null
$installedExecutable = Join-Path $cargoBin 'divit.exe'
if (-not (Test-Path -LiteralPath $installedExecutable)) {
    $installedExecutable = Join-Path $cargoBin 'divit'
}
foreach ($shell in @('powershell', 'bash', 'zsh', 'fish', 'elvish')) {
    $extension = if ($shell -eq 'powershell') { 'ps1' } else { $shell }
    $output = Join-Path $completionDirectory "divit.$extension"
    & $installedExecutable completions $shell | Set-Content -LiteralPath $output -Encoding utf8
    if ($LASTEXITCODE -ne 0) {
        throw "Completion generation for $shell failed with exit code $LASTEXITCODE."
    }
}

$legacyCommand = Join-Path $cargoBin 'opensource.cmd'
$longCommand = Join-Path $cargoBin 'divit-opensource.cmd'
$brandCommand = Join-Path $cargoBin 'divits-opensource.cmd'
$wrapper = "@echo off`r`n`"%~dp0divit.exe`" %*`r`n"
Set-Content -LiteralPath $legacyCommand -Value $wrapper -Encoding ascii
Set-Content -LiteralPath $longCommand -Value $wrapper -Encoding ascii
Set-Content -LiteralPath $brandCommand -Value $wrapper -Encoding ascii

Write-Host ''
Write-Host "Divit's OpenSource Tool is installed. Open any project directory and run:"
Write-Host '  divit'
Write-Host ''
Write-Host "Shell completions were generated in $completionDirectory"
