<#
.SYNOPSIS
    Publishes an Odium Launcher pack to GitHub and builds a player-ready folder.

.DESCRIPTION
    Input is the folder produced by Import-Mrpack.ps1 (or New-OdiumManifest.ps1):
      manifest.json
      overrides/        configs and custom jars that must be hosted by you

    The script:
      1. Rewrites every overrides/* URL in manifest.json to the GitHub raw URL.
      2. Pushes overrides/ and manifest.json to the repository with git.
      3. Writes version.json for quick update checks.
      4. Creates dist/ with launcher_data/config.json (source URL preconfigured),
         a copy of OdiumLauncher.exe and a ready-to-send ZIP.

    Mods that came from Modrinth keep their CDN URLs, so they are not uploaded.

.PARAMETER Token
    GitHub token with "repo" scope (classic) or Contents: read/write.
    Defaults to $env:GITHUB_TOKEN or $env:GH_TOKEN. Not needed with -DryRun.

.EXAMPLE
    .\Publish-Pack.ps1 -PackDir ..\pack\eblyacraft-dons-edit -Repo myname/my-pack

.EXAMPLE
    .\Publish-Pack.ps1 -PackDir ..\pack\eblyacraft-dons-edit -Repo myname/my-pack -DryRun
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$PackDir,

    [string]$Repo = '',

    [string]$Token = '',

    [string]$Branch = 'main',

    [string]$Version = '',

    [string]$Tag = '',

    [string]$DistDir = '',

    [string]$Exe = '',

    [switch]$CreateRepo,

    [switch]$Private,

    [switch]$DryRun,

    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function ConvertTo-UrlPath([string]$Path) {
    ($Path.Replace('\', '/').Split('/') | ForEach-Object { [uri]::EscapeDataString($_) }) -join '/'
}

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    $parent = Split-Path -Parent $Path
    if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

function Invoke-GitHub([string]$Method, [string]$Uri, [hashtable]$Headers, [string]$Body = '') {
    $parameters = @{
        Method  = $Method
        Uri     = $Uri
        Headers = $Headers
    }
    if ($Body) {
        $parameters.Body = $Body
        $parameters.ContentType = 'application/json'
    }
    Invoke-RestMethod @parameters
}

function Test-RepoExists([string]$FullName, [hashtable]$Headers) {
    try {
        Invoke-GitHub 'GET' "https://api.github.com/repos/$FullName" $Headers | Out-Null
        return $true
    } catch {
        if ($_.Exception.Response.StatusCode.value__ -eq 404) { return $false }
        throw
    }
}

function New-Repo([string]$FullName, [hashtable]$Headers) {
    $parts = $FullName.Split('/')
    $owner = $parts[0]
    $name = $parts[1]
    $me = Invoke-GitHub 'GET' 'https://api.github.com/user' $Headers
    $body = @{
        name      = $name
        private   = [bool]$Private
        auto_init = $false
    } | ConvertTo-Json
    if ($me.login -eq $owner) {
        Invoke-GitHub 'POST' 'https://api.github.com/user/repos' $Headers $body | Out-Null
    } else {
        Invoke-GitHub 'POST' "https://api.github.com/orgs/$owner/repos" $Headers $body | Out-Null
    }
    Write-Host "Created repository $FullName"
}

$packRoot = (Resolve-Path -LiteralPath $PackDir).Path
$manifestPath = Join-Path $packRoot 'manifest.json'
$overridesRoot = Join-Path $packRoot 'overrides'

if (-not (Test-Path -LiteralPath $manifestPath)) {
    throw "manifest.json not found in $packRoot"
}
if (-not (Test-Path -LiteralPath $overridesRoot)) {
    throw "overrides folder not found in $packRoot"
}
if (-not $Repo) {
    $Repo = Read-Host 'GitHub repository (owner/name)'
}
if ($Repo -notmatch '^[^/]+/[^/]+$') {
    throw "Repository must look like owner/name, got: $Repo"
}

$rawBase = "https://raw.githubusercontent.com/$Repo/$Branch"
$manifestUrl = "$rawBase/manifest.json"

$manifest = [System.IO.File]::ReadAllText($manifestPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
if (-not $manifest.name) { throw 'manifest.json has no name' }
if ($Version) { $manifest.version = $Version }
if (-not $manifest.version) { $manifest.version = '1.0.0' }

# --- 1. Rewrite override URLs and make sure every override file is listed. ---
$overrideFiles = @{}
Get-ChildItem -LiteralPath $overridesRoot -Recurse -File | ForEach-Object {
    $relative = $_.FullName.Substring($overridesRoot.Length).TrimStart('\', '/').Replace('\', '/')
    $overrideFiles[$relative] = $_
}

$files = @($manifest.files)
$optional = @($manifest.optional_files)
$seen = @{}
foreach ($entry in @($files) + @($optional)) {
    $seen[$entry.path] = $true
}

$rewritten = 0
foreach ($entry in @($files) + @($optional)) {
    if ($overrideFiles.ContainsKey($entry.path)) {
        $entry.url = "$rawBase/overrides/" + (ConvertTo-UrlPath $entry.path)
        $rewritten++
    }
}

$added = 0
foreach ($relative in $overrideFiles.Keys) {
    if ($seen.ContainsKey($relative)) { continue }
    $file = $overrideFiles[$relative]
    $entry = [pscustomobject]@{
        path   = $relative
        url    = "$rawBase/overrides/" + (ConvertTo-UrlPath $relative)
        sha256 = Get-Sha256 $file.FullName
        size   = [long]$file.Length
    }
    $files += $entry
    $added++
}

$manifest.files = $files
$manifest.optional_files = $optional
$manifestJson = $manifest | ConvertTo-Json -Depth 8
Write-Utf8NoBom $manifestPath $manifestJson
Write-Host "Manifest: $($files.Count) files, $rewritten override URLs rewritten, $added added"

# --- 2. Player distribution: config + exe + zip. ---
$dist = if ($DistDir) { $DistDir } else { Join-Path $packRoot 'dist' }
$launcherData = Join-Path $dist 'launcher_data'
New-Item -ItemType Directory -Path $launcherData -Force | Out-Null

$sourceId = ($Repo.Split('/')[1] -replace '[^A-Za-z0-9_-]', '-').ToLowerInvariant()
$sourceName = [string]$manifest.name
if ($manifest.version -and -not $sourceName.EndsWith([string]$manifest.version)) {
    $sourceName = "$sourceName $($manifest.version)"
}
if ($manifest.server) {
    $serverConfig = [ordered]@{
        ip   = [string]$manifest.server.ip
        port = [int]$manifest.server.port
    }
} else {
    $serverConfig = [ordered]@{
        ip   = '127.0.0.1'
        port = 25565
    }
}
$config = [ordered]@{
    schema_version         = 1
    nick                   = 'Player'
    sources                = @([ordered]@{
        id           = $sourceId
        name         = $sourceName
        manifest_url = $manifestUrl
    })
    active_source_id       = $sourceId
    jvm                    = [ordered]@{
        xmx = 6144
        xms = 2048
    }
    server                 = $serverConfig
    optional_mods_overrides = @{}
    close_launcher_on_start = $true
    download_concurrency    = 8
}
Write-Utf8NoBom (Join-Path $launcherData 'config.json') ($config | ConvertTo-Json -Depth 6)

$exePath = $Exe
if (-not $exePath) {
    $candidate = Join-Path $PSScriptRoot '..\target\release\OdiumLauncher.exe'
    if (Test-Path -LiteralPath $candidate) {
        $exePath = (Resolve-Path -LiteralPath $candidate).Path
    }
}
if ($exePath) {
    if (-not (Test-Path -LiteralPath $exePath)) { throw "Exe not found: $exePath" }
    Copy-Item -LiteralPath $exePath -Destination (Join-Path $dist 'OdiumLauncher.exe') -Force
    Write-Host "Launcher copied: $exePath"
} else {
    Write-Warning 'OdiumLauncher.exe not found; dist will contain only launcher_data/config.json'
}

$readme = @"
Odium Launcher pack
===================

Pack:      $sourceName
Minecraft: $($manifest.mc_version)
Loader:    $($manifest.loader.type) $($manifest.loader.version)

How to play:
1. Keep OdiumLauncher.exe and the launcher_data folder together.
2. Run OdiumLauncher.exe.
3. Enter your nick and press "Play".

The launcher will download Java, the loader, mods and configs automatically.
The server is added to the multiplayer list.
"@
Write-Utf8NoBom (Join-Path $dist 'README.txt') $readme

$zipPath = Join-Path $packRoot ("OdiumLauncher-" + $sourceId + ".zip")
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path (Join-Path $dist '*') -DestinationPath $zipPath -Force
Write-Host "Player package: $zipPath"

# --- 3. Publish to GitHub. ---
$versionJson = [ordered]@{
    version         = [string]$manifest.version
    manifest_sha256 = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    updated_at      = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    manifest_url    = $manifestUrl
} | ConvertTo-Json -Depth 4

if ($DryRun) {
    Write-Host ''
    Write-Host 'Dry run: nothing was pushed.'
    Write-Host "  manifest URL : $manifestUrl"
    Write-Host "  dist folder  : $dist"
    Write-Host "  zip          : $zipPath"
    return
}

if (-not $Token) { $Token = $env:GITHUB_TOKEN }
if (-not $Token) { $Token = $env:GH_TOKEN }
if (-not $Token) {
    $secure = Read-Host 'GitHub token (repo scope)' -AsSecureString
    $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        $Token = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr)
    } finally {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr)
    }
}
if (-not $Token) { throw 'A GitHub token is required to publish' }

$headers = @{
    Authorization = "Bearer $Token"
    Accept        = 'application/vnd.github+json'
    'User-Agent'  = 'OdiumPublisher'
}

if ($CreateRepo) {
    if (-not (Test-RepoExists $Repo $headers)) {
        New-Repo $Repo $headers
    }
}

$git = Get-Command git -ErrorAction SilentlyContinue
if (-not $git) { throw 'git is not installed or not in PATH' }

$env:GIT_TERMINAL_PROMPT = '0'
$temp = Join-Path $env:TEMP ("odium_publish_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp -Force | Out-Null

try {
    $cloneUrl = "https://x-access-token:$Token@github.com/$Repo.git"
    $cloneOutput = & git clone --depth 1 $cloneUrl $temp 2>&1
    $cloned = $LASTEXITCODE -eq 0
    if (-not $cloned) {
        $text = ($cloneOutput | Out-String)
        if ($text -match 'Authentication failed|could not read Username|Repository not found|Permission denied') {
            throw "Cannot access $Repo with this token: $($text.Trim())"
        }
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Path $temp -Force | Out-Null
        & git -C $temp init 2>&1 | Out-Null
        & git -C $temp remote add origin $cloneUrl 2>&1 | Out-Null
    } else {
        & git -C $temp remote set-url origin $cloneUrl 2>&1 | Out-Null
    }
    & git -C $temp checkout -B $Branch 2>&1 | Out-Null

    $repoOverrides = Join-Path $temp 'overrides'
    if (Test-Path -LiteralPath $repoOverrides) {
        Remove-Item -LiteralPath $repoOverrides -Recurse -Force
    }
    Copy-Item -LiteralPath $overridesRoot -Destination $repoOverrides -Recurse -Force
    Copy-Item -LiteralPath $manifestPath -Destination (Join-Path $temp 'manifest.json') -Force
    Write-Utf8NoBom (Join-Path $temp 'version.json') $versionJson

    $readmePath = Join-Path $temp 'README.md'
    if (-not (Test-Path -LiteralPath $readmePath)) {
        $repoReadme = @"
# $($manifest.name)

Minecraft $($manifest.mc_version) · $($manifest.loader.type) $($manifest.loader.version)

This repository is the update source for Odium Launcher.
Players only need OdiumLauncher.exe and launcher_data/config.json.

Manifest: $manifestUrl
"@
        Write-Utf8NoBom $readmePath $repoReadme
    }

    & git -C $temp add -A 2>&1 | Out-Null
    $changes = (& git -C $temp status --porcelain) | Out-String
    if ($changes.Trim()) {
        & git -C $temp -c user.name='Odium Publisher' -c user.email='publisher@odium.local' `
            commit -m "Pack $($manifest.name) $($manifest.version)" 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'git commit failed' }
    } else {
        Write-Host 'No repository changes to commit.'
    }

    $pushArgs = @('-C', $temp, 'push')
    if ($Force) { $pushArgs += '--force' }
    $pushArgs += @('origin', $Branch)
    & git @pushArgs 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'git push failed' }

    if ($Tag) {
        & git -C $temp tag -f $Tag 2>&1 | Out-Null
        $tagArgs = @('-C', $temp, 'push')
        if ($Force) { $tagArgs += '--force' }
        $tagArgs += @('origin', $Tag)
        & git @tagArgs 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'git tag push failed' }
    }

    Write-Host ''
    Write-Host 'Published.'
    Write-Host "  repository   : https://github.com/$Repo"
    Write-Host "  manifest URL : $manifestUrl"
    Write-Host "  player zip   : $zipPath"
    Write-Host ''
    Write-Host 'Give players the ZIP. It already contains the source URL, server and Russian defaults.'
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\GIT_TERMINAL_PROMPT -ErrorAction SilentlyContinue
}
