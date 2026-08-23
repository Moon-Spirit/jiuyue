# JiuYue local dev readiness check (dockerless hosts).
# Expects: PostgreSQL 16 service running with superuser password in PGPASSWORD,
# plus redis-server on 127.0.0.1:6379. Databases jiuyue_dev / jiuyue_test must exist.
#
# Usage: .\deploy\local-dev.ps1   (exit 0 = ready)

$ErrorActionPreference = "Stop"

$pgPassword = $env:PGPASSWORD
if (-not $pgPassword) {
    Write-Host "PGPASSWORD not set; defaulting to jiuyue_dev" -ForegroundColor Yellow
    $pgPassword = "jiuyue_dev"
}
$env:PGPASSWORD = $pgPassword

$psql = "C:\Program Files\PostgreSQL\16\bin\psql.exe"
if (-not (Test-Path $psql)) {
    Write-Host "[FAIL] psql not found at $psql - install PostgreSQL 16 (winget install PostgreSQL.PostgreSQL.16)" -ForegroundColor Red
    exit 1
}

foreach ($db in @("jiuyue_dev", "jiuyue_test")) {
    & $psql -U postgres -h localhost -d $db -c "SELECT 1;" *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[FAIL] cannot connect to database $db" -ForegroundColor Red
        exit 1
    }
    Write-Host "[OK] postgres db $db reachable"
}

& C:\tools\redis\redis-cli.exe ping | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "[FAIL] redis not responding on 127.0.0.1:6379" -ForegroundColor Red
    exit 1
}
Write-Host "[OK] redis reachable"

Write-Host "[READY] local dev stack is up" -ForegroundColor Green
exit 0
