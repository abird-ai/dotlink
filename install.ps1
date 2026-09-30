param(
    [string]$Version = $(if ($env:ABIRD_LINK_VERSION) { $env:ABIRD_LINK_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:ABIRD_LINK_INSTALL_DIR) { $env:ABIRD_LINK_INSTALL_DIR } else { Join-Path $HOME ".local\bin" })
)

$ErrorActionPreference = "Stop"

$Repo = $env:ABIRD_LINK_REPO
$BaseUrl = $env:ABIRD_LINK_RELEASE_BASE_URL
$Asset = "abird-link-windows-x86_64.exe"

if (-not $BaseUrl) {
    if (-not $Repo) {
        throw "Set ABIRD_LINK_REPO=owner/repo or ABIRD_LINK_RELEASE_BASE_URL=https://... before running this installer."
    }

    if ($Version -eq "latest") {
        $BaseUrl = "https://github.com/$Repo/releases/latest/download"
    } else {
        $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        $BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
    }
}

$WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ("abird-link-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $WorkDir | Out-Null

try {
    $BinaryPath = Join-Path $WorkDir $Asset
    $ChecksumPath = "$BinaryPath.sha256"

    Write-Host "Downloading $Asset..."
    Invoke-WebRequest -UseBasicParsing -Uri "$($BaseUrl.TrimEnd('/'))/$Asset" -OutFile $BinaryPath
    Invoke-WebRequest -UseBasicParsing -Uri "$($BaseUrl.TrimEnd('/'))/$Asset.sha256" -OutFile $ChecksumPath

    $Expected = ((Get-Content $ChecksumPath -Raw).Trim() -split "\s+")[0].ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 $BinaryPath).Hash.ToLowerInvariant()
    if ($Expected -ne $Actual) {
        throw "SHA-256 verification failed."
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Destination = Join-Path $InstallDir "abird-link.exe"
    Copy-Item -Force $BinaryPath $Destination

    Write-Host "Installed abird-link to $Destination"
    if (-not (($env:PATH -split ';') -contains $InstallDir)) {
        Write-Host "Add $InstallDir to PATH to run 'abird-link' directly."
    }
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $WorkDir
}
