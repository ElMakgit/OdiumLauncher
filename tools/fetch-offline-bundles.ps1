<#
.SYNOPSIS
    Downloads the pinned offline NeoForge archives into dist/neoforge-mirror/.

.DESCRIPTION
    A release build compiles these archives into the executable
    (see crates/pml-loader/build.rs), so the launcher can install NeoForge
    without touching Maven or GitHub at runtime. The archives are too large for
    git and are published as assets of the `neoforge-mirror` release.

    Every download is checked against the size and SHA-256 pinned in
    crates/pml-loader/src/lib.rs; a mismatch deletes the file and fails.

.PARAMETER Directory
    Destination folder. Defaults to dist/neoforge-mirror.

.PARAMETER Force
    Download again even when a valid archive is already present.

.EXAMPLE
    ./tools/fetch-offline-bundles.ps1 -Force
#>
[CmdletBinding()]
param(
    [string] $Directory,
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# $PSScriptRoot is empty while a param default is evaluated, so resolve it here.
if (-not $Directory) { $Directory = Join-Path $PSScriptRoot '..\dist\neoforge-mirror' }

$Base = 'https://github.com/ElMakgit/OdiumLauncher/releases/download/neoforge-mirror'
# Keep in sync with GITHUB_PROXIES in crates/pml-loader/src/lib.rs.
$Proxies = @('https://ghproxy.net/', 'https://ghfast.top/', 'https://gh-proxy.com/')

# Keep in sync with OFFLINE_BUNDLES in crates/pml-loader/src/lib.rs.
$Bundles = @(
    @{
        File   = 'neoforge-21.1.248-offline.zip'
        Size   = 24145957
        Sha256 = '59e0e8a436f780f90da8ab54bf687cb4dfb4f85ff43057e5c4cd65e17b50b26b'
    },
    @{
        File   = 'neoforge-1.20.1-47.1.79-offline.zip'
        Size   = 18678629
        Sha256 = 'ab863b0e7b6f872e7fad80ee062e322771a4f8dc12bed2a47eb6407d9707251a'
    }
)

function Test-Bundle {
    param([string] $Path, [long] $Size, [string] $Sha256)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    $file = Get-Item -LiteralPath $Path
    if ($file.Length -ne $Size) { return $false }
    # Get-FileHash is not present in PowerShell 5.1 on some Server images only
    # for old builds; the standard .NET API keeps the script portable.
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    return $actual -eq $Sha256
}

function Save-Bundle {
    param([string] $Url, [string] $Path, [long] $Size, [string] $Sha256)

    $part = "$Path.part"
    if (Test-Path -LiteralPath $part) { Remove-Item -LiteralPath $part -Force }

    Write-Host "==> downloading $Url"
    try {
        # BITS keeps large transfers resilient to a flaky connection.
        Start-BitsTransfer -Source $Url -Destination $part -Description $Path -ErrorAction Stop
    }
    catch {
        Write-Warning "BITS failed ($($_.Exception.Message)); falling back to Invoke-WebRequest"
        Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing
    }

    $length = (Get-Item -LiteralPath $part).Length
    if ($length -ne $Size) {
        Remove-Item -LiteralPath $part -Force
        throw "$Url returned $length bytes, expected $Size"
    }
    $actual = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) {
        Remove-Item -LiteralPath $part -Force
        throw "$Url has SHA-256 $actual, expected $Sha256"
    }

    Move-Item -LiteralPath $part -Destination $Path -Force
}

New-Item -ItemType Directory -Path $Directory -Force | Out-Null

foreach ($bundle in $Bundles) {
    $target = Join-Path $Directory $bundle.File
    if (-not $Force -and (Test-Bundle -Path $target -Size $bundle.Size -Sha256 $bundle.Sha256)) {
        Write-Host "==> $($bundle.File) is already valid"
        continue
    }

    $url = "$Base/$($bundle.File)"
    # The canonical asset first, then the same bytes through the proxies the
    # launcher itself trusts.
    $sources = @($url)
    foreach ($proxy in $Proxies) { $sources += "$proxy$url" }

    $saved = $false
    foreach ($source in $sources) {
        try {
            Save-Bundle -Url $source -Path $target -Size $bundle.Size -Sha256 $bundle.Sha256
            $saved = $true
            break
        }
        catch {
            Write-Warning $_.Exception.Message
        }
    }
    if (-not $saved) { throw "could not download $($bundle.File) from any source" }
}

Get-ChildItem -LiteralPath $Directory -Filter '*.zip' |
    Select-Object Name, Length | Format-Table -AutoSize
