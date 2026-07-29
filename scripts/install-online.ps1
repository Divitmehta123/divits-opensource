param(
    [ValidatePattern('^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')]
    [string]$Repository = 'Divitmehta123/divits-opensource',
    [ValidateSet('x64')]
    [string]$Architecture = 'x64'
)

$ErrorActionPreference = 'Stop'
$apiHeaders = @{ 'User-Agent' = 'Divits-OpenSource-Installer' }
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest" -Headers $apiHeaders
$assetName = "divits-opensource-windows-$Architecture.zip"
$asset = @($release.assets | Where-Object { $_.name -eq $assetName })[0]
$checksum = @($release.assets | Where-Object { $_.name -eq "$assetName.sha256" })[0]
if ($null -eq $asset -or $null -eq $checksum) {
    throw "The latest release for $Repository does not contain $assetName and its SHA-256 checksum."
}

$temporary = Join-Path ([System.IO.Path]::GetTempPath()) "divit-install-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
try {
    $archive = Join-Path $temporary $assetName
    $checksumPath = Join-Path $temporary "$assetName.sha256"
    Invoke-WebRequest -Uri $asset.browser_download_url -Headers $apiHeaders -OutFile $archive
    Invoke-WebRequest -Uri $checksum.browser_download_url -Headers $apiHeaders -OutFile $checksumPath
    $expected = ((Get-Content -LiteralPath $checksumPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw 'Release checksum verification failed. The archive was not installed.'
    }
    $expanded = Join-Path $temporary 'release'
    Expand-Archive -LiteralPath $archive -DestinationPath $expanded -Force
    & (Join-Path $expanded 'install-binary.ps1') -SourceDirectory $expanded
} finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}
