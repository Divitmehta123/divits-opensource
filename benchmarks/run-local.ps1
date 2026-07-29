param(
    [int]$Iterations = 1000,
    [string]$Output = 'F:\Project OpenSource\benchmarks\results\local-classifier.json'
)

$ErrorActionPreference = 'Stop'
$app = 'F:\Project OpenSource\app'
& 'C:\Users\HP\.cargo\bin\cargo.exe' run `
    --manifest-path (Join-Path $app 'Cargo.toml') `
    -q `
    -p opensrc-cli `
    -- benchmark-local `
    --scenarios 'F:\Project OpenSource\benchmarks\scenarios.json' `
    --iterations $Iterations `
    --output $Output
if ($LASTEXITCODE -ne 0) {
    throw "Local benchmark failed with exit code $LASTEXITCODE"
}
