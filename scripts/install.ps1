[CmdletBinding()]
param(
    [switch]$Init,
    [string]$Version = $env:TAKKA_VERSION,
    [string]$InstallDir = $env:TAKKA_INSTALL_DIR,
    [switch]$SkipPathUpdate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repository = 'dchya24/takka'
$DefaultReleaseBaseUrl = "https://github.com/$Repository/releases/latest/download"
$Asset = 'takka-windows-x86_64.zip'
$RuntimeAsset = 'takka-runtime-windows-x86_64.exe'
$ChecksumAsset = 'checksums-windows.txt'

function Fail([string]$Message) {
    throw "takka installer: $Message"
}

# Pre-rebrand compat: honor legacy AGENTIC_* installer variables for one
# release cycle, with a deprecation warning (never silently).
function ResolveLegacyEnv([string]$NewName, [string]$LegacyName) {
    $New = [Environment]::GetEnvironmentVariable($NewName)
    if (-not [string]::IsNullOrWhiteSpace($New)) { return $New }
    $Legacy = [Environment]::GetEnvironmentVariable($LegacyName)
    if (-not [string]::IsNullOrWhiteSpace($Legacy)) {
        Write-Host "takka: warning: using legacy $LegacyName; set $NewName instead"
        return $Legacy
    }
    return $null
}

if (-not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows)) {
    Fail 'Windows is required. Use scripts/install.sh on Linux or macOS.'
}

$Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($env:TAKKA_INSTALLER_TESTING -eq '1' -and $env:TAKKA_TEST_ARCHITECTURE) {
    $Architecture = $env:TAKKA_TEST_ARCHITECTURE
}
if ($Architecture -ne 'X64') {
    Fail "Unsupported Windows architecture '$Architecture'. Prebuilt releases currently support Windows x64 only."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = ResolveLegacyEnv 'TAKKA_INSTALL_DIR' 'AGENTIC_INSTALL_DIR'
}
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        Fail 'LOCALAPPDATA is not set; pass -InstallDir explicitly.'
    }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\takka\bin'
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = ResolveLegacyEnv 'TAKKA_VERSION' 'AGENTIC_VERSION'
}

$ReleaseBaseUrl = if ($env:TAKKA_RELEASE_BASE_URL) {
    $env:TAKKA_RELEASE_BASE_URL.TrimEnd('/')
} else {
    $LegacyBaseUrl = ResolveLegacyEnv 'TAKKA_RELEASE_BASE_URL' 'AGENTIC_RELEASE_BASE_URL'
    if ($LegacyBaseUrl) { $LegacyBaseUrl.TrimEnd('/') } else { $DefaultReleaseBaseUrl }
}

if (-not [string]::IsNullOrWhiteSpace($Version)) {
    if (-not $Version.StartsWith('v')) {
        $Version = "v$Version"
    }
    if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
        Fail "Invalid version '$Version'; expected vX.Y.Z or X.Y.Z."
    }
    if ($ReleaseBaseUrl -match '/releases/latest/download$') {
        $ReleaseBaseUrl = $ReleaseBaseUrl -replace '/releases/latest/download$', "/releases/download/$Version"
    } else {
        $ReleaseBaseUrl = "$($ReleaseBaseUrl.TrimEnd('/'))/$Version"
    }
}

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("takka-install-" + [Guid]::NewGuid().ToString('N'))
$ArchivePath = Join-Path $TempDir $Asset
$ChecksumPath = Join-Path $TempDir $ChecksumAsset
$StageDir = Join-Path $TempDir 'stage'

try {
    New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

    # The headless runtime daemon is a hard dependency of the CLI (it spawns
    # it for every run) - download it separately (raw asset) with checksum.
    $RuntimePath = Join-Path $TempDir $RuntimeAsset

    Write-Host "Downloading $Asset..."
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$Asset" -OutFile $ArchivePath
    Write-Host "Downloading $RuntimeAsset..."
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$RuntimeAsset" -OutFile $RuntimePath
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$ChecksumAsset" -OutFile $ChecksumPath

    foreach ($Entry in @($Asset, $RuntimeAsset)) {
        $EscapedAsset = [Regex]::Escape($Entry)
        $ChecksumLine = Get-Content -LiteralPath $ChecksumPath |
            Where-Object { $_ -match "^([A-Fa-f0-9]{64})\s+\*?$EscapedAsset$" } |
            Select-Object -First 1
        if (-not $ChecksumLine) {
            Fail "Checksum entry for $Entry was not found in $ChecksumAsset."
        }
        $ExpectedHash = ([Regex]::Match($ChecksumLine, '^([A-Fa-f0-9]{64})')).Groups[1].Value
        $FilePath = if ($Entry -eq $Asset) { $ArchivePath } else { $RuntimePath }
        $ActualHash = (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash
        if (-not $ActualHash.Equals($ExpectedHash, [StringComparison]::OrdinalIgnoreCase)) {
            Fail "Checksum mismatch for $Entry."
        }
    }
    Write-Host 'Checksums verified.'

    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $StageDir -Force
    $StagedBinary = Join-Path $StageDir 'takka-windows-x86_64.exe'
    if (-not (Test-Path -LiteralPath $StagedBinary -PathType Leaf)) {
        Fail 'Downloaded archive does not contain takka-windows-x86_64.exe.'
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $Destination = Join-Path $InstallDir 'takka.exe'
    $RuntimeDestination = Join-Path $InstallDir 'takka-runtime.exe'
    $DestinationTemp = Join-Path $InstallDir ('.takka.install.' + [Guid]::NewGuid().ToString('N') + '.exe')
    Copy-Item -LiteralPath $StagedBinary -Destination $DestinationTemp -Force
    Move-Item -LiteralPath $DestinationTemp -Destination $Destination -Force
    Copy-Item -LiteralPath $RuntimePath -Destination $RuntimeDestination -Force
    Write-Host "takka installed successfully: $Destination"
    Write-Host "Runtime daemon installed:     $RuntimeDestination"

    $ConfigPath = Join-Path $env:USERPROFILE '.config\takka\config.json'
    $LegacyConfigPath = Join-Path $env:USERPROFILE '.config\agentic\config.json'
    if (Test-Path -LiteralPath $ConfigPath -PathType Leaf) {
        Write-Host "Existing config preserved: $ConfigPath"
    } elseif (Test-Path -LiteralPath $LegacyConfigPath -PathType Leaf) {
        Write-Host "Pre-rebrand config found at $LegacyConfigPath; it is still read."
        Write-Host 'Run `takka config migrate` to move it to the new location.'
    } else {
        Write-Host 'No config was created. Initialize it when ready.'
    }

    if (-not $SkipPathUpdate) {
        $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $PathEntries = @($UserPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $AlreadyPresent = $PathEntries | Where-Object {
            $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\')
        }
        if (-not $AlreadyPresent) {
            $NewUserPath = (@($PathEntries) + $InstallDir) -join ';'
            [Environment]::SetEnvironmentVariable('Path', $NewUserPath, 'User')
            Write-Host 'Added takka to the current user PATH. Open a new terminal to use it.'
        }
    }

    if ($Init) {
        & $Destination config init --interactive
        if ($LASTEXITCODE -ne 0) {
            Fail "Configuration wizard exited with status $LASTEXITCODE."
        }
    } else {
        Write-Host "Next: `"$Destination`" config init --interactive"
    }
} finally {
    if (Test-Path -LiteralPath $TempDir) {
        Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
