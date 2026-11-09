<#
.SYNOPSIS
    Converts a Modrinth .mrpack into an Odium Launcher manifest.json.

.DESCRIPTION
    Reads modrinth.index.json, keeps only client-side files, downloads each mod
    from the Modrinth CDN to compute the SHA-256 that the launcher requires, and
    stages the overrides/ folder for upload to GitHub.

    The resulting manifest uses the original Modrinth CDN URLs for mods, so you
    only need to host manifest.json and the small overrides folder yourself.
    Downloads are cached by SHA-512 in %LOCALAPPDATA%\OdiumLauncher\mrpack-cache,
    so re-running after a pack update only fetches new files.

.EXAMPLE
    .\Import-Mrpack.ps1 -Mrpack "C:\Downloads\pack.mrpack" -OutDir .\pack\my-pack `
        -ServerIp 45.85.60.39 -ServerPort 25592 `
        -OverridesUrlTemplate 'https://raw.githubusercontent.com/USER/REPO/main/overrides/{path}'
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$Mrpack,

    [Parameter(Mandatory = $true)]
    [string]$OutDir,

    # {path} = URL-encoded path relative to the instance (config/foo.toml).
    [string]$OverridesUrlTemplate = 'https://raw.githubusercontent.com/USER/REPO/main/overrides/{path}',

    [string]$Name = '',

    [string]$Version = '',

    [string]$ServerIp = '',

    [int]$ServerPort = 25565,

    [ValidateSet('', 'fabric', 'quilt', 'forge', 'neoforge')]
    [string]$LoaderType = '',

    [string]$LoaderVersion = '',

    [string]$McVersion = '',

    # Optional: copy staged overrides into an existing instance for a local test.
    [string]$InstanceDir = '',

    [switch]$SkipOverrides
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-Sha512([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA512).Hash.ToLowerInvariant()
}

function ConvertTo-UrlPath([string]$Path) {
    ($Path.Replace('\', '/').Split('/') | ForEach-Object { [uri]::EscapeDataString($_) }) -join '/'
}

function Test-SafeRelative([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
    $normalized = $Path.Replace('\', '/')
    if ($normalized.StartsWith('/')) { return $false }
    if ($normalized -match '^[A-Za-z]:') { return $false }
    $parts = $normalized.Split('/')
    if ($parts -contains '..' -or $parts -contains '.') { return $false }
    if (($parts | Where-Object { $_ -eq '' }).Count -gt 0) { return $false }
    return $true
}

function Get-CachedMod([string]$Url, [string]$Sha512, [string]$Sha1) {
    $key = if ($Sha512) { $Sha512 } elseif ($Sha1) { $Sha1 } else { [guid]::NewGuid().ToString('N') }
    $cachePath = Join-Path $script:cacheDir $key
    if (Test-Path -LiteralPath $cachePath) {
        if (-not $Sha512 -or (Get-Sha512 $cachePath) -eq $Sha512) {
            $script:cachedCount++
            return $cachePath
        }
        Remove-Item -LiteralPath $cachePath -Force
    }

    $part = "$cachePath.part"
    & curl.exe -L --fail --silent --show-error --http1.1 --retry 5 --retry-all-errors --retry-delay 2 `
        -A 'OdiumLauncher-MrpackImporter/1.0' -o $part $Url
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        throw "Download failed: $Url (curl exit $LASTEXITCODE)"
    }
    Move-Item -LiteralPath $part -Destination $cachePath -Force

    if ($Sha512) {
        $actual = Get-Sha512 $cachePath
        if ($actual -ne $Sha512) {
            Remove-Item -LiteralPath $cachePath -Force
            throw "SHA-512 mismatch: $Url"
        }
    }
    $script:downloadedCount++
    return $cachePath
}

if (-not (Test-Path -LiteralPath $Mrpack)) {
    throw "File not found: $Mrpack"
}

$work = Join-Path $env:TEMP ("odium_mrpack_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    [System.IO.Compression.ZipFile]::ExtractToDirectory((Resolve-Path -LiteralPath $Mrpack).Path, $work)
    $indexPath = Join-Path $work 'modrinth.index.json'
    if (-not (Test-Path -LiteralPath $indexPath)) {
        throw "modrinth.index.json is missing - this is not an .mrpack"
    }
    $index = [System.IO.File]::ReadAllText($indexPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json

    $deps = $index.dependencies
    if (-not $McVersion) { $McVersion = [string]$deps.minecraft }
    if (-not $McVersion) { throw "No Minecraft version in dependencies" }

    $detectedType = ''
    $detectedVersion = ''
    if ($deps.PSObject.Properties['neoforge']) {
        $detectedType = 'neoforge'; $detectedVersion = [string]$deps.neoforge
    } elseif ($deps.PSObject.Properties['forge']) {
        $detectedType = 'forge'; $detectedVersion = [string]$deps.forge
    } elseif ($deps.PSObject.Properties['fabric-loader']) {
        $detectedType = 'fabric'; $detectedVersion = [string]$deps.'fabric-loader'
    } elseif ($deps.PSObject.Properties['quilt-loader']) {
        $detectedType = 'quilt'; $detectedVersion = [string]$deps.'quilt-loader'
    }
    if ($LoaderType) { $detectedType = $LoaderType }
    if ($LoaderVersion) { $detectedVersion = $LoaderVersion }
    if (-not $detectedType) { throw "Cannot detect the loader in dependencies" }
    if (-not $detectedVersion) { throw "Cannot detect the loader version" }

    $manifestName = if ($Name) { $Name } else { "$($index.name) $($index.versionId)" }
    $manifestVersion = if ($Version) { $Version } else { [string]$index.versionId }

    $script:cacheDir = Join-Path $env:LOCALAPPDATA 'OdiumLauncher\mrpack-cache'
    New-Item -ItemType Directory -Path $script:cacheDir -Force | Out-Null
    $script:downloadedCount = 0
    $script:cachedCount = 0
    $skipped = 0

    $required = [ordered]@{}
    $optional = [ordered]@{}
    $total = $index.files.Count
    $position = 0

    foreach ($file in $index.files) {
        $position++
        if (-not (Test-SafeRelative $file.path)) {
            throw "Unsafe path in .mrpack: $($file.path)"
        }
        $relative = $file.path.Replace('\', '/')

        $clientEnv = 'required'
        if ($file.env -and $file.env.client) { $clientEnv = [string]$file.env.client }
        if ($clientEnv -eq 'unsupported') {
            $skipped++
            continue
        }
        if (-not $file.downloads -or $file.downloads.Count -eq 0) {
            throw "No download URL for $relative"
        }

        $sha512 = $null
        $sha1 = $null
        if ($file.hashes) {
            if ($file.hashes.sha512) { $sha512 = [string]$file.hashes.sha512 }
            if ($file.hashes.sha1) { $sha1 = [string]$file.hashes.sha1 }
        }

        $local = Get-CachedMod ([string]$file.downloads[0]) $sha512 $sha1
        $sha256 = Get-Sha256 $local
        $size = (Get-Item -LiteralPath $local).Length
        if ($file.fileSize -and $size -ne [long]$file.fileSize) {
            Write-Warning "Size mismatch for $relative : $size != $($file.fileSize)"
        }

        $entry = [ordered]@{
            path   = $relative
            url    = [string]$file.downloads[0]
            sha256 = $sha256
            size   = [long]$size
        }
        if ($clientEnv -eq 'optional') { $optional[$relative] = $entry }
        else { $required[$relative] = $entry }

        Write-Host ("[{0}/{1}] {2} ({3:N1} MB)" -f $position, $total, $relative, ($size / 1MB))
    }

    $overrideEntries = @()
    if (-not $SkipOverrides) {
        $overrideTarget = Join-Path $OutDir 'overrides'
        foreach ($folder in @(
            @{ Source = (Join-Path $work 'overrides'); Label = 'overrides' },
            @{ Source = (Join-Path $work 'client-overrides'); Label = 'client-overrides' }
        )) {
            if (-not (Test-Path -LiteralPath $folder.Source)) { continue }
            Get-ChildItem -LiteralPath $folder.Source -Recurse -File | Sort-Object FullName | ForEach-Object {
                $relative = $_.FullName.Substring($folder.Source.Length).TrimStart('\', '/').Replace('\', '/')
                if (-not (Test-SafeRelative $relative)) {
                    throw "Unsafe path in $($folder.Label): $relative"
                }
                $destination = Join-Path $overrideTarget $relative
                $parent = Split-Path -Parent $destination
                if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
                Copy-Item -LiteralPath $_.FullName -Destination $destination -Force

                $sha256 = Get-Sha256 $_.FullName
                $url = $OverridesUrlTemplate.Replace('{path}', (ConvertTo-UrlPath $relative))
                $url = $url.Replace('{name}', [uri]::EscapeDataString($_.Name))
                $entry = [ordered]@{
                    path   = $relative
                    url    = $url
                    sha256 = $sha256
                    size   = [long]$_.Length
                }
                $required[$relative] = $entry
                $overrideEntries += $entry
            }
        }
    }

    $manifest = [ordered]@{
        schema_version = 1
        name           = $manifestName
        version        = $manifestVersion
        mc_version     = $McVersion
        loader         = [ordered]@{
            type    = $detectedType
            version = $detectedVersion
        }
        files          = @($required.Values)
        optional_files = @($optional.Values)
    }
    if ($ServerIp) {
        $manifest.server = [ordered]@{
            ip   = $ServerIp
            port = $ServerPort
        }
    }

    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $json = $manifest | ConvertTo-Json -Depth 8
    $manifestPath = Join-Path $OutDir 'manifest.json'
    [System.IO.File]::WriteAllText($manifestPath, $json, (New-Object System.Text.UTF8Encoding($false)))

    if ($InstanceDir) {
        if (-not (Test-Path -LiteralPath $InstanceDir)) {
            throw "Instance directory not found: $InstanceDir"
        }
        foreach ($entry in $overrideEntries) {
            $source = Join-Path (Join-Path $OutDir 'overrides') $entry.path
            $destination = Join-Path $InstanceDir $entry.path
            $parent = Split-Path -Parent $destination
            if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
            Copy-Item -LiteralPath $source -Destination $destination -Force
        }
        Write-Host "Overrides copied into instance: $InstanceDir"
    }

    Write-Host ""
    Write-Host "Done."
    Write-Host "  manifest.json : $manifestPath"
    Write-Host "  pack          : $manifestName"
    Write-Host "  Minecraft     : $McVersion"
    Write-Host "  loader        : $detectedType $detectedVersion"
    Write-Host "  files         : $($required.Count) required, $($optional.Count) optional"
    Write-Host "  skipped       : $skipped (server-only)"
    Write-Host "  downloaded    : $script:downloadedCount, cache hits: $script:cachedCount"
    if (-not $SkipOverrides) {
        Write-Host "  overrides     : $(Join-Path $OutDir 'overrides')"
        Write-Host ""
        Write-Host "Next steps:"
        Write-Host "  1. Upload the overrides folder to your GitHub repository."
        Write-Host "  2. Replace USER/REPO in manifest.json, or re-run with"
        Write-Host "     -OverridesUrlTemplate 'https://raw.githubusercontent.com/YOUR_NAME/YOUR_REPO/main/overrides/{path}'"
        Write-Host "  3. Put manifest.json in the repository root and set its raw URL as the launcher source."
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
