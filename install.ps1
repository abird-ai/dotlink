param(
    [string]$Version = $(if ($env:DOTLINK_VERSION) { $env:DOTLINK_VERSION } else { "latest" }),
    [string]$InstallDir = $env:DOTLINK_INSTALL_DIR
)

$ErrorActionPreference = "Stop"

function Get-DotlinkVersion {
    param([string]$Path)

    try {
        $Output = & $Path --version 2>$null | Select-Object -First 1
        if ($Output -match '^dotlink\s+(.+)$') {
            return $Matches[1].Trim()
        }
    }
    catch {
    }

    return $null
}

if (-not $InstallDir) {
    if (-not $HOME) {
        throw "HOME is not set; pass -InstallDir or set DOTLINK_INSTALL_DIR."
    }
    $InstallDir = Join-Path $HOME ".local\bin"
}

$Repo = if ($env:DOTLINK_REPO) { $env:DOTLINK_REPO } else { "abird-ai/dotlink" }
$BaseUrl = $env:DOTLINK_RELEASE_BASE_URL

$OsArchitecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($OsArchitecture) {
    ([System.Runtime.InteropServices.Architecture]::X64) {
        $Asset = "dotlink-windows-x86_64.exe"
    }
    ([System.Runtime.InteropServices.Architecture]::Arm64) {
        $Asset = "dotlink-windows-aarch64.exe"
    }
    default {
        throw "Unsupported Windows architecture: $OsArchitecture. Published: x64, arm64."
    }
}

if (-not $BaseUrl) {
    if ($Version -eq "latest") {
        $BaseUrl = "https://github.com/$Repo/releases/latest/download"
    } else {
        $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        $BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
    }
}

$WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dotlink-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $WorkDir | Out-Null

try {
    $BinaryPath = Join-Path $WorkDir $Asset
    $ChecksumPath = "$BinaryPath.sha256"

    Write-Host "Downloading dotlink..."
    Invoke-WebRequest -UseBasicParsing -Uri "$($BaseUrl.TrimEnd('/'))/$Asset" -OutFile $BinaryPath
    Invoke-WebRequest -UseBasicParsing -Uri "$($BaseUrl.TrimEnd('/'))/$Asset.sha256" -OutFile $ChecksumPath

    $Expected = ((Get-Content $ChecksumPath -Raw).Trim() -split "\s+")[0].ToLowerInvariant()
    if ($Expected -notmatch '^[0-9a-f]{64}$') {
        throw "Invalid SHA-256 sidecar."
    }

    $Actual = (Get-FileHash -Algorithm SHA256 $BinaryPath).Hash.ToLowerInvariant()
    if ($Expected -ne $Actual) {
        throw "SHA-256 verification failed."
    }

    $DownloadedVersion = Get-DotlinkVersion $BinaryPath

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Destination = Join-Path $InstallDir "dotlink.exe"
    $HadExisting = Test-Path -LiteralPath $Destination
    $OldVersion = $null
    $InstalledHash = $null

    if ($HadExisting) {
        $OldVersion = Get-DotlinkVersion $Destination
        $InstalledHash = (Get-FileHash -Algorithm SHA256 $Destination).Hash.ToLowerInvariant()
    }

    if ($InstalledHash -eq $Expected) {
        if ($DownloadedVersion) {
            Write-Host "dotlink $DownloadedVersion is already up to date at $Destination"
        } else {
            Write-Host "dotlink is already up to date at $Destination"
        }
    } else {
        Copy-Item -Force $BinaryPath $Destination

        if ($HadExisting) {
            if ($OldVersion -and $DownloadedVersion) {
                Write-Host "Updated dotlink $OldVersion -> $DownloadedVersion at $Destination"
            } elseif ($DownloadedVersion) {
                Write-Host "Updated dotlink to $DownloadedVersion at $Destination"
            } else {
                Write-Host "Updated dotlink at $Destination"
            }
        } elseif ($DownloadedVersion) {
            Write-Host "Installed dotlink $DownloadedVersion to $Destination"
        } else {
            Write-Host "Installed dotlink to $Destination"
        }
    }

    if (-not (($env:PATH -split ';') -contains $InstallDir)) {
        Write-Host "Add $InstallDir to PATH to run 'dotlink' directly."
    }
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $WorkDir
}
