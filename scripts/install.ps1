param(
    [string]$Version = $(if ($env:SHADOWDROID_VERSION) { $env:SHADOWDROID_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:SHADOWDROID_INSTALL_DIR) { $env:SHADOWDROID_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "ShadowDroid\bin" }),
    [string]$Repo = $(if ($env:SHADOWDROID_REPO) { $env:SHADOWDROID_REPO } else { "andriyo/ShadowDroid" }),
    [switch]$NoPathUpdate,
    [switch]$Uninstall,
    [switch]$RemovePath,
    [switch]$Help
)

$ErrorActionPreference = "Stop"

function Show-Help {
@"
ShadowDroid installer for Windows PowerShell.

Usage:
  .\install.ps1 [options]

Options:
  -Version <tag>      Install a specific release tag, e.g. v0.1.1.
                      Values without a leading "v" are normalized.
  -InstallDir <dir>   Install directory. Default: %LOCALAPPDATA%\ShadowDroid\bin.
  -Repo <owner/repo>  GitHub repo. Default: andriyo/ShadowDroid.
  -NoPathUpdate       Install only; do not update the user PATH.
  -Uninstall          Remove shadowdroid.exe from the install directory.
  -RemovePath         With -Uninstall, also remove install dir from the user PATH.
  -Help               Show this help.

Environment overrides:
  SHADOWDROID_VERSION
  SHADOWDROID_INSTALL_DIR
  SHADOWDROID_REPO

Examples:
  powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"

  .\install.ps1 -Version v0.1.1 -InstallDir "$env:USERPROFILE\bin"
"@
}

function Normalize-Version([string]$Value) {
    if ($Value -eq "latest" -or $Value.StartsWith("v")) {
        return $Value
    }
    return "v$Value"
}

function Update-UserPath([string]$Directory) {
    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if (-not [string]::IsNullOrWhiteSpace($currentUserPath)) {
        $parts = $currentUserPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }
    if ($parts -notcontains $Directory) {
        $newPath = (@($parts) + $Directory) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        $env:Path = "$env:Path;$Directory"
        return "Added $Directory to your user PATH. Open a new terminal if shadowdroid is not found."
    }
    return "$Directory is already on your user PATH."
}

function Remove-UserPath([string]$Directory) {
    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ([string]::IsNullOrWhiteSpace($currentUserPath)) {
        return "User PATH is already empty."
    }
    $parts = $currentUserPath -split ";" | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_) -and $_ -ne $Directory
    }
    [Environment]::SetEnvironmentVariable("Path", ($parts -join ";"), "User")
    return "Removed $Directory from your user PATH."
}

if ($Help) {
    Show-Help
    exit 0
}

$Version = Normalize-Version $Version
$exe = Join-Path $InstallDir "shadowdroid.exe"

if ($Uninstall) {
    Remove-Item -Path $exe -Force -ErrorAction SilentlyContinue
    Write-Host "removed $exe"
    if ($RemovePath) {
        Write-Host (Remove-UserPath $InstallDir)
    }
    exit 0
}

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($arch) {
    "X64" { $target = "x86_64-pc-windows-msvc" }
    default { throw "Unsupported Windows architecture: $arch" }
}

$asset = "shadowdroid-$target.zip"
if ($Version -eq "latest") {
    $baseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $baseUrl = "https://github.com/$Repo/releases/download/$Version"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("shadowdroid-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "Installing shadowdroid $Version for $target..."

    $archive = Join-Path $tmp $asset
    $sums = Join-Path $tmp "SHA256SUMS"
    Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $archive
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sums

    $escaped = [Regex]::Escape($asset)
    $line = Get-Content $sums | Where-Object { $_ -match "^\s*([a-fA-F0-9]{64})\s+\*?$escaped\s*$" } | Select-Object -First 1
    if (-not $line) {
        throw "Checksum for $asset not found in SHA256SUMS"
    }
    $expected = ([Regex]::Match($line, "([a-fA-F0-9]{64})")).Groups[1].Value.ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "Checksum mismatch for $asset`nexpected: $expected`nactual:   $actual"
    }

    Expand-Archive -Path $archive -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path (Join-Path $tmp "shadowdroid.exe") -Destination $exe -Force

    Write-Host "shadowdroid installed to $exe"
    Write-Host "Run: shadowdroid connect"
    if (-not $NoPathUpdate) {
        Write-Host (Update-UserPath $InstallDir)
    } else {
        Write-Host "PATH update skipped. Add $InstallDir to PATH if needed."
    }
}
finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
