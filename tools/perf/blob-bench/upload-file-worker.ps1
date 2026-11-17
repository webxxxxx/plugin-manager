[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Base,
    [Parameter(Mandatory = $true)][string]$PartsList,
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$Name = 'file.bin',
    [string]$MimeType = 'application/octet-stream',
    [string]$Curl = 'C:\Windows\System32\curl.exe'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$parts = @(Get-Content -LiteralPath $PartsList -Raw | ConvertFrom-Json)
$stopwatch = [Diagnostics.Stopwatch]::StartNew()
$created = Invoke-RestMethod -Method Post -Uri "$Base/api/v1/file/uploads" -ContentType 'application/json' -Body '{}'
$uploadId = $created.uploadId
$etags = [System.Collections.Generic.List[object]]::new()

foreach ($part in $parts) {
    $number = [int]$part.number
    $path = [string]$part.path
    $json = & $Curl -sS -X PUT --upload-file $path "$Base/api/v1/file/uploads/$uploadId/parts/$number"
    if ($LASTEXITCODE -ne 0) {
        throw "curl part $number failed (exit $LASTEXITCODE): $json"
    }
    $meta = $json | ConvertFrom-Json
    if (-not $meta.etag -or [int64]$meta.size -le 0) {
        throw "part $number response: $json"
    }
    $etags.Add([pscustomobject]@{
        partNumber = $number
        etag       = [string]$meta.etag
        size       = [int64]$meta.size
    })
}

$completeBody = @{
    name     = $Name
    mimeType = $MimeType
    parts    = $etags
} | ConvertTo-Json -Depth 8 -Compress
$complete = Invoke-RestMethod -Method Post -Uri "$Base/api/v1/file/uploads/$uploadId/complete" -ContentType 'application/json' -Body $completeBody
$stopwatch.Stop()

$result = [pscustomobject]@{
    uploadId = $uploadId
    id       = $complete.id
    size     = [int64]$complete.size
    name     = $complete.name
    mimeType = $complete.mimeType
    seconds  = [math]::Round($stopwatch.Elapsed.TotalSeconds, 3)
    parts    = $etags
}
$result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Out -Encoding utf8
