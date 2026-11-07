<#
.SYNOPSIS
    Fails when build output, runtime data or likely secrets are tracked by git.

.DESCRIPTION
    The launcher is portable: `target/`, `launcher_data/`, generated `dist/`
    folders and player ZIPs are build output, not source. This check runs in CI
    so a local `git add -f` cannot silently grow the repository again.

    It also scans source/config text files for common token and private-key
    patterns. Binary files and large manifests are skipped by size.
#>
$ErrorActionPreference = 'Stop'

$root = (& git rev-parse --show-toplevel).Trim()
if (-not $root) { throw 'not inside a git repository' }

$tracked = @(& git -C $root -c core.quotePath=false ls-files)
$failures = [System.Collections.Generic.List[string]]::new()
$maxBytes = 20 * 1024 * 1024

$artifactPatterns = @(
    @{ Pattern = '^(?:target|launcher_data|dist)/'; Reason = 'build/runtime directory' },
    @{ Pattern = '/(?:target|launcher_data|dist)/'; Reason = 'build/runtime directory' },
    @{ Pattern = '\.(?:pdb|log)$'; Reason = 'debug/log artifact' },
    @{ Pattern = '/OdiumLauncher-[^/]+\.zip$'; Reason = 'generated player ZIP' },
    @{ Pattern = '/manifest\.local\.json$'; Reason = 'generated local manifest' },
    @{ Pattern = '\.(?:exe|dll)$'; Reason = 'compiled binary' }
)

foreach ($file in $tracked) {
    foreach ($rule in $artifactPatterns) {
        if ($file -match $rule.Pattern) {
            $failures.Add("$file ($($rule.Reason))")
            break
        }
    }
    $path = Join-Path $root $file
    $item = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
    if ($item -and $item.Length -gt $maxBytes) {
        $failures.Add("$file ($([math]::Round($item.Length / 1MB, 1)) MiB exceeds 20 MiB)")
    }
}

# Keep this list narrow and text-only: minified JSON, jars and pack configs are
# not credentials and would only add noise.
$secretPatterns = @(
    'gh[pousr]_[A-Za-z0-9]{36,}',
    'github_pat_[A-Za-z0-9_]{50,}',
    'AKIA[0-9A-Z]{16}',
    '-----BEGIN (?:RSA |OPENSSH |EC )?PRIVATE KEY-----',
    'x-access-token:[^@\s]+@github\.com'
)
$textExtensions = @('.rs', '.toml', '.ps1', '.md', '.yml', '.yaml', '.json', '.txt', '.gitignore')
$skipSecretScan = @('tools/Check-RepoHygiene.ps1')

foreach ($file in $tracked) {
    if ($skipSecretScan -contains $file) { continue }
    $extension = [System.IO.Path]::GetExtension($file).ToLowerInvariant()
    if ($extension -and $textExtensions -notcontains $extension) { continue }
    if ($file -like 'pack/*') { continue }
    $path = Join-Path $root $file
    $item = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
    if (-not $item -or $item.Length -gt 2 * 1024 * 1024) { continue }
    $content = Get-Content -LiteralPath $path -Raw -ErrorAction SilentlyContinue
    if ($null -eq $content) { continue }
    foreach ($pattern in $secretPatterns) {
        if ($content -match $pattern) {
            $failures.Add("$file (possible secret matching $pattern)")
        }
    }
}

if ($failures.Count -gt 0) {
    Write-Host 'Repository hygiene failed:' -ForegroundColor Red
    $failures | Sort-Object -Unique | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "Repository hygiene OK: $($tracked.Count) tracked files."
exit 0
