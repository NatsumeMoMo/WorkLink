param([switch]$SkipTests)
$ErrorActionPreference = 'Stop'
$taskRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location -LiteralPath $taskRoot
try {
    & cargo fmt --check
    if ($LASTEXITCODE -ne 0) { throw 'Formatting failed' }
    & cargo clippy --all-targets --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Clippy failed' }
    if (-not $SkipTests) {
        & cargo test --locked -- --nocapture
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
    }
    & cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    $taskDist = Join-Path $taskRoot 'dist'
    New-Item -ItemType Directory -Path $taskDist -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $taskRoot 'target\release\WorkLink.exe') -Destination $taskDist -Force
    Copy-Item -LiteralPath (Join-Path $taskRoot 'README.md') -Destination $taskDist -Force
    Copy-Item -LiteralPath (Join-Path $taskRoot 'LICENSE') -Destination $taskDist -Force
    Copy-Item -LiteralPath (Join-Path $taskRoot 'docs') -Destination $taskDist -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $taskRoot 'examples') -Destination $taskDist -Recurse -Force
    $taskHash = Get-FileHash -LiteralPath (Join-Path $taskDist 'WorkLink.exe') -Algorithm SHA256
    ($taskHash.Hash.ToLowerInvariant() + '  WorkLink.exe') | Set-Content -LiteralPath (Join-Path $taskDist 'SHA256SUMS.txt') -Encoding ASCII
    $taskReleases = Join-Path $taskRoot 'releases'
    New-Item -ItemType Directory -Path $taskReleases -Force | Out-Null
    $taskVersion = [regex]::Match([IO.File]::ReadAllText((Join-Path $taskRoot 'Cargo.toml')), '(?m)^version = "([^"]+)"').Groups[1].Value
    $taskZip = Join-Path $taskReleases ('WorkLink-v' + $taskVersion + '-win11-x64.zip')
    Compress-Archive -Path (Join-Path $taskDist '*') -DestinationPath $taskZip -Force
    Write-Output ('Release ready: ' + $taskDist)
    Write-Output ('Release archive: ' + $taskZip)
} finally { Pop-Location }
