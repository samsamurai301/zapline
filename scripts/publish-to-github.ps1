<#
.SYNOPSIS
    Create the private GitHub repository for zapline and push the full history.

.DESCRIPTION
    Run it from either place:
      * next to the downloaded zapline.bundle: it clones the bundle into .\zapline first, or
      * inside a zapline checkout (for example after `git clone zapline.bundle zapline`).

    With the GitHub CLI (gh) installed, it creates the repository for you (logging
    you in if needed). Without gh, it opens a pre-filled "new repository" page,
    waits for you to create the empty repository, then pushes with git (Git for
    Windows asks you to sign in to GitHub the first time).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\publish-to-github.ps1

.EXAMPLE
    .\publish-to-github.ps1 -Owner my-org -Name zapline
#>
param(
    [string]$Owner = "samsamurai301",
    [string]$Name = "zapline",
    # Create a public repository instead of a private one.
    [switch]$Public,
    # Push to this remote URL instead of github.com/<Owner>/<Name> (skips repository creation).
    [string]$Remote = ""
)

# "Continue", not "Stop": Windows PowerShell 5.1 turns redirected stderr of
# native commands (git, gh) into terminating errors under "Stop". Every
# native call's exit code is checked explicitly instead.
$ErrorActionPreference = "Continue"

function Info([string]$Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Fail([string]$Message) { Write-Host "error: $Message" -ForegroundColor Red; exit 1 }
function Git {
    & git @args
    if ($LASTEXITCODE -ne 0) { Fail "git $($args -join ' ') failed" }
}
function Has([string]$Tool) { [bool](Get-Command $Tool -ErrorAction SilentlyContinue) }

if (-not (Has "git")) { Fail "git is not installed. Get it from https://git-scm.com/download/win and run this script again." }

# 1. Find the zapline working copy, or create it from the bundle.
$root = (& git rev-parse --show-toplevel 2>$null)
if ($LASTEXITCODE -eq 0 -and $root -and (Test-Path (Join-Path $root "crates/zapline/Cargo.toml"))) {
    Set-Location $root
    Info "Using the zapline checkout at $root"
} else {
    $bundle = @((Join-Path $PSScriptRoot "zapline.bundle"), (Join-Path (Get-Location) "zapline.bundle")) |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $bundle) { Fail "Run this script inside a zapline checkout, or put it next to zapline.bundle." }
    $target = Join-Path (Split-Path $bundle -Parent) "zapline"
    if (Test-Path $target) { Fail "$target already exists. Run the script from inside it, or move it away." }
    Info "Cloning $bundle into $target"
    Git clone $bundle $target
    Set-Location $target
}

$branch = "$(& git branch --show-current)".Trim()
if (-not $branch) { Fail "No branch is checked out." }
$dirty = & git status --porcelain
if ($dirty) { Write-Host "warning: uncommitted changes will not be pushed:" -ForegroundColor Yellow; $dirty | Write-Host }

# 2. Create the GitHub repository (unless an explicit remote was given).
$visibility = if ($Public) { "public" } else { "private" }
$url = if ($Remote) { $Remote } else { "https://github.com/$Owner/$Name.git" }

if (-not $Remote) {
    if (Has "gh") {
        & gh auth status *> $null
        if ($LASTEXITCODE -ne 0) {
            Info "Logging in to GitHub (a browser window will open)"
            & gh auth login --hostname github.com --git-protocol https --web
            if ($LASTEXITCODE -ne 0) { Fail "GitHub login failed." }
        }
        & gh repo view "$Owner/$Name" *> $null
        if ($LASTEXITCODE -eq 0) {
            Info "Repository $Owner/$Name already exists; pushing to it"
        } else {
            Info "Creating $visibility repository $Owner/$Name"
            & gh repo create "$Owner/$Name" "--$visibility" --disable-wiki `
                --description "A fast, batteries-included Rust web framework: FastAPI ergonomics, enterprise structure, top-tier performance"
            if ($LASTEXITCODE -ne 0) { Fail "Could not create the repository (check the owner name and your permissions)." }
        }
        & gh auth setup-git *> $null
    } else {
        $newRepo = "https://github.com/new?owner=$Owner&name=$Name&visibility=$visibility"
        Write-Host ""
        Write-Host "The GitHub CLI (gh) is not installed, so create the repository in the browser:" -ForegroundColor Yellow
        Write-Host "  1. Open $newRepo"
        Write-Host "  2. Keep it EMPTY: no README, no .gitignore, no license."
        Write-Host "  3. Click 'Create repository'."
        Start-Process $newRepo -ErrorAction SilentlyContinue
        Read-Host "Press Enter once the empty repository exists"
    }
}

# 3. Point origin at GitHub and push the full history.
& git remote get-url origin *> $null
if ($LASTEXITCODE -eq 0) { Git remote set-url origin $url } else { Git remote add origin $url }

Info "Pushing $branch to $url"
Git push -u origin "${branch}:main"
Git push origin --tags

Write-Host ""
Write-Host "Done." -ForegroundColor Green
if (-not $Remote) {
    Write-Host "  Repository: https://github.com/$Owner/$Name"
    Write-Host "  CI runs:    https://github.com/$Owner/$Name/actions"
}
