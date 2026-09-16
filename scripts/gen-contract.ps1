#Requires -Version 5.1
<#
.SYNOPSIS
    Regenerate the frontend TypeScript contract from the Rust contract crate.

.DESCRIPTION
    The Rust types in backend/crates/contract are the single source of truth for
    the WebSocket wire contract. This script regenerates frontend/src/generated
    from them; the result is committed and CI fails if it drifts.

    It works from a clean shell with no session state: the cargo bin directory is
    added to PATH when cargo is not already available.

.PARAMETER Check
    Regenerate and then fail if the committed files changed (drift guard).

.EXAMPLE
    powershell -File scripts\gen-contract.ps1
    powershell -File scripts\gen-contract.ps1 -Check
#>
[CmdletBinding()]
param(
    [switch]$Check
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path (Join-Path $repoRoot 'backend') 'Cargo.toml'
$exportDir = Join-Path (Join-Path (Join-Path $repoRoot 'frontend') 'src') 'generated'

# A fresh shell (especially on Windows) may not have cargo on PATH yet.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $homeDir = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
    $cargoBin = Join-Path $homeDir '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) {
        $env:Path = $cargoBin + ';' + $env:Path
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '[gen-contract] cargo not found on PATH and not in %USERPROFILE%\.cargo\bin'
    exit 1
}

# Start from an empty directory so a removed type leaves no stale file behind.
if (Test-Path -LiteralPath $exportDir) {
    Remove-Item -LiteralPath $exportDir -Recurse -Force
}
New-Item -ItemType Directory -Path $exportDir -Force | Out-Null

$env:TS_RS_EXPORT_DIR = $exportDir

Write-Host ('[gen-contract] exporting Rust contract -> {0}' -f $exportDir)
& cargo test --manifest-path $manifest -p jiuyue-contract -- --nocapture
if ($LASTEXITCODE -ne 0) {
    Write-Host '[gen-contract] cargo test failed; bindings were not generated'
    exit $LASTEXITCODE
}

if ($Check) {
    Write-Host '[gen-contract] checking for drift in the committed contract'

    # Modified/deleted committed files.
    & git -C $repoRoot diff --exit-code -- frontend/src/generated
    if ($LASTEXITCODE -ne 0) {
        Write-Host '[gen-contract] DRIFT: frontend/src/generated differs from the committed contract'
        exit $LASTEXITCODE
    }

    # Files that were regenerated but never committed. `git diff` alone cannot
    # see these, so the guard would otherwise pass on a contract that is simply
    # missing from the repository.
    $untracked = & git -C $repoRoot ls-files --others --exclude-standard -- frontend/src/generated
    if ($untracked) {
        Write-Host '[gen-contract] DRIFT: the generated contract is not committed:'
        $untracked | ForEach-Object { Write-Host ('  ' + $_) }
        exit 1
    }

    Write-Host '[gen-contract] no drift'
}

Write-Host '[gen-contract] done'
