[CmdletBinding()]
param(
    [string]$Iso = 'D:\learn\pve\i3-n305\proxmox-ve_9.2-1.iso',
    [string]$Repo = '',
    [string]$Binary = '',
    [string]$WorkDir = '',
    [int]$Concurrency = 5,
    [int]$MonitorMs = 250,
    [string]$Curl = 'C:\Windows\System32\curl.exe'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# tools/perf/blob-bench -> 仓库根（向上三级）
if (-not $Repo) { $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path }
if (-not $Binary) { $Binary = Join-Path $Repo 'target\release\svcd.exe' }
if (-not $WorkDir) { $WorkDir = Join-Path $env:TEMP 'svcd-blob-bench\run' }

# 1.59 GiB ISO 需要把 MAX_BLOB_BYTES 放宽到 > 1.59 GiB 的构建；生产 release
# 会以 413 拒绝。用 -Binary 指向放宽后的二进制，详见 README.md。

function Write-Step([string]$Text) {
    Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $Text)
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

function Wait-Http([string]$ConfigDir, [int]$TimeoutSec = 25) {
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

function Invoke-Phase {
    param(
        [ValidateSet('upload', 'download')][string]$Kind,
        [string]$Url,
        [string]$Dir,
        [int]$Concurrency,
        [int64]$IsoSize,
        [int]$DaemonPid,
        [int]$MonitorMs
    )

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Get-ChildItem -LiteralPath $Dir -File -ErrorAction SilentlyContinue | Remove-Item -Force
    $stopFile = Join-Path $Dir 'monitor.stop'
    $csv = Join-Path $Dir 'monitor.csv'
    Remove-Item -LiteralPath $stopFile -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $csv -ErrorAction SilentlyContinue

    $monitor = Start-Job -ScriptBlock {
        param($DaemonPid, $StopFile, $Csv, $MonitorMs)
        $rows = [System.Collections.Generic.List[object]]::new()
        while (-not (Test-Path -LiteralPath $StopFile)) {
            $process = Get-Process -Id $DaemonPid -ErrorAction SilentlyContinue
            if ($null -eq $process) { break }
            try {
                $process.Refresh()
                $system = $null
                try {
                    $system = [int](Get-CimInstance -ClassName Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'" -ErrorAction Stop).PercentProcessorTime
                }
                catch {
                    $system = $null
                }
                $rows.Add([pscustomobject]@{
                    t       = (Get-Date).ToString('o')
                    cpu     = [double]$process.TotalProcessorTime.TotalSeconds
                    ws      = [int64]$process.WorkingSet64
                    priv    = [int64]$process.PrivateMemorySize64
                    threads = [int]$process.Threads.Count
                    handles = [int]$process.HandleCount
                    sys     = $system
                })
            }
            catch {
                break
            }
            Start-Sleep -Milliseconds $MonitorMs
        }
        if ($rows.Count -gt 0) {
            $rows | Export-Csv -LiteralPath $Csv -NoTypeInformation -Encoding utf8
        }
    } -ArgumentList $DaemonPid, $stopFile, $csv, $MonitorMs

    Start-Sleep -Milliseconds 500
    $daemon = Get-Process -Id $DaemonPid
    $daemon.Refresh()
    $cpu0 = [double]$daemon.TotalProcessorTime.TotalSeconds
    $ws0 = [int64]$daemon.WorkingSet64
    $priv0 = [int64]$daemon.PrivateMemorySize64
    $io0 = Get-ProcessIo $DaemonPid

    $requests = [System.Collections.Generic.List[object]]::new()
    $watch = [Diagnostics.Stopwatch]::StartNew()
    for ($index = 1; $index -le $Concurrency; $index++) {
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $Curl
        $startInfo.UseShellExecute = $false
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true

        $startInfo.ArgumentList.Add('-sS')
        if ($Kind -eq 'upload') {
            $startInfo.ArgumentList.Add('-X')
            $startInfo.ArgumentList.Add('POST')
            $startInfo.ArgumentList.Add('--upload-file')
            $startInfo.ArgumentList.Add($Iso)
            $startInfo.ArgumentList.Add('-w')
            $startInfo.ArgumentList.Add("`nHTTP %{http_code} %{time_total} %{size_upload}`n")
            $startInfo.ArgumentList.Add($Url)
        }
        else {
            $startInfo.ArgumentList.Add('-o')
            $startInfo.ArgumentList.Add('NUL')
            $startInfo.ArgumentList.Add('-w')
            $startInfo.ArgumentList.Add("HTTP %{http_code} %{time_total} %{size_download}`n")
            $startInfo.ArgumentList.Add($Url)
        }

        $process = [Diagnostics.Process]::new()
        $process.StartInfo = $startInfo
        $started = [DateTime]::UtcNow
        [void]$process.Start()
        $requests.Add([pscustomobject]@{
            Index   = $index
            Process = $process
            Started = $started
        })
    }

    foreach ($request in $requests) {
        $request.Process.WaitForExit()
        $request | Add-Member -NotePropertyName Ended -NotePropertyValue ([DateTime]::UtcNow)
        $request | Add-Member -NotePropertyName StdOut -NotePropertyValue $request.Process.StandardOutput.ReadToEnd()
        $request | Add-Member -NotePropertyName StdErr -NotePropertyValue $request.Process.StandardError.ReadToEnd()
        $request | Add-Member -NotePropertyName ExitCode -NotePropertyValue $request.Process.ExitCode
    }
    $watch.Stop()

    # Take the daemon counters immediately after the last response, before the
    # monitor job is torn down.
    $daemon.Refresh()
    $cpu1 = [double]$daemon.TotalProcessorTime.TotalSeconds
    $ws1 = [int64]$daemon.WorkingSet64
    $priv1 = [int64]$daemon.PrivateMemorySize64
    $io1 = Get-ProcessIo $DaemonPid

    New-Item -ItemType File -Path $stopFile | Out-Null
    Wait-Job -Job $monitor -Timeout 15 | Out-Null
    if ((Get-Job -Id $monitor.Id).State -eq 'Running') {
        Stop-Job -Job $monitor
    }
    Receive-Job -Job $monitor -ErrorAction SilentlyContinue | Out-Null
    Remove-Job -Job $monitor -Force

    $rows = @()
    if (Test-Path -LiteralPath $csv) {
        $rows = @(Import-Csv -LiteralPath $csv)
    }

    $peakWs = [math]::Max($ws0, $ws1)
    $peakPriv = [math]::Max($priv0, $priv1)
    $peakThreads = 0
    $peakHandles = 0
    $peakCpu = 0.0
    $systemValues = [System.Collections.Generic.List[double]]::new()
    for ($index = 0; $index -lt $rows.Count; $index++) {
        $row = $rows[$index]
        $ws = [int64]$row.ws
        $priv = [int64]$row.priv
        $threads = [int]$row.threads
        $handles = [int]$row.handles
        if ($ws -gt $peakWs) { $peakWs = $ws }
        if ($priv -gt $peakPriv) { $peakPriv = $priv }
        if ($threads -gt $peakThreads) { $peakThreads = $threads }
        if ($handles -gt $peakHandles) { $peakHandles = $handles }
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

    $parsed = [System.Collections.Generic.List[object]]::new()
    foreach ($request in $requests) {
        $stdout = $request.StdOut
        $jsonText = ($stdout -split "`r?`n" | Where-Object { $_.TrimStart().StartsWith('{') } | Select-Object -First 1)
        $httpLine = ($stdout -split "`r?`n" | Where-Object { $_ -match '^HTTP\s+(\d{3})\s+([0-9.]+)\s+(\d+)' } | Select-Object -Last 1)
        $status = $null
        $serverSeconds = $null
        $bytes = $null
        if ($httpLine -and $httpLine -match '^HTTP\s+(\d{3})\s+([0-9.]+)\s+(\d+)') {
            $status = [int]$Matches[1]
            $serverSeconds = [double]$Matches[2]
            $bytes = [int64]$Matches[3]
        }
        $json = $null
        if ($jsonText) {
            try { $json = $jsonText | ConvertFrom-Json } catch { $json = $null }
        }
        $elapsed = ($request.Ended - $request.Started).TotalSeconds
        $errorText = $null
        if ($request.ExitCode -ne 0 -or $status -ne 200) {
            $errorText = ("exit=$($request.ExitCode); " + $request.StdErr.Trim() + ' ' + $stdout.Trim()).Trim()
        }
        $parsed.Add([pscustomobject]@{
            index         = $request.Index
            exitCode      = $request.ExitCode
            status        = $status
            serverSeconds = $serverSeconds
            clientSeconds = [math]::Round($elapsed, 3)
            bytes         = $bytes
            id            = if ($null -ne $json -and $null -ne $json.id) { [string]$json.id } else { $null }
            size          = if ($null -ne $json -and $null -ne $json.size) { [int64]$json.size } else { $null }
            error         = $errorText
        })
    }

    $wall = $watch.Elapsed.TotalSeconds
    $cpuDelta = $cpu1 - $cpu0
    $avgCpu = if ($wall -gt 0) { $cpuDelta / $wall * 100.0 } else { 0.0 }
    $totalBytes = [int64]$Concurrency * $IsoSize
    $throughput = if ($wall -gt 0) { ($totalBytes / 1MB) / $wall } else { 0.0 }
    $okCount = @($parsed | Where-Object { $_.status -eq 200 -and $_.exitCode -eq 0 }).Count
    $serverTimes = @($parsed | Where-Object { $null -ne $_.serverSeconds } | ForEach-Object { [double]$_.serverSeconds })
    $systemAvg = if ($systemValues.Count -gt 0) { ($systemValues | Measure-Object -Average).Average } else { $null }
    $systemPeak = if ($systemValues.Count -gt 0) { ($systemValues | Measure-Object -Maximum).Maximum } else { $null }

    return [pscustomobject]@{
        kind                    = $Kind
        concurrency             = $Concurrency
        wallSeconds             = [math]::Round($wall, 3)
        totalBytes              = $totalBytes
        throughputMiBps         = [math]::Round($throughput, 1)
        requestsOk              = $okCount
        requestsFailed          = $Concurrency - $okCount
        perRequestSeconds       = [pscustomobject]@{
            min = if ($serverTimes.Count -gt 0) { [math]::Round(($serverTimes | Measure-Object -Minimum).Minimum, 3) } else { $null }
            avg = if ($serverTimes.Count -gt 0) { [math]::Round(($serverTimes | Measure-Object -Average).Average, 3) } else { $null }
            max = if ($serverTimes.Count -gt 0) { [math]::Round(($serverTimes | Measure-Object -Maximum).Maximum, 3) } else { $null }
        }
        daemonCpuSeconds        = [math]::Round($cpuDelta, 3)
        daemonAvgCpuPercent     = [math]::Round($avgCpu, 1)
        daemonPeakCpuPercent    = [math]::Round($peakCpu, 1)
        daemonPeakWorkingSetMiB = [math]::Round($peakWs / 1MB, 1)
        daemonPeakPrivateMiB    = [math]::Round($peakPriv / 1MB, 1)
        daemonThreadsPeak       = $peakThreads
        daemonHandlesPeak       = $peakHandles
        processIoReadMiB        = [math]::Round(($io1.Read - $io0.Read) / 1MB, 1)
        processIoWriteMiB       = [math]::Round(($io1.Write - $io0.Write) / 1MB, 1)
        systemCpuAvgPercent     = if ($null -ne $systemAvg) { [math]::Round($systemAvg, 1) } else { $null }
        systemCpuPeakPercent    = if ($null -ne $systemPeak) { [math]::Round($systemPeak, 1) } else { $null }
        requests                = $parsed
    }
}

$isoItem = Get-Item -LiteralPath $Iso
$script:IsoSize = [int64]$isoItem.Length
if (-not (Test-Path -LiteralPath $Binary)) { throw "binary not found: $Binary" }
if (-not (Test-Path -LiteralPath $Curl)) { throw "curl not found: $Curl" }

$config = Join-Path $WorkDir 'config'
$data = Join-Path $WorkDir 'data'
$logs = Join-Path $WorkDir 'logs'
$trust = Join-Path $config 'trust.d'
$temp = Join-Path $WorkDir 'temp'
$uploadDir = Join-Path $WorkDir 'upload'
$downloadDir = Join-Path $WorkDir 'download'

Write-Step "Preparing $WorkDir"
foreach ($directory in @($config, $data, $logs, $temp, $uploadDir, $downloadDir)) {
    if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
}
foreach ($directory in @($config, $data, $logs, $trust, $temp, $uploadDir, $downloadDir)) {
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
}

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

    $probeCode = & $Curl -sS -o NUL -w '%{http_code}' "$base/api/v1/version"
    if ($LASTEXITCODE -ne 0 -or "$probeCode" -ne '200') {
        throw "NUL download probe failed: code=$probeCode exit=$LASTEXITCODE"
    }

    Write-Step 'Hashing ISO (also warms the page cache for repeatable CPU/loopback numbers)'
    $hash = (Get-FileHash -LiteralPath $Iso -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedId = "sha256:$hash"
    Write-Step "Expected id $expectedId"

    Write-Step "Upload phase: $Concurrency concurrent POST /api/v1/blobs"
    $upload = Invoke-Phase -Kind upload -Url "$base/api/v1/blobs" -Dir $uploadDir -Concurrency $Concurrency -IsoSize $script:IsoSize -DaemonPid $daemon.Id -MonitorMs $MonitorMs

    $ids = @($upload.requests | ForEach-Object { $_.id } | Where-Object { $_ })
    $uniqueIds = @($ids | Select-Object -Unique)
    $blobId = if ($uniqueIds.Count -eq 1) { $uniqueIds[0] } else { $expectedId }
    $uploadIdsMatch = ($uniqueIds.Count -eq 1 -and $uniqueIds[0] -eq $expectedId)
    Write-Step "Upload finished: ok=$($upload.requestsOk)/$Concurrency wall=$($upload.wallSeconds)s id=$blobId match=$uploadIdsMatch"

    Write-Step "Download phase: $Concurrency concurrent GET /api/v1/blobs/$blobId"
    $download = Invoke-Phase -Kind download -Url "$base/api/v1/blobs/$blobId" -Dir $downloadDir -Concurrency $Concurrency -IsoSize $script:IsoSize -DaemonPid $daemon.Id -MonitorMs $MonitorMs
    Write-Step "Download finished: ok=$($download.requestsOk)/$Concurrency wall=$($download.wallSeconds)s"

    $result = [ordered]@{
        generatedAt       = (Get-Date).ToString('o')
        iso               = $Iso
        isoBytes          = $script:IsoSize
        isoMiB            = [math]::Round($script:IsoSize / 1MB, 1)
        expectedBlobId    = $expectedId
        uploadIdsMatch    = $uploadIdsMatch
        blobId            = $blobId
        binary            = $Binary
        binaryNote        = 'benchmark build from src copy with MAX_BLOB_BYTES=4 GiB; production source still caps at 512 MiB'
        concurrency       = $Concurrency
        monitorIntervalMs = $MonitorMs
        upload            = $upload
        download          = $download
        daemonStderrTail  = @(Get-Content -LiteralPath $daemonErr -Tail 20 -ErrorAction SilentlyContinue)
    }

    $resultPath = Join-Path $WorkDir 'result.json'
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $resultPath -Encoding utf8
    Write-Step "Result written to $resultPath"
    Write-Host ''
    Write-Host '=== upload ==='
    $upload | Select-Object kind, concurrency, wallSeconds, throughputMiBps, requestsOk, requestsFailed, perRequestSeconds, daemonCpuSeconds, daemonAvgCpuPercent, daemonPeakCpuPercent, daemonPeakWorkingSetMiB, daemonPeakPrivateMiB, systemCpuAvgPercent, systemCpuPeakPercent, processIoReadMiB, processIoWriteMiB | Format-List
    Write-Host '=== download ==='
    $download | Select-Object kind, concurrency, wallSeconds, throughputMiBps, requestsOk, requestsFailed, perRequestSeconds, daemonCpuSeconds, daemonAvgCpuPercent, daemonPeakCpuPercent, daemonPeakWorkingSetMiB, daemonPeakPrivateMiB, systemCpuAvgPercent, systemCpuPeakPercent, processIoReadMiB, processIoWriteMiB | Format-List
}
finally {
    if ($null -ne $daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
        try { $daemon.WaitForExit() } catch {}
    }
}