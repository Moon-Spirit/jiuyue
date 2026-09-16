#Requires -Version 5.1
<#
.SYNOPSIS
    Start / stop / status for the local PostgreSQL 16 development database (jiuyue).

.DESCRIPTION
    Docker-free local dev database.
    Install : native Windows install of PostgreSQL 16 (EDB build), run as the
              Windows service "postgresql-x64-16".
    Endpoint: localhost:5432 (loopback only)
    App connection string:
        postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_dev

    Full details (paths, superuser, how to recreate): docs/agents/local-dev.md

.PARAMETER Command
    start   Start the postgresql-x64-16 service and wait until it accepts connections.
    stop    Stop the service and wait until it has fully stopped.
    status  Print service state, readiness and connection facts.
            Exit code 0 = server is up and accepting connections.

.EXAMPLE
    powershell -File scripts\pg-dev.ps1 status
    powershell -File scripts\pg-dev.ps1 stop
    powershell -File scripts\pg-dev.ps1 start

.NOTES
    start/stop control a Windows service and therefore need an elevated shell.
    status works without elevation.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet('start', 'stop', 'status')]
    [string]$Command
)

$ErrorActionPreference = 'Stop'

# --- instance facts (keep in sync with docs/agents/local-dev.md) ------------
$PgHome     = 'C:\Program Files\PostgreSQL\16'
$PgBin      = Join-Path $PgHome 'bin'
$PgData     = Join-Path $PgHome 'data'
$PgService  = 'postgresql-x64-16'
$PgHostName = 'localhost'
$PgPort     = 5432
$DbName     = 'jiuyue_dev'
$DbUser     = 'jiuyue'
$ConnStr    = 'postgres://' + $DbUser + ':jiuyue_dev_password@' + $PgHostName + ':' + $PgPort + '/' + $DbName

$Psql      = Join-Path $PgBin 'psql.exe'
$PgIsReady = Join-Path $PgBin 'pg_isready.exe'

$ElevationHint = 'start/stop control the Windows service "' + $PgService + '"; run this script from an elevated PowerShell (Run as Administrator).'

function Get-PgService {
    Get-Service -Name $PgService -ErrorAction SilentlyContinue
}

function Test-PgReady {
    if (-not (Test-Path -LiteralPath $PgIsReady)) { return $false }
    & $PgIsReady -h $PgHostName -p $PgPort -q *> $null
    return ($LASTEXITCODE -eq 0)
}

function Wait-PgReady {
    param([int]$TimeoutSeconds = 30)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if (Test-PgReady) { return $true }
        Start-Sleep -Milliseconds 400
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Wait-PgStopped {
    param([int]$TimeoutSeconds = 30)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $svc = Get-PgService
        if ((-not $svc) -or ($svc.Status -eq 'Stopped')) { return $true }
        Start-Sleep -Milliseconds 400
    } while ((Get-Date) -lt $deadline)
    return $false
}

switch ($Command) {
    'start' {
        $svc = Get-PgService
        if (-not $svc) {
            Write-Host ('[pg-dev] service {0} not found - is PostgreSQL 16 installed? See docs/agents/local-dev.md' -f $PgService)
            exit 1
        }
        if (Test-PgReady) {
            Write-Host ('[pg-dev] already running - accepting connections on {0}:{1}' -f $PgHostName, $PgPort)
            exit 0
        }
        try {
            Start-Service -Name $PgService
        } catch {
            Write-Host ('[pg-dev] failed to start {0}: {1}' -f $PgService, $_.Exception.Message)
            Write-Host ('[pg-dev] ' + $ElevationHint)
            exit 1
        }
        if (Wait-PgReady) {
            Write-Host ('[pg-dev] started - accepting connections on {0}:{1}' -f $PgHostName, $PgPort)
            exit 0
        }
        Write-Host ('[pg-dev] service started but not accepting connections after 30s - check the log in {0}\log' -f $PgData)
        exit 1
    }
    'stop' {
        $svc = Get-PgService
        if (-not $svc) {
            Write-Host ('[pg-dev] service {0} not found - is PostgreSQL 16 installed? See docs/agents/local-dev.md' -f $PgService)
            exit 1
        }
        if ($svc.Status -eq 'Stopped') {
            Write-Host '[pg-dev] already stopped'
            exit 0
        }
        try {
            Stop-Service -Name $PgService -Force
        } catch {
            Write-Host ('[pg-dev] failed to stop {0}: {1}' -f $PgService, $_.Exception.Message)
            Write-Host ('[pg-dev] ' + $ElevationHint)
            exit 1
        }
        if (Wait-PgStopped) {
            Write-Host '[pg-dev] stopped'
            exit 0
        }
        Write-Host '[pg-dev] stop requested but the service is still stopping after 30s'
        exit 1
    }
    'status' {
        $svc = Get-PgService
        if (-not $svc) {
            Write-Host ('[pg-dev] service {0} not found - is PostgreSQL 16 installed? See docs/agents/local-dev.md' -f $PgService)
            exit 1
        }
        $ready = Test-PgReady
        $endpointState = 'NOT accepting connections'
        if ($ready) { $endpointState = 'accepting connections' }
        Write-Host '[pg-dev] PostgreSQL 16 dev database'
        Write-Host ('  service  : {0} = {1} (startup: {2})' -f $PgService, $svc.Status, $svc.StartType)
        Write-Host ('  endpoint : {0}:{1} - {2}' -f $PgHostName, $PgPort, $endpointState)
        Write-Host ('  data dir : {0}' -f $PgData)
        Write-Host ('  psql     : {0}' -f $Psql)
        Write-Host ('  database : {0} (owner: {1})' -f $DbName, $DbUser)
        Write-Host ('  conn     : {0}' -f $ConnStr)
        if ($ready) { exit 0 } else { exit 1 }
    }
}
