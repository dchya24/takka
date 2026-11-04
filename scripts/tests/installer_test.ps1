Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Installer = Join-Path $RepoRoot 'scripts\install.ps1'
$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("takka-installer-test-" + [Guid]::NewGuid().ToString('N'))
$FixtureDir = Join-Path $TestRoot 'release'
$Global:TakkaDownloadLog = [System.Collections.Generic.List[string]]::new()
$Passed = 0
$Failed = 0

function Pass([string]$Name) {
    $Script:Passed++
    Write-Host "ok - $Name"
}

function Fail-Test([string]$Name, [string]$Detail) {
    $Script:Failed++
    Write-Error "not ok - $Name`n  $Detail" -ErrorAction Continue
}

function Assert-True([string]$Name, [bool]$Condition, [string]$Detail) {
    if ($Condition) { Pass $Name } else { Fail-Test $Name $Detail }
}

function Invoke-WebRequest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile,
        [switch]$UseBasicParsing
    )
    $Global:TakkaDownloadLog.Add($Uri)
    $Asset = [System.IO.Path]::GetFileName(([Uri]$Uri).AbsolutePath)
    Copy-Item -LiteralPath (Join-Path $FixtureDir $Asset) -Destination $OutFile -Force
}

function Write-Checksums([string]$Hash) {
    # The installer verifies the takka-runtime daemon against the same
    # checksum file, so the fixture always ships a matching runtime entry.
    $Runtime = Join-Path $FixtureDir 'takka-runtime-windows-x86_64.exe'
    $RuntimeHash = (Get-FileHash -LiteralPath $Runtime -Algorithm SHA256).Hash
    "$Hash  takka-windows-x86_64.zip" | Set-Content -LiteralPath (Join-Path $FixtureDir 'checksums-windows.txt') -Encoding ascii
    "$RuntimeHash  takka-runtime-windows-x86_64.exe" | Add-Content -LiteralPath (Join-Path $FixtureDir 'checksums-windows.txt') -Encoding ascii
}

function Invoke-InstallerCase {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [string]$Version,
        [switch]$Init
    )
    $CaseDir = Join-Path $TestRoot $Name
    $InstallDir = Join-Path $CaseDir 'bin'
    $env:USERPROFILE = Join-Path $CaseDir 'home'
    $env:LOCALAPPDATA = Join-Path $CaseDir 'local-app-data'
    $env:TAKKA_RELEASE_BASE_URL = 'https://fixtures.invalid/releases/latest/download'
    $env:TAKKA_VERSION = $null
    New-Item -ItemType Directory -Path $env:USERPROFILE -Force | Out-Null
    $Global:TakkaDownloadLog.Clear()

    if ($Version) {
        & $Installer -InstallDir $InstallDir -Version $Version -SkipPathUpdate -Init:$Init
    } else {
        & $Installer -InstallDir $InstallDir -SkipPathUpdate -Init:$Init
    }
    return @{ CaseDir = $CaseDir; InstallDir = $InstallDir }
}

function Build-FixtureExe {
    <#
        Compile the fixture C# source into a runnable console executable.

        PowerShell 7 (pwsh) cannot emit executables via Add-Type -- it raises
        "Both the assembly types 'ConsoleApplication' and 'WindowsApplication'
        are not currently supported." Delegate to Windows PowerShell 5.1
        (powershell.exe), which is always installed on Windows and emits a
        self-contained .NET Framework executable (no runtimeconfig.json is
        required, which matters because the installer copies only the .exe).
    #>
    param(
        [Parameter(Mandatory = $true)][string]$SourceFile,
        [Parameter(Mandatory = $true)][string]$Output
    )
    $BuilderPath = Join-Path ([System.IO.Path]::GetTempPath()) ("takka-build-fixture-" + [Guid]::NewGuid().ToString('N') + '.ps1')
    $Builder = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$source = Get-Content -LiteralPath '{0}' -Raw
Add-Type -TypeDefinition $source -OutputType ConsoleApplication -OutputAssembly '{1}'
'@ -f ($SourceFile -replace "'", "''"), ($Output -replace "'", "''")
    Set-Content -LiteralPath $BuilderPath -Value $Builder -Encoding UTF8
    try {
        $buildOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $BuilderPath 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Building fixture executable failed (exit $LASTEXITCODE):`n$buildOutput"
        }
    } finally {
        Remove-Item -LiteralPath $BuilderPath -Force -ErrorAction SilentlyContinue
    }
}

try {
    New-Item -ItemType Directory -Path $FixtureDir -Force | Out-Null
    $BuildDir = Join-Path $TestRoot 'build'
    New-Item -ItemType Directory -Path $BuildDir -Force | Out-Null
    $FixtureSource = @'
using System;
using System.IO;

public static class FixtureTakka {
    public static int Main(string[] args) {
        string log = Environment.GetEnvironmentVariable("TAKKA_INIT_LOG");
        if (!String.IsNullOrEmpty(log)) {
            File.WriteAllText(log, String.Join(" ", args));
        }
        return 0;
    }
}
'@
    $FixtureSourcePath = Join-Path $BuildDir 'FixtureTakka.cs'
    Set-Content -LiteralPath $FixtureSourcePath -Value $FixtureSource -Encoding UTF8
    Build-FixtureExe -SourceFile $FixtureSourcePath -Output (Join-Path $BuildDir 'takka-windows-x86_64.exe')
    $Archive = Join-Path $FixtureDir 'takka-windows-x86_64.zip'
    Compress-Archive -LiteralPath (Join-Path $BuildDir 'takka-windows-x86_64.exe') -DestinationPath $Archive -Force
    $RuntimeFixture = Join-Path $FixtureDir 'takka-runtime-windows-x86_64.exe'
    'runtime daemon fixture' | Set-Content -LiteralPath $RuntimeFixture -Encoding ascii
    $GoodHash = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash
    Write-Checksums $GoodHash

    $Result = Invoke-InstallerCase -Name 'verified-install'
    Assert-True 'Windows x64 installs verified executable' `
        (Test-Path -LiteralPath (Join-Path $Result.InstallDir 'takka.exe') -PathType Leaf) `
        'takka.exe was not installed'

    $DefaultInitLog = Join-Path $TestRoot 'default-init.log'
    $env:TAKKA_INIT_LOG = $DefaultInitLog
    Invoke-InstallerCase -Name 'default-skips-init' | Out-Null
    Assert-True 'Windows default install does not invoke config init' `
        (-not (Test-Path -LiteralPath $DefaultInitLog)) `
        'fixture executable was invoked without -Init'

    $ExplicitInitLog = Join-Path $TestRoot 'explicit-init.log'
    $env:TAKKA_INIT_LOG = $ExplicitInitLog
    Invoke-InstallerCase -Name 'explicit-init' -Init | Out-Null
    Assert-True 'Windows -Init invokes config wizard' `
        ((Get-Content -LiteralPath $ExplicitInitLog -Raw) -eq 'config init --interactive') `
        'initializer arguments did not match config init --interactive'
    $env:TAKKA_INIT_LOG = $null

    $ConfigCase = Join-Path $TestRoot 'config-preserved'
    $ConfigPath = Join-Path $ConfigCase 'home\.config\takka\config.json'
    New-Item -ItemType Directory -Path (Split-Path $ConfigPath) -Force | Out-Null
    '{"keep":true}' | Set-Content -LiteralPath $ConfigPath -Encoding ascii
    $Result = Invoke-InstallerCase -Name 'config-preserved'
    Assert-True 'Windows install preserves existing config' `
        ((Get-Content -LiteralPath $ConfigPath -Raw) -match '"keep":true') `
        'config marker changed'

    $LegacyConfigCase = Join-Path $TestRoot 'legacy-config-preserved'
    $LegacyConfigPath = Join-Path $LegacyConfigCase 'home\.config\agentic\config.json'
    New-Item -ItemType Directory -Path (Split-Path $LegacyConfigPath) -Force | Out-Null
    '{"keep":true}' | Set-Content -LiteralPath $LegacyConfigPath -Encoding ascii
    Invoke-InstallerCase -Name 'legacy-config-preserved' | Out-Null
    Assert-True 'Windows install preserves pre-rebrand config' `
        ((Get-Content -LiteralPath $LegacyConfigPath -Raw) -match '"keep":true') `
        'legacy config was modified or removed'

    $Result = Invoke-InstallerCase -Name 'tagged-version' -Version '0.3.2'
    Assert-True 'Windows explicit version selects tagged URL' `
        (($Global:TakkaDownloadLog -join "`n") -match '/releases/download/v0\.3\.2/') `
        ($Global:TakkaDownloadLog -join ', ')

    $MismatchDir = Join-Path $TestRoot 'checksum-mismatch'
    $MismatchBin = Join-Path $MismatchDir 'bin'
    New-Item -ItemType Directory -Path $MismatchBin -Force | Out-Null
    'old executable' | Set-Content -LiteralPath (Join-Path $MismatchBin 'takka.exe') -Encoding ascii
    Write-Checksums ('0' * 64)
    $MismatchFailed = $false
    try {
        Invoke-InstallerCase -Name 'checksum-mismatch' | Out-Null
    } catch {
        $MismatchFailed = $_.Exception.Message -match 'Checksum mismatch'
    }
    $OldPreserved = (Get-Content -LiteralPath (Join-Path $MismatchBin 'takka.exe') -Raw) -match 'old executable'
    Assert-True 'Windows checksum mismatch preserves existing executable' `
        ($MismatchFailed -and $OldPreserved) `
        'checksum failure was not reported or old executable changed'
    Write-Checksums $GoodHash

    $env:TAKKA_INSTALLER_TESTING = '1'
    $env:TAKKA_TEST_ARCHITECTURE = 'Arm64'
    $ArmFailed = $false
    try {
        Invoke-InstallerCase -Name 'unsupported-arm64' | Out-Null
    } catch {
        $ArmFailed = $_.Exception.Message -match 'Windows x64 only'
    }
    Assert-True 'Windows ARM64 exits with guidance' $ArmFailed 'unsupported architecture was accepted'
} finally {
    $env:TAKKA_RELEASE_BASE_URL = $null
    $env:TAKKA_INSTALLER_TESTING = $null
    $env:TAKKA_TEST_ARCHITECTURE = $null
    $env:TAKKA_INIT_LOG = $null
    if (Test-Path -LiteralPath $TestRoot) {
        Remove-Item -LiteralPath $TestRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "`n$Passed passed; $Failed failed"
if ($Failed -ne 0) { exit 1 }
