$ErrorActionPreference = "Stop"

$Repo = if ($env:SHADOWDROID_REPO) { $env:SHADOWDROID_REPO } else { "andriyo/ShadowDroid" }
$Version = if ($env:SHADOWDROID_VERSION) { $env:SHADOWDROID_VERSION } else { "latest" }
$InstallDir = if ($env:SHADOWDROID_INSTALL_DIR) {
    $env:SHADOWDROID_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "ShadowDroid\bin"
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
    Copy-Item -Path (Join-Path $tmp "shadowdroid.exe") -Destination (Join-Path $InstallDir "shadowdroid.exe") -Force

    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not (($currentUserPath -split ";") -contains $InstallDir)) {
        $newPath = if ([string]::IsNullOrWhiteSpace($currentUserPath)) { $InstallDir } else { "$currentUserPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        $pathMessage = "Added $InstallDir to your user PATH. Open a new terminal if shadowdroid is not found."
    } else {
        $pathMessage = "$InstallDir is already on your user PATH."
    }

    Write-Host "shadowdroid installed to $(Join-Path $InstallDir 'shadowdroid.exe')"
    Write-Host "Run: shadowdroid connect"
    Write-Host $pathMessage
}
finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
