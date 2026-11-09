<#
.SYNOPSIS
    Generates an Odium Launcher manifest.json from a prepared modpack folder.

.DESCRIPTION
    The launcher does not download a zip of the whole pack: it reads manifest.json
    and downloads every listed file by direct URL, checking its SHA-256. This
    script scans a folder (mods/, config/, resourcepacks/, ...) and emits that
    manifest with hashes and sizes already filled in.

    URL template placeholders:
      {path} - URL-encoded path relative to PackDir, e.g. mods/jei.jar
      {name} - URL-encoded file name, e.g. jei.jar

    Use a GitHub Release download URL for large jars:
      https://github.com/USER/REPO/releases/download/pack-1/{name}
    or raw repository URLs for small text/config files:
      https://raw.githubusercontent.com/USER/REPO/main/{path}

.EXAMPLE
    .\New-OdiumManifest.ps1 -PackDir .\pack -LoaderVersion 21.1.248 `
        -UrlTemplate 'https://github.com/USER/REPO/releases/download/pack-1/{name}' `
        -ServerIp 45.85.60.39 -ServerPort 25592 -OutFile .\manifest.json
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$PackDir,

    [Parameter(Mandatory = $true)]
    [string]$UrlTemplate,

    [string]$Name = 'Odium NeoForge',

    [string]$Version = '1.0.0',

    [string]$McVersion = '1.21.1',

    [ValidateSet('fabric', 'quilt', 'forge', 'neoforge')]
    [string]$Loader = 'neoforge',

    [Parameter(Mandatory = $true)]
    [string]$LoaderVersion,

    [string]$ServerIp = '',

    [int]$ServerPort = 25565,

    # Paths or wildcards that go to optional_files instead of files.
    [string[]]$Optional = @(),

    [string]$OutFile = ''
)

$ErrorActionPreference = 'Stop'

$root = (Resolve-Path -LiteralPath $PackDir).Path

# Runtime noise that must never be shipped to other players.
$ignoredFiles = @(
    'manifest.json',
    '.pml_manifest_state.json',
    'options.txt',
    'servers.dat',
    'usercache.json',
    'servers.dat_old'
)
$ignoredDirs = @(
    '.git',
    '.github',
    'logs',
    'saves',
    'screenshots',
    'crash-reports',
    'natives',
    'versions',
    'libraries',
    'assets'
)

$required = [System.Collections.Generic.List[object]]::new()
$optional = [System.Collections.Generic.List[object]]::new()
$large = [System.Collections.Generic.List[string]]::new()

Get-ChildItem -LiteralPath $root -Recurse -File | Sort-Object FullName | ForEach-Object {
    $relative = $_.FullName.Substring($root.Length).TrimStart('\', '/').Replace('\', '/')
    $segments = $relative.Split('/')

    if ($ignoredFiles -contains $_.Name) { return }
    if ($segments | Where-Object { $ignoredDirs -contains $_ }) { return }
    if ($_.Extension -eq '.log') { return }
    if ($_.Name -like '*.tmp') { return }

    $encodedPath = ($segments | ForEach-Object { [uri]::EscapeDataString($_) }) -join '/'
    $url = $UrlTemplate.Replace('{path}', $encodedPath).Replace('{name}', [uri]::EscapeDataString($_.Name))
    $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()

    $entry = [ordered]@{
        path   = $relative
        url    = $url
        sha256 = $hash
        size   = [long]$_.Length
    }

    if ($_.Length -gt 100MB) {
        $large.Add("$relative ($([math]::Round($_.Length / 1MB, 1)) MB)")
    }

    $isOptional = $false
    foreach ($pattern in $Optional) {
        if ($relative -like $pattern) { $isOptional = $true; break }
    }

    if ($isOptional) { $optional.Add($entry) } else { $required.Add($entry) }
}

if ($required.Count -eq 0 -and $optional.Count -eq 0) {
    throw "No files found in $root. Put mods/configs there before generating the manifest."
}

$manifest = [ordered]@{
    schema_version = 1
    name           = $Name
    version        = $Version
    mc_version     = $McVersion
    loader         = [ordered]@{
        type    = $Loader
        version = $LoaderVersion
    }
    files          = $required
    optional_files = $optional
}
if ($ServerIp) {
    $manifest.server = [ordered]@{
        ip   = $ServerIp
        port = $ServerPort
    }
}

$json = $manifest | ConvertTo-Json -Depth 6
$utf8 = New-Object System.Text.UTF8Encoding($false)
if ($OutFile) {
    [System.IO.File]::WriteAllText((Join-Path $root $OutFile), $json, $utf8)
    Write-Host "Manifest written to $(Join-Path $root $OutFile)"
} else {
    $json
}

Write-Host "Required files: $($required.Count), optional files: $($optional.Count)"
if ($large.Count -gt 0) {
    Write-Warning "GitHub rejects files larger than 100 MB. Move these to GitHub Releases or an external CDN:"
    $large | ForEach-Object { Write-Warning "  $_" }
}
