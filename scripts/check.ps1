# Full local verification on Windows: what CI runs, in one command.
#
#   .\scripts\check.ps1          # everything
#   .\scripts\check.ps1 quick    # fmt + clippy + tests (all features)
#
# If script execution is disabled, run it with:
#   powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
param([string]$Mode = "full")

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Step([string]$Name) { Write-Host "`n==> $Name" -ForegroundColor Cyan }

function Run([string]$Exe, [string[]]$Arguments) {
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $Exe $($Arguments -join ' ')" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Step "rustfmt"
Run cargo @("fmt", "--all", "--check")

Step "clippy (all features)"
Run cargo @("clippy", "--workspace", "--exclude", "bench-servers", "--all-features", "--all-targets", "--", "-D", "warnings")

Step "tests (all features)"
Run cargo @("test", "-p", "zapline", "--all-features")
Run cargo @("test", "-p", "zapline-cli")

if ($Mode -eq "quick") { exit 0 }

Step "feature matrix"
Run cargo @("check", "-p", "zapline", "--no-default-features")
$features = "macros", "json", "query", "form", "openapi", "validate", "config", "tracing",
            "http2", "tls", "ws", "fs", "multipart", "compression", "jwt", "cookies", "views", "postgres", "sqlite", "password", "mail", "storage", "s3", "cache", "oauth", "i18n", "redis", "otlp"
foreach ($f in $features) {
    Write-Host "  - $f"
    Run cargo @("check", "-q", "-p", "zapline", "--no-default-features", "--features", $f)
}
Run cargo @("check", "-p", "zapline")

Step "examples"
Run cargo @("build", "-p", "zapline-examples", "--examples")
# The flagship app is its own project (own lockfile, sqlite only).
Push-Location examples/saas
$env:CARGO_TARGET_DIR = (Join-Path $PSScriptRoot "../target/saas")
try { Run cargo @("test", "-q") } finally { Remove-Item Env:\CARGO_TARGET_DIR; Pop-Location }

Step "docs"
$env:RUSTDOCFLAGS = "-D warnings"
Run cargo @("doc", "-p", "zapline", "--all-features", "--no-deps")
Remove-Item Env:\RUSTDOCFLAGS

Write-Host "`nAll checks passed." -ForegroundColor Green
