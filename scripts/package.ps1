<#
.SYNOPSIS
    Builds the release and packages the player and developer zips into dist\.

.DESCRIPTION
    dist\ACR-Rewind-<version>.zip (what players install; nothing else goes in it):
        dwmapi.dll, acr_hook.dll, acr-rewind.toml, signatures.toml, README.md (the player part of
        the repository README), LICENSE-MIT, LICENSE-APACHE, THIRD_PARTY_NOTICES.md, CHANGELOG.md
    dist\acr-rewind-dev-<version>.zip (development tools; skipped with -NoDevZip):
        injector.exe, acr-probe.exe, licenses and notices
    dist\SHA256SUMS.txt: sha256sum-style checksums of the zips.

    The cargo target directory is read from `cargo metadata` so this works even when
    CARGO_TARGET_DIR is redirected.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoDevZip
)

$ErrorActionPreference = 'Stop'
$env:Path += ";$HOME\.cargo\bin"

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not $SkipBuild) {
        Write-Host '==> cargo build --workspace --release'
        cargo build --workspace --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }

    $meta = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    $targetDir = $meta.target_directory
    $version = ($meta.packages | Where-Object { $_.name -eq 'acr_hook' }).version
    if (-not $version) { throw 'could not determine version from cargo metadata' }
    $rel = Join-Path $targetDir 'release'

    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $stagingRoot = Join-Path $targetDir 'package'

    function New-Staging([string]$name) {
        $dir = Join-Path $stagingRoot $name
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $dir
    }

    function Copy-Files([string]$staging, $files) {
        foreach ($f in $files) {
            if (-not (Test-Path -LiteralPath $f.Src)) { throw "missing required file: $($f.Src)" }
            Copy-Item -LiteralPath $f.Src -Destination (Join-Path $staging $f.Name) -Force
            Write-Host "    + $($f.Name)"
        }
    }

    function New-Zip([string]$staging, [string]$zipName) {
        $zip = Join-Path $dist $zipName
        if (Test-Path -LiteralPath $zip) { Remove-Item -Force -LiteralPath $zip }
        Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -CompressionLevel Optimal
        Write-Host "==> wrote $zip"
        $zip
    }

    function Get-ZipEntries([string]$zip) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $z = [System.IO.Compression.ZipFile]::OpenRead($zip)
        try { @($z.Entries | ForEach-Object { $_.FullName }) } finally { $z.Dispose() }
    }

    $legal = @(
        @{ Src = (Join-Path $root 'LICENSE-MIT');            Name = 'LICENSE-MIT' },
        @{ Src = (Join-Path $root 'LICENSE-APACHE');         Name = 'LICENSE-APACHE' },
        @{ Src = (Join-Path $root 'THIRD_PARTY_NOTICES.md'); Name = 'THIRD_PARTY_NOTICES.md' }
    )

    # The binaries must not depend on the VC++ redistributable (.cargo/config.toml: +crt-static).
    # Import names are plain ASCII in the PE, so a byte search is enough; this also catches a
    # stale build in a redirected target directory.
    foreach ($bin in 'dwmapi.dll', 'acr_hook.dll') {
        $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $rel $bin)))
        if ($text -match '(?i)vcruntime140|api-ms-win-crt-|msvcp140') {
            throw "$rel\$bin imports the dynamic C runtime ($($Matches[0])): stale build, or RUSTFLAGS overrides .cargo/config.toml; rebuild"
        }
    }

    # ---- Player zip ------------------------------------------------------------------------
    Write-Host "==> player package $version"
    $player = New-Staging "ACR-Rewind-$version"
    Copy-Files $player (@(
        @{ Src = (Join-Path $rel 'dwmapi.dll');                Name = 'dwmapi.dll' },
        @{ Src = (Join-Path $rel 'acr_hook.dll');              Name = 'acr_hook.dll' },
        @{ Src = (Join-Path $root 'config\acr-rewind.toml');   Name = 'acr-rewind.toml' },
        @{ Src = (Join-Path $root 'config\signatures.toml');   Name = 'signatures.toml' },
        @{ Src = (Join-Path $root 'CHANGELOG.md');             Name = 'CHANGELOG.md' }
    ) + $legal)

    # README.md for players: the repository README up to its developer section.
    $readme = [IO.File]::ReadAllText((Join-Path $root 'README.md')) -replace "`r`n", "`n"
    $cut = $readme.IndexOf("`n## For developers")
    if ($cut -lt 0) { throw "README.md has no '## For developers' section to cut at" }
    $readme = $readme.Substring(0, $cut)
    # Media (GIF, attached video, its captions) doesn't work in the zip: replace it with one link.
    $readme = [regex]::Replace($readme, '(?s)<!--.*?-->', '')
    $demoLinked = $false
    $kept = foreach ($line in $readme -split "`n") {
        if ($line -match '^\s*!\[' -or $line -match 'https://github\.com/user-attachments/' -or $line -match '(?i)demo video') {
            if (-not $demoLinked) { 'Demo video: https://github.com/cherrymcgerry/acr-rewind#readme'; $demoLinked = $true }
        } else { $line }
    }
    $readme = [regex]::Replace(($kept -join "`n"), "\n{3,}", "`n`n")
    $readme = $readme.TrimEnd() + "`n`n## Source code`n`nACR Rewind is open source; the source code and developer documentation are on GitHub: https://github.com/cherrymcgerry/acr-rewind`n"
    foreach ($bad in '![', '<!--', 'user-attachments') {
        if ($readme.Contains($bad)) { throw "player README.md still contains '$bad'" }
    }
    [IO.File]::WriteAllText((Join-Path $player 'README.md'), $readme, [Text.UTF8Encoding]::new($false))
    Write-Host '    + README.md (player section)'

    $playerZip = New-Zip $player "ACR-Rewind-$version.zip"
    $expected = @('dwmapi.dll', 'acr_hook.dll', 'acr-rewind.toml', 'signatures.toml', 'README.md',
        'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES.md', 'CHANGELOG.md') | Sort-Object
    $actual = Get-ZipEntries $playerZip | Sort-Object
    if (Compare-Object $expected $actual) {
        throw "player zip contents differ from the allowed list: $($actual -join ', ')"
    }
    $zips = @($playerZip)

    # ---- Developer zip ---------------------------------------------------------------------
    if (-not $NoDevZip) {
        Write-Host "==> developer package $version"
        $dev = New-Staging "acr-rewind-dev-$version"
        Copy-Files $dev (@(
            @{ Src = (Join-Path $rel 'injector.exe');  Name = 'injector.exe' },
            @{ Src = (Join-Path $rel 'acr-probe.exe'); Name = 'acr-probe.exe' }
        ) + $legal)
        $zips += New-Zip $dev "acr-rewind-dev-$version.zip"
    }

    # ---- Checksums -------------------------------------------------------------------------
    $sums = Join-Path $dist 'SHA256SUMS.txt'
    $lines = foreach ($z in $zips) {
        "$((Get-FileHash -Algorithm SHA256 -LiteralPath $z).Hash.ToLowerInvariant())  $(Split-Path -Leaf $z)"
    }
    [IO.File]::WriteAllText($sums, (($lines -join "`n") + "`n"), [Text.UTF8Encoding]::new($false))
    Write-Host "==> wrote $sums"

    foreach ($z in $zips) {
        $item = Get-Item -LiteralPath $z
        Write-Host ("{0} ({1} KB)" -f $item.Name, [math]::Round($item.Length / 1KB, 1))
        Get-ZipEntries $z | ForEach-Object { Write-Host "    $_" }
    }
}
finally {
    Pop-Location
}
