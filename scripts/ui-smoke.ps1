param([string]$Exe = (Join-Path $PSScriptRoot '..\target\debug\WorkLink.exe'), [switch]$Visible)
$ErrorActionPreference = 'Stop'
$taskExe = (Resolve-Path -LiteralPath $Exe).Path
$taskRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskData = Join-Path $taskRoot ('test-artifacts\ui-' + [guid]::NewGuid().ToString('N'))
$taskOutput = Join-Path $taskRoot 'docs\worklink-ui.png'
$taskPrevious = $env:WORKLINK_SCREENSHOT
try {
    & $taskExe --data-dir $taskData init --name 'My Windows PC' --outbound-only | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'UI fixture initialization failed' }
    $env:WORKLINK_SCREENSHOT = $taskOutput
    $taskStyle = if ($Visible) { 'Normal' } else { 'Hidden' }
    $taskError = Join-Path $taskData 'ui-stderr.txt'
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList @('--data-dir', ('"' + $taskData + '"'), 'gui') -PassThru -WindowStyle $taskStyle -RedirectStandardError $taskError
    $taskProcessHandle = $taskProcess.Handle
    if (-not $taskProcess.WaitForExit(25000)) { throw ('GUI screenshot timed out; title=' + $taskProcess.MainWindowTitle + '; ' + [IO.File]::ReadAllText($taskError)) }
    if ($taskProcess.ExitCode -ne 0) { throw ('GUI exited: ' + $taskProcess.ExitCode) }
    if (-not (Test-Path -LiteralPath $taskOutput)) { throw 'Screenshot was not written' }
    Write-Output ('GUI screenshot: ' + $taskOutput)
} finally {
    $env:WORKLINK_SCREENSHOT = $taskPrevious
    Get-CimInstance Win32_Process -Filter "Name='WorkLink.exe'" | Where-Object { $_.ExecutablePath -eq $taskExe -and $_.CommandLine -like ('*' + $taskData + '*') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}
