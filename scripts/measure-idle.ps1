param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\WorkLink.exe'),
    [ValidateRange(5, 60)][int]$Seconds = 30
)
$ErrorActionPreference = 'Stop'
$taskExe = (Resolve-Path -LiteralPath $Exe).Path
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskData = Join-Path $taskRoot ('test-artifacts\idle-' + [guid]::NewGuid().ToString('N'))
$taskA = Join-Path $taskData 'A'
$taskB = Join-Path $taskData 'B'
$taskChildren = @()
function Invoke-WorkLink([string]$DataDir, [string[]]$Arguments) {
    $taskText = & $taskExe --data-dir $DataDir --json @Arguments | Out-String
    if ($LASTEXITCODE -ne 0) { throw ($taskText -join "`n") }
    return ($taskText | ConvertFrom-Json)
}
try {
    $taskListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $taskListener.Start()
    $taskPort = $taskListener.LocalEndpoint.Port
    $taskListener.Stop()
    $null = Invoke-WorkLink $taskA @('init', '--name', 'Measure A', '--listen', "127.0.0.1:$taskPort")
    $null = Invoke-WorkLink $taskB @('init', '--name', 'Measure B', '--outbound-only')
    $taskInviteA = Invoke-WorkLink $taskA @('invite')
    $taskInviteB = Invoke-WorkLink $taskB @('invite')
    $null = Invoke-WorkLink $taskA @('peer', 'add', '--invite', $taskInviteB.invite, '--fingerprint', $taskInviteB.fingerprint)
    $null = Invoke-WorkLink $taskB @('peer', 'add', '--invite', $taskInviteA.invite, '--fingerprint', $taskInviteA.fingerprint, '--endpoint', "127.0.0.1:$taskPort")
    foreach ($taskSpec in @(@('A daemon', $taskA, 'daemon'), @('B daemon', $taskB, 'daemon'), @('A tray', $taskA, 'tray'))) {
        $taskProcess = Start-Process -FilePath $taskExe -ArgumentList @('--data-dir', ('"' + $taskSpec[1] + '"'), $taskSpec[2]) -PassThru -WindowStyle Hidden
        $taskChildren += [pscustomobject]@{ Name = $taskSpec[0]; Process = $taskProcess }
    }
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 500
        $taskOnline = Invoke-WorkLink $taskB @('status')
        if ($taskOnline.peers[0].online) { break }
    } while ([DateTime]::UtcNow -lt $taskDeadline)
    if (-not $taskOnline.peers[0].online) { throw 'Measurement peers did not connect' }
    Start-Sleep -Seconds 2
    $taskBaselines = @{}
    foreach ($taskChild in $taskChildren) {
        $taskChild.Process.Refresh()
        $taskBaselines[$taskChild.Name] = $taskChild.Process.TotalProcessorTime.TotalSeconds
    }
    $taskWatch = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $Seconds
    $taskWatch.Stop()
    $taskResults = foreach ($taskChild in $taskChildren) {
        $taskChild.Process.Refresh()
        if ($taskChild.Process.HasExited) { throw ($taskChild.Name + ' exited during measurement') }
        $taskDelta = $taskChild.Process.TotalProcessorTime.TotalSeconds - $taskBaselines[$taskChild.Name]
        [pscustomobject]@{
            role = $taskChild.Name
            working_set_mib = [math]::Round($taskChild.Process.WorkingSet64 / 1MB, 2)
            private_mib = [math]::Round($taskChild.Process.PrivateMemorySize64 / 1MB, 2)
            cpu_seconds = [math]::Round($taskDelta, 4)
            cpu_percent_one_core = [math]::Round(100 * $taskDelta / $taskWatch.Elapsed.TotalSeconds, 3)
            threads = $taskChild.Process.Threads.Count
            handles = $taskChild.Process.HandleCount
        }
    }
    $taskReport = [pscustomobject]@{
        binary = $taskExe
        binary_bytes = (Get-Item -LiteralPath $taskExe).Length
        duration_seconds = [math]::Round($taskWatch.Elapsed.TotalSeconds, 3)
        connected = $true
        processes = @($taskResults)
    }
    $taskJson = $taskReport | ConvertTo-Json -Depth 5
    $taskJson | Set-Content -LiteralPath (Join-Path $taskRoot 'docs\idle-measurement.json') -Encoding UTF8
    Write-Output $taskJson
} finally {
    foreach ($taskChild in $taskChildren) {
        if (-not $taskChild.Process.HasExited) { Stop-Process -Id $taskChild.Process.Id -Force -ErrorAction SilentlyContinue }
        $taskChild.Process.Dispose()
    }
}
