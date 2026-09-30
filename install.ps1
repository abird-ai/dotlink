param(
    [string]$Version = $(if ($env:DOTLINK_VERSION) { $env:DOTLINK_VERSION } else { "latest" }),
    [string]$InstallDir = $env:DOTLINK_INSTALL_DIR
)

$ErrorActionPreference = "Stop"

if (-not $InstallDir) {
    if (-not $HOME) {
        throw "HOME is not set; pass -InstallDir or set DOTLINK_INSTALL_DIR."
    }
    $InstallDir = Join-Path $HOME ".local\bin"
}

$Repo = if ($env:DOTLINK_REPO) { $env:DOTLINK_REPO } else { "abird-ai/dotlink" }
$BaseUrl = $env:DOTLINK_RELEASE_BASE_URL
$Asset = "dotlink-windows-x86_64.exe"

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

    Write-Host "Downloading $Asset..."
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

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Destination = Join-Path $InstallDir "dotlink.exe"
    Copy-Item -Force $BinaryPath $Destination

    Write-Host "Installed dotlink to $Destination"
    if (-not (($env:PATH -split ';') -contains $InstallDir)) {
        Write-Host "Add $InstallDir to PATH to run 'dotlink' directly."
    }
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $WorkDir
}
