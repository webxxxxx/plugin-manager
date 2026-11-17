[CmdletBinding()]
param(
    [string]$Iso = 'D:\learn\pve\i3-n305\proxmox-ve_9.2-1.iso',
    [string]$Repo = '',
    [string]$Binary = '',
    [string]$Spk = '',
    [string]$WorkDir = '',
    [int64]$PartSize = 536870912,
    [int]$Concurrency = 5,
    [int]$MonitorMs = 250,
    [string]$Curl = 'C:\Windows\System32\curl.exe',
    [string]$Pwsh = 'C:\Program Files\PowerShell\7\pwsh.exe'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# tools/perf/blob-bench -> 仓库根（向上三级）
if (-not $Repo) { $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path }
if (-not $Binary) { $Binary = Join-Path $Repo 'target\release\svcd.exe' }
if (-not $Spk) { $Spk = Join-Path $PSScriptRoot 'filebench.spk' }
if (-not $WorkDir) { $WorkDir = Join-Path $env:TEMP 'svcd-blob-bench\run-file' }

function Write-Step([string]$Text) {
    Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $Text)
}

function Start-ManagedProcess {
    param(
        [string]$File,
        [string[]]$Arguments,
        [string]$OutFile,
        [string]$ErrFile
    )
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $File
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    [void]$process.Start()
    return $process
}

function Get-ProcessIo([int]$ProcessId) {
    try {
        $process = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction Stop
        return [pscustomobject]@{
            Read  = if ($null -eq $process.ReadTransferCount) { [int64]0 } else { [int64]$process.ReadTransferCount }
            Write = if ($null -eq $process.WriteTransferCount) { [int64]0 } else { [int64]$process.WriteTransferCount }
        }
    }
    catch {
        return [pscustomobject]@{ Read = [int64]0; Write = [int64]0 }
    }
}

function Wait-Http([string]$ConfigDir, [int]$TimeoutSec = 30) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $portFile = Join-Path $ConfigDir 'http.port'
        if (Test-Path -LiteralPath $portFile) {
            $address = (Get-Content -LiteralPath $portFile -Raw).Trim()
            if ($address -match ':(\d+)$') {
                $port = [int]$Matches[1]
                $client = [Net.Sockets.TcpClient]::new()
                try {
                    $client.Connect('127.0.0.1', $port)
                    $client.Close()
                    return $port
                }
                catch {
                    $client.Dispose()
                }
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "daemon did not expose an HTTP port within ${TimeoutSec}s"
}

function Start-Monitor {
    param([int[]]$Pids, [string]$Dir)
    $stopFile = Join-Path $Dir 'monitor.stop'
    $csv = Join-Path $Dir 'monitor.csv'
    Remove-Item -LiteralPath $stopFile -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $csv -ErrorAction SilentlyContinue
    $pidList = ($Pids | ForEach-Object { [string]$_ }) -join ','
    $job = Start-Job -ScriptBlock {
        param($PidList, $StopFile, $Csv, $MonitorMs)
        $ids = @($PidList.Split(',') | ForEach-Object { [int]$_ })
        $rows = [System.Collections.Generic.List[object]]::new()
        while (-not (Test-Path -LiteralPath $StopFile)) {
            try {
                $system = [int](Get-CimInstance -ClassName Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'" -ErrorAction Stop).PercentProcessorTime
            }
            catch {
                $system = $null
            }
            $stamp = (Get-Date).ToString('o')
            foreach ($processId in $ids) {
                $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
                if ($null -eq $process) { continue }
                try {
                    $process.Refresh()
                    $rows.Add([pscustomobject]@{
                        t       = $stamp
                        pid     = $processId
                        cpu     = [double]$process.TotalProcessorTime.TotalSeconds
                        ws      = [int64]$process.WorkingSet64
                        priv    = [int64]$process.PrivateMemorySize64
                        threads = [int]$process.Threads.Count
                        handles = [int]$process.HandleCount
                        sys     = $system
                    })
                }
                catch {
                    continue
                }
            }
            Start-Sleep -Milliseconds $MonitorMs
        }
        if ($rows.Count -gt 0) {
            $rows | Export-Csv -LiteralPath $Csv -NoTypeInformation -Encoding utf8
        }
    } -ArgumentList $pidList, $stopFile, $csv, $MonitorMs
    return [pscustomobject]@{ Job = $job; StopFile = $stopFile; Csv = $csv }
}

function Stop-Monitor {
    param($Monitor)
    New-Item -ItemType File -Path $Monitor.StopFile | Out-Null
    Wait-Job -Job $Monitor.Job -Timeout 20 | Out-Null
    if ((Get-Job -Id $Monitor.Job.Id).State -eq 'Running') {
        Stop-Job -Job $Monitor.Job
    }
    Receive-Job -Job $Monitor.Job -ErrorAction SilentlyContinue | Out-Null
    Remove-Job -Job $Monitor.Job -Force
}

function Get-Metrics {
    param(
        [string]$Csv,
        [int]$ProcessId,
        [double]$Cpu0,
        [double]$Cpu1,
        [double]$WallSeconds
    )
    $rows = @()
    if (Test-Path -LiteralPath $Csv) {
        $rows = @(Import-Csv -LiteralPath $Csv | Where-Object { [int]$_.pid -eq $ProcessId })
    }
    $peakWs = [int64]0
    $peakPriv = [int64]0
    $peakThreads = 0
    $peakHandles = 0
    $peakCpu = 0.0
    $systemValues = [System.Collections.Generic.List[double]]::new()
    for ($index = 0; $index -lt $rows.Count; $index++) {
        $row = $rows[$index]
        if ([int64]$row.ws -gt $peakWs) { $peakWs = [int64]$row.ws }
        if ([int64]$row.priv -gt $peakPriv) { $peakPriv = [int64]$row.priv }
        if ([int]$row.threads -gt $peakThreads) { $peakThreads = [int]$row.threads }
        if ([int]$row.handles -gt $peakHandles) { $peakHandles = [int]$row.handles }
        if ($null -ne $row.sys -and "$($row.sys)" -ne '') {
            $systemValues.Add([double]$row.sys)
        }
        if ($index -gt 0) {
            $dt = ([datetime]::Parse($rows[$index].t) - [datetime]::Parse($rows[$index - 1].t)).TotalSeconds
            $dc = [double]$row.cpu - [double]$rows[$index - 1].cpu
            if ($dt -gt 0) {
                $instant = $dc / $dt * 100.0
                if ($instant -gt $peakCpu) { $peakCpu = $instant }
            }
        }
    }
    $cpuDelta = $Cpu1 - $Cpu0
    $average = if ($WallSeconds -gt 0) { $cpuDelta / $WallSeconds * 100.0 } else { 0.0 }
    $systemAvg = if ($systemValues.Count -gt 0) { ($systemValues | Measure-Object -Average).Average } else { $null }
    $systemPeak = if ($systemValues.Count -gt 0) { ($systemValues | Measure-Object -Maximum).Maximum } else { $null }
    return [pscustomobject]@{
        cpuSeconds          = [math]::Round($cpuDelta, 3)
        avgCpuPercent       = [math]::Round($average, 1)
        peakCpuPercent      = [math]::Round($peakCpu, 1)
        peakWorkingSetMiB   = [math]::Round($peakWs / 1MB, 1)
        peakPrivateMiB      = [math]::Round($peakPriv / 1MB, 1)
        threadsPeak         = $peakThreads
        handlesPeak         = $peakHandles
        systemCpuAvgPercent = if ($null -ne $systemAvg) { [math]::Round($systemAvg, 1) } else { $null }
        systemCpuPeakPercent = if ($null -ne $systemPeak) { [math]::Round($systemPeak, 1) } else { $null }
    }
}

function Split-Iso {
    param([string]$SourcePath, [int64]$ChunkBytes, [string]$Dir, [string]$PartsJson)
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Get-ChildItem -LiteralPath $Dir -File -ErrorAction SilentlyContinue | Remove-Item -Force
    $input = [IO.File]::OpenRead($SourcePath)
    $parts = [System.Collections.Generic.List[object]]::new()
    $buffer = New-Object byte[] (4 * 1024 * 1024)
    $number = 0
    try {
        while ($true) {
            $number++
            $path = Join-Path $Dir ("part-{0}.bin" -f $number)
            $output = [IO.File]::Create($path)
            $written = [int64]0
            try {
                while ($written -lt $ChunkBytes) {
                    $want = [int]($ChunkBytes - $written)
                    if ($want -gt $buffer.Length) { $want = $buffer.Length }
                    $read = $input.Read($buffer, 0, $want)
                    if ($read -le 0) { break }
                    $output.Write($buffer, 0, $read)
                    $written += $read
                }
            }
            finally {
                $output.Dispose()
            }
            if ($written -eq 0) {
                Remove-Item -LiteralPath $path -Force
                break
            }
            $parts.Add([pscustomobject]@{
                number = $number
                path   = $path
                size   = $written
            })
            if ($written -lt $ChunkBytes) { break }
        }
    }
    finally {
        $input.Dispose()
    }
    $parts | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath $PartsJson -Encoding utf8
    return $parts
}

$isoItem = Get-Item -LiteralPath $Iso
$isoSize = [int64]$isoItem.Length
if (-not (Test-Path -LiteralPath $Binary)) { throw "binary not found: $Binary" }
if (-not (Test-Path -LiteralPath $Spk)) { throw "plugin package not found: $Spk" }
if (-not (Test-Path -LiteralPath $Curl)) { throw "curl not found: $Curl" }
if (-not (Test-Path -LiteralPath $Pwsh)) { throw "pwsh not found: $Pwsh" }

$config = Join-Path $WorkDir 'config'
$data = Join-Path $WorkDir 'data'
$logs = Join-Path $WorkDir 'logs'
$trust = Join-Path $config 'trust.d'
$temp = Join-Path $WorkDir 'temp'
$partsDir = Join-Path $WorkDir 'parts'
$uploadDir = Join-Path $WorkDir 'upload'
$downloadDir = Join-Path $WorkDir 'download'
$pluginDir = Join-Path $WorkDir 'plugin'

Write-Step "Preparing $WorkDir"
if (Test-Path -LiteralPath $WorkDir) { Remove-Item -LiteralPath $WorkDir -Recurse -Force }
foreach ($directory in @($config, $data, $logs, $trust, $temp, $partsDir, $uploadDir, $downloadDir, $pluginDir)) {
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
}
$trustKey = Join-Path $PSScriptRoot 'sample-2026.pub'
if (-not (Test-Path -LiteralPath $trustKey)) { throw "sample trust key not found: $trustKey" }
Copy-Item -LiteralPath $trustKey -Destination (Join-Path $trust 'sample-2026.pub') -Force

Write-Step 'Splitting ISO (part cap 512 MiB)'
$parts = Split-Iso -SourcePath $Iso -ChunkBytes $PartSize -Dir $partsDir -PartsJson (Join-Path $partsDir 'parts.json')
Write-Step "Split into $($parts.Count) parts; computing SHA-256 (warms page cache)"
$expectedHex = (Get-FileHash -LiteralPath $Iso -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedId = "sha256:$expectedHex"
Write-Step "Expected id $expectedId"

$env:SVCD_CONFIG_DIR = $config
$env:SVCD_DATA_DIR = $data
$env:SVCD_LOG_DIR = $logs
$env:SVCD_TRUST_DIR = $trust
$env:SVCD_TEMP_DIR = $temp
$env:SVCD_BIND = '127.0.0.1:0'
$env:SVCD_RESCAN_INTERVAL_MS = '0'
$env:SVCD_BOOT_SCAN_WAIT_MS = '0'

$daemonOut = Join-Path $WorkDir 'daemon.out'
$daemonErr = Join-Path $WorkDir 'daemon.err'
$daemon = Start-Process -FilePath $Binary -ArgumentList @('run', '--foreground') -PassThru -NoNewWindow -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr

try {
    $port = Wait-Http -ConfigDir $config
    $base = "http://127.0.0.1:$port"
    Write-Step "Daemon pid=$($daemon.Id) at $base"

    Write-Step 'Installing filebench plugin'
    $installRaw = & $Curl -sS -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$Spk" "$base/api/v1/plugins?strategy=replace&enable=true"
    if ($LASTEXITCODE -ne 0) { throw "plugin install failed: $installRaw" }
    $install = $installRaw | ConvertFrom-Json
    if ($install.id -ne 'filebench') { throw "plugin install response: $installRaw" }

    $status = Invoke-RestMethod -Method Get -Uri "$base/api/v1/services/filebench"
    if ($status.summary.state -ne 'running') {
        Invoke-RestMethod -Method Post -Uri "$base/api/v1/services/filebench/start" | Out-Null
    }
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        $status = Invoke-RestMethod -Method Get -Uri "$base/api/v1/services/filebench"
        if ($status.summary.state -eq 'running' -and $null -ne $status.summary.pid) { break }
        Start-Sleep -Milliseconds 200
    }
    $pluginPid = [int]$status.summary.pid
    if ($pluginPid -le 0) { throw 'filebench plugin did not report a pid' }
    Write-Step "Plugin pid=$pluginPid state=$($status.summary.state)"
    $token = (Invoke-RestMethod -Method Post -Uri "$base/api/v1/plugins/filebench/ui-token").token

    # ---- upload phase -----------------------------------------------------
    Write-Step "Upload phase: $Concurrency concurrent logical files, $($parts.Count) parts each"
    $uploadMonitor = Start-Monitor -Pids @($daemon.Id) -Dir $uploadDir
    Start-Sleep -Milliseconds 1500
    $daemonProcess = Get-Process -Id $daemon.Id
    $daemonProcess.Refresh()
    $uploadDaemonCpu0 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $uploadDaemonIo0 = Get-ProcessIo $daemon.Id

    $workers = [System.Collections.Generic.List[object]]::new()
    $uploadWatch = [Diagnostics.Stopwatch]::StartNew()
    for ($index = 1; $index -le $Concurrency; $index++) {
        $outFile = Join-Path $uploadDir ("worker-{0}.json" -f $index)
        $process = Start-ManagedProcess -File $Pwsh -Arguments @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $PSScriptRoot 'upload-file-worker.ps1'),
            '-Base', $base,
            '-PartsList', (Join-Path $partsDir 'parts.json'),
            '-Out', $outFile,
            '-Name', 'proxmox-ve_9.2-1.iso',
            '-MimeType', 'application/x-iso9660-image'
        ) -OutFile (Join-Path $uploadDir ("worker-{0}.out" -f $index)) -ErrFile (Join-Path $uploadDir ("worker-{0}.err" -f $index))
        $workers.Add([pscustomobject]@{
            Index   = $index
            Process = $process
            OutFile = $outFile
            Started = [DateTime]::UtcNow
        })
    }
    foreach ($worker in $workers) {
        $worker.Process.WaitForExit()
        $worker | Add-Member -NotePropertyName Ended -NotePropertyValue ([DateTime]::UtcNow)
        $worker | Add-Member -NotePropertyName StdOut -NotePropertyValue $worker.Process.StandardOutput.ReadToEnd()
        $worker | Add-Member -NotePropertyName StdErr -NotePropertyValue $worker.Process.StandardError.ReadToEnd()
        $worker | Add-Member -NotePropertyName ExitCode -NotePropertyValue $worker.Process.ExitCode
    }
    $uploadWatch.Stop()
    $daemonProcess.Refresh()
    $uploadDaemonCpu1 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $uploadDaemonIo1 = Get-ProcessIo $daemon.Id
    Stop-Monitor $uploadMonitor
    $uploadDaemonMetrics = Get-Metrics -Csv $uploadMonitor.Csv -ProcessId $daemon.Id -Cpu0 $uploadDaemonCpu0 -Cpu1 $uploadDaemonCpu1 -WallSeconds $uploadWatch.Elapsed.TotalSeconds

    $uploadResults = [System.Collections.Generic.List[object]]::new()
    foreach ($worker in $workers) {
        $result = $null
        if (Test-Path -LiteralPath $worker.OutFile) {
            $result = Get-Content -LiteralPath $worker.OutFile -Raw | ConvertFrom-Json
        }
        $uploadResults.Add([pscustomobject]@{
            index    = $worker.Index
            exitCode = $worker.ExitCode
            seconds  = if ($null -ne $result) { [double]$result.seconds } else { $null }
            id       = if ($null -ne $result) { [string]$result.id } else { $null }
            size     = if ($null -ne $result) { [int64]$result.size } else { $null }
            error    = if ($worker.ExitCode -ne 0 -or $null -eq $result) { ($worker.StdErr + ' ' + $worker.StdOut).Trim() } else { $null }
        })
    }
    $uploadIds = @($uploadResults | Where-Object { $_.exitCode -eq 0 -and $null -ne $_.id } | ForEach-Object { $_.id } | Select-Object -Unique)
    $fileId = if ($uploadIds.Count -eq 1) { $uploadIds[0] } else { $expectedId }
    $uploadOk = @($uploadResults | Where-Object { $_.exitCode -eq 0 -and $_.id -eq $expectedId -and $_.size -eq $isoSize }).Count
    $uploadTimes = @($uploadResults | Where-Object { $null -ne $_.seconds } | ForEach-Object { $_.seconds })
    $uploadSummary = [pscustomobject]@{
        wallSeconds     = [math]::Round($uploadWatch.Elapsed.TotalSeconds, 3)
        throughputMiBps = [math]::Round(($Concurrency * $isoSize / 1MB) / $uploadWatch.Elapsed.TotalSeconds, 1)
        requestsOk      = $uploadOk
        requestsFailed  = $Concurrency - $uploadOk
        perRequestSeconds = [pscustomobject]@{
            min = if ($uploadTimes.Count) { [math]::Round(($uploadTimes | Measure-Object -Minimum).Minimum, 3) } else { $null }
            avg = if ($uploadTimes.Count) { [math]::Round(($uploadTimes | Measure-Object -Average).Average, 3) } else { $null }
            max = if ($uploadTimes.Count) { [math]::Round(($uploadTimes | Measure-Object -Maximum).Maximum, 3) } else { $null }
        }
        daemon = $uploadDaemonMetrics
        fileIo = [pscustomobject]@{
            readMiB  = [math]::Round(($uploadDaemonIo1.Read - $uploadDaemonIo0.Read) / 1MB, 1)
            writeMiB = [math]::Round(($uploadDaemonIo1.Write - $uploadDaemonIo0.Write) / 1MB, 1)
        }
        requests = $uploadResults
    }
    Write-Step "Upload finished: ok=$uploadOk/$Concurrency wall=$($uploadSummary.wallSeconds)s"

    # ---- download phase ---------------------------------------------------
    Write-Step "Download phase: $Concurrency concurrent GET /api/v1/file/$fileId"
    $downloadMonitor = Start-Monitor -Pids @($daemon.Id) -Dir $downloadDir
    Start-Sleep -Milliseconds 1500
    $daemonProcess.Refresh()
    $downloadDaemonCpu0 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $downloadDaemonIo0 = Get-ProcessIo $daemon.Id
    $downloads = [System.Collections.Generic.List[object]]::new()
    $downloadWatch = [Diagnostics.Stopwatch]::StartNew()
    for ($index = 1; $index -le $Concurrency; $index++) {
        $process = Start-ManagedProcess -File $Curl -Arguments @(
            '-sS', '-o', 'NUL', '-w', "HTTP %{http_code} %{time_total} %{size_download}`n",
            "$base/api/v1/file/$fileId"
        ) -OutFile (Join-Path $downloadDir ("curl-{0}.out" -f $index)) -ErrFile (Join-Path $downloadDir ("curl-{0}.err" -f $index))
        $downloads.Add([pscustomobject]@{
            Index   = $index
            Process = $process
            Started = [DateTime]::UtcNow
        })
    }
    foreach ($download in $downloads) {
        $download.Process.WaitForExit()
        $download | Add-Member -NotePropertyName Ended -NotePropertyValue ([DateTime]::UtcNow)
        $download | Add-Member -NotePropertyName StdOut -NotePropertyValue $download.Process.StandardOutput.ReadToEnd()
        $download | Add-Member -NotePropertyName StdErr -NotePropertyValue $download.Process.StandardError.ReadToEnd()
        $download | Add-Member -NotePropertyName ExitCode -NotePropertyValue $download.Process.ExitCode
    }
    $downloadWatch.Stop()
    $daemonProcess.Refresh()
    $downloadDaemonCpu1 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $downloadDaemonIo1 = Get-ProcessIo $daemon.Id
    Stop-Monitor $downloadMonitor
    $downloadDaemonMetrics = Get-Metrics -Csv $downloadMonitor.Csv -ProcessId $daemon.Id -Cpu0 $downloadDaemonCpu0 -Cpu1 $downloadDaemonCpu1 -WallSeconds $downloadWatch.Elapsed.TotalSeconds

    $downloadResults = [System.Collections.Generic.List[object]]::new()
    foreach ($download in $downloads) {
        $statusCode = $null
        $serverSeconds = $null
        $bytes = $null
        if ($download.StdOut -match 'HTTP\s+(\d{3})\s+([0-9.]+)\s+(\d+)') {
            $statusCode = [int]$Matches[1]
            $serverSeconds = [double]$Matches[2]
            $bytes = [int64]$Matches[3]
        }
        $downloadResults.Add([pscustomobject]@{
            index         = $download.Index
            exitCode      = $download.ExitCode
            status        = $statusCode
            seconds       = $serverSeconds
            bytes         = $bytes
            error         = if ($download.ExitCode -ne 0 -or $statusCode -ne 200) { ($download.StdErr + ' ' + $download.StdOut).Trim() } else { $null }
        })
    }
    $downloadOk = @($downloadResults | Where-Object { $_.exitCode -eq 0 -and $_.status -eq 200 -and $_.bytes -eq $isoSize }).Count
    $downloadTimes = @($downloadResults | Where-Object { $null -ne $_.seconds } | ForEach-Object { $_.seconds })
    $downloadSummary = [pscustomobject]@{
        wallSeconds     = [math]::Round($downloadWatch.Elapsed.TotalSeconds, 3)
        throughputMiBps = [math]::Round(($Concurrency * $isoSize / 1MB) / $downloadWatch.Elapsed.TotalSeconds, 1)
        requestsOk      = $downloadOk
        requestsFailed  = $Concurrency - $downloadOk
        perRequestSeconds = [pscustomobject]@{
            min = if ($downloadTimes.Count) { [math]::Round(($downloadTimes | Measure-Object -Minimum).Minimum, 3) } else { $null }
            avg = if ($downloadTimes.Count) { [math]::Round(($downloadTimes | Measure-Object -Average).Average, 3) } else { $null }
            max = if ($downloadTimes.Count) { [math]::Round(($downloadTimes | Measure-Object -Maximum).Maximum, 3) } else { $null }
        }
        daemon = $downloadDaemonMetrics
        fileIo = [pscustomobject]@{
            readMiB  = [math]::Round(($downloadDaemonIo1.Read - $downloadDaemonIo0.Read) / 1MB, 1)
            writeMiB = [math]::Round(($downloadDaemonIo1.Write - $downloadDaemonIo0.Write) / 1MB, 1)
        }
        requests = $downloadResults
    }
    Write-Step "Download finished: ok=$downloadOk/$Concurrency wall=$($downloadSummary.wallSeconds)s"

    # ---- plugin consume phase --------------------------------------------
    Write-Step 'Plugin phase: filebench consumes the logical file through $file'
    $pluginMonitor = Start-Monitor -Pids @($daemon.Id, $pluginPid) -Dir $pluginDir
    Start-Sleep -Milliseconds 1500
    $daemonProcess.Refresh()
    $pluginProcess = Get-Process -Id $pluginPid
    $pluginProcess.Refresh()
    $pluginDaemonCpu0 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $pluginCpu0 = [double]$pluginProcess.TotalProcessorTime.TotalSeconds
    $pluginIo0 = Get-ProcessIo $pluginPid
    $rpcBody = @{
        jsonrpc = '2.0'
        id      = 1
        method  = 'tools/call'
        params  = @{
            name      = 'consume'
            arguments = @{
                input = @{ '$file' = $fileId }
            }
        }
    } | ConvertTo-Json -Depth 10 -Compress
    $pluginWatch = [Diagnostics.Stopwatch]::StartNew()
    $rpc = Invoke-RestMethod -Method Post -Uri "$base/plugin-ui/filebench/$token/rpc" -ContentType 'application/json' -Body $rpcBody
    $pluginWatch.Stop()
    $daemonProcess.Refresh()
    $pluginProcess.Refresh()
    $pluginDaemonCpu1 = [double]$daemonProcess.TotalProcessorTime.TotalSeconds
    $pluginCpu1 = [double]$pluginProcess.TotalProcessorTime.TotalSeconds
    $pluginIo1 = Get-ProcessIo $pluginPid
    Stop-Monitor $pluginMonitor
    $pluginDaemonMetrics = Get-Metrics -Csv $pluginMonitor.Csv -ProcessId $daemon.Id -Cpu0 $pluginDaemonCpu0 -Cpu1 $pluginDaemonCpu1 -WallSeconds $pluginWatch.Elapsed.TotalSeconds
    $pluginMetrics = Get-Metrics -Csv $pluginMonitor.Csv -ProcessId $pluginPid -Cpu0 $pluginCpu0 -Cpu1 $pluginCpu1 -WallSeconds $pluginWatch.Elapsed.TotalSeconds

    $pluginSha = $null
    $pluginBytes = $null
    if ($null -ne $rpc.result -and $null -ne $rpc.result.structuredContent) {
        $pluginSha = [string]$rpc.result.structuredContent.sha256
        $pluginBytes = [int64]$rpc.result.structuredContent.bytes
    }
    $pluginOk = ($pluginSha -eq $expectedHex -and $pluginBytes -eq $isoSize)
    $pluginSummary = [pscustomobject]@{
        wallSeconds = [math]::Round($pluginWatch.Elapsed.TotalSeconds, 3)
        ok          = $pluginOk
        bytes       = $pluginBytes
        sha256      = $pluginSha
        plugin      = $pluginMetrics
        daemon      = $pluginDaemonMetrics
        fileIo = [pscustomobject]@{
            readMiB  = [math]::Round(($pluginIo1.Read - $pluginIo0.Read) / 1MB, 1)
            writeMiB = [math]::Round(($pluginIo1.Write - $pluginIo0.Write) / 1MB, 1)
        }
        rpc = $rpc
    }
    Write-Step "Plugin finished: ok=$pluginOk sha=$pluginSha"

    $result = [ordered]@{
        generatedAt     = (Get-Date).ToString('o')
        iso             = $Iso
        isoBytes        = $isoSize
        isoMiB          = [math]::Round($isoSize / 1MB, 1)
        partSizeBytes   = $PartSize
        partCount       = $parts.Count
        expectedBlobId  = $expectedId
        fileId          = $fileId
        uploadIdsMatch  = ($uploadIds.Count -eq 1 -and $uploadIds[0] -eq $expectedId)
        concurrency     = $Concurrency
        binary          = $Binary
        pluginPackage   = $Spk
        pluginPid       = $pluginPid
        upload          = $uploadSummary
        download        = $downloadSummary
        plugin          = $pluginSummary
        daemonStderrTail = @(Get-Content -LiteralPath $daemonErr -Tail 30 -ErrorAction SilentlyContinue)
    }
    $resultPath = Join-Path $WorkDir 'result.json'
    $result | ConvertTo-Json -Depth 14 | Set-Content -LiteralPath $resultPath -Encoding utf8
    Write-Step "Result written to $resultPath"
    Write-Host ''
    Write-Host '=== upload / daemon ==='
    $uploadSummary | Select-Object wallSeconds, throughputMiBps, requestsOk, requestsFailed, perRequestSeconds, fileIo, daemon | Format-List
    Write-Host '=== download / daemon ==='
    $downloadSummary | Select-Object wallSeconds, throughputMiBps, requestsOk, requestsFailed, perRequestSeconds, fileIo, daemon | Format-List
    Write-Host '=== plugin consume / daemon vs plugin ==='
    Write-Host ("wall={0}s ok={1} bytes={2} sha={3}" -f $pluginSummary.wallSeconds, $pluginSummary.ok, $pluginSummary.bytes, $pluginSummary.sha256)
    Write-Host 'daemon:'
    $pluginSummary.daemon | Format-List
    Write-Host 'plugin:'
    $pluginSummary.plugin | Format-List
}
finally {
    if ($null -ne $daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
        try { $daemon.WaitForExit() } catch {}
    }
}