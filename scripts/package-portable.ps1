<#
.SYNOPSIS
    Build Handy MaxLogic and package it as a portable Windows x64 folder and zip.

.DESCRIPTION
    Runs `bun run tauri build --no-bundle` (no installer, no updater artifacts,
    no code signing), then assembles `<OutputDir>\Handy MaxLogic\` with:

      handy-maxlogic.exe       release executable
      *.dll                    DLLs next to the built exe (transcribe-cpp/ggml,
                               DirectML for ONNX Runtime), src-tauri\transcribe-libs,
                               and the app-local VC++ runtime when Visual Studio
                               redistributables are found. Every DLL the binaries
                               import must be in the folder or in System32.
      resources\               bundled resources (VAD model, sounds, tray icons)
      portable                 marker that keeps all data in Data\ next to the exe
      README-portable.txt      usage, data location, identity of this build
      LICENSE                  upstream MIT license (attribution)

    The folder is zipped and the script prints the zip path, size, SHA-256 and
    the source commit plus a hash of any uncommitted diff. It never starts the
    application.

.PARAMETER OutputDir
    Where the folder and zip are written. Defaults to artifacts\portable.

.PARAMETER CargoTargetDir
    Optional CARGO_TARGET_DIR for the release build.

.PARAMETER SkipBuild
    Package an existing release build instead of building.

.PARAMETER SkipDllCheck
    Skip the dumpbin check that every imported DLL is packaged. Without this
    switch a missing dumpbin.exe (Visual Studio C++ tools) fails the script.

.EXAMPLE
    pwsh -File scripts/package-portable.ps1
#>
[CmdletBinding()]
param(
    [string]$OutputDir,
    [string]$CargoTargetDir,
    [switch]$SkipBuild,
    [switch]$SkipDllCheck
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$productName = 'Handy MaxLogic'
$exeName = 'handy-maxlogic.exe'
# Must match portable.rs: the marker content that enables portable mode.
$portableMagic = 'Handy Portable Mode'

$repoRoot = Split-Path -Parent $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'
if (-not $OutputDir) { $OutputDir = Join-Path $repoRoot 'artifacts\portable' }

function Get-VisualStudioRoot {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $root = & $vswhere -latest -products * -property installationPath
    if ($LASTEXITCODE -ne 0 -or -not $root) { return $null }
    return $root
}

function Get-VcRuntimeDll {
    param([string]$VsRoot)
    if (-not $VsRoot) { return @() }
    $versionFile = Join-Path $VsRoot 'VC\Auxiliary\Build\Microsoft.VCRedistVersion.default.txt'
    if (-not (Test-Path $versionFile)) { return @() }
    $archDir = Join-Path $VsRoot ("VC\Redist\MSVC\{0}\x64" -f (Get-Content $versionFile).Trim())
    if (-not (Test-Path $archDir)) { return @() }
    $dirs = foreach ($filter in 'Microsoft.VC*.CRT', 'Microsoft.VC*.OpenMP') {
        Get-ChildItem $archDir -Directory -Filter $filter | Sort-Object Name | Select-Object -Last 1
    }
    # Same selection as build.rs (stage_vc_runtime_dlls) uses in CI.
    return @($dirs | Where-Object { $_ } | ForEach-Object {
            Get-ChildItem $_.FullName -Filter '*.dll' |
                Where-Object { $_.Name -match '^(msvcp140|vcruntime140|vcomp140)' }
        })
}

function Copy-Dll {
    param([System.IO.FileInfo[]]$Dll, [string]$Destination)
    foreach ($file in $Dll) {
        # Cargo places some DLLs (DirectML.dll from ort) as symlinks.
        $source = if ($file.LinkTarget) { $file.ResolveLinkTarget($true).FullName } else { $file.FullName }
        Copy-Item $source (Join-Path $Destination $file.Name) -Force
    }
}

function Assert-ImportsResolved {
    param([string]$Folder, [string]$VsRoot)
    $dumpbin = if ($VsRoot) {
        Get-ChildItem (Join-Path $VsRoot 'VC\Tools\MSVC') -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
            Where-Object FullName -like '*\Hostx64\x64\*' | Select-Object -First 1
    }
    if (-not $dumpbin) {
        throw 'dumpbin.exe not found, so the DLL import check cannot run. Install the Visual Studio C++ build tools, or pass -SkipDllCheck to package without the check.'
    }
    $system32 = [Environment]::GetFolderPath('System')
    $binaries = Get-ChildItem $Folder -File | Where-Object Extension -in '.exe', '.dll'
    $missing = foreach ($binary in $binaries) {
        foreach ($line in (& $dumpbin.FullName /nologo /dependents $binary.FullName)) {
            $name = $line.Trim()
            # Dependency lines hold a bare file name; headers hold paths.
            if ($name -notmatch '^[\w.-]+\.dll$' -or $name -like 'api-ms-win-*' -or $name -like 'ext-ms-*') { continue }
            if (-not (Test-Path (Join-Path $Folder $name)) -and -not (Test-Path (Join-Path $system32 $name))) {
                "$name (needed by $($binary.Name))"
            }
        }
    }
    if ($missing) { throw "Imported DLLs missing from the package: $($missing -join ', ')" }
}

function Invoke-Native {
    param([string]$FilePath, [string[]]$ArgumentList)
    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath $($ArgumentList -join ' ') failed with exit code $LASTEXITCODE"
    }
}

$config = Get-Content (Join-Path $tauriDir 'tauri.conf.json') -Raw | ConvertFrom-Json
if ($config.productName -ne $productName) {
    throw "tauri.conf.json productName is '$($config.productName)', expected '$productName'"
}
$version = $config.version

Push-Location $repoRoot
try {
    $commit = (git rev-parse HEAD).Trim()
    $shortCommit = $commit.Substring(0, 7)
    $diffFile = New-TemporaryFile
    try {
        Invoke-Native git @('diff', 'HEAD', '--binary', "--output=$($diffFile.FullName)")
        $dirty = (Get-Item $diffFile.FullName).Length -gt 0
        $diffHash = if ($dirty) { (Get-FileHash $diffFile.FullName -Algorithm SHA256).Hash.ToLowerInvariant() } else { '' }
    }
    finally {
        Remove-Item $diffFile.FullName -Force
    }
    $untracked = @(git ls-files --others --exclude-standard)

    if ($CargoTargetDir) { $env:CARGO_TARGET_DIR = $CargoTargetDir }
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $tauriDir 'target' }

    if (-not $SkipBuild) {
        if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
            $vsRoot = Get-VisualStudioRoot
            $cmakeBin = if ($vsRoot) { Join-Path $vsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin' }
            if ($cmakeBin -and (Test-Path $cmakeBin)) {
                $env:PATH = "$cmakeBin;$env:PATH"
                Write-Output "Using Visual Studio CMake: $cmakeBin"
            }
        }
        Write-Output "Building $productName $version (release, no bundle)..."
        Invoke-Native bun @('run', 'tauri', 'build', '--no-bundle')
    }

    $builtExe = Join-Path $targetDir 'release\handy.exe'
    if (-not (Test-Path $builtExe)) { throw "Release executable not found: $builtExe" }
    $vadModel = Join-Path $tauriDir 'resources\models\silero_vad_v4.onnx'
    if (-not (Test-Path $vadModel)) {
        throw "Missing $vadModel. Download it as described in AGENTS.md (Model Setup)."
    }
    $runtimeDlls = @(Get-ChildItem (Join-Path $tauriDir 'transcribe-libs') -Filter '*.dll' -ErrorAction SilentlyContinue)
    if ($runtimeDlls.Count -eq 0) { throw 'src-tauri\transcribe-libs contains no DLLs; run the release build first.' }

    New-Item -ItemType Directory -Force $OutputDir | Out-Null
    $packageDir = Join-Path $OutputDir $productName
    if (Test-Path $packageDir) { Remove-Item $packageDir -Recurse -Force }
    New-Item -ItemType Directory $packageDir | Out-Null

    Copy-Item $builtExe (Join-Path $packageDir $exeName)
    # DLLs next to the built exe (ggml/transcribe, DirectML for ONNX Runtime),
    # then the staged bundle libraries tauri.windows.conf.json maps to '.'.
    Copy-Dll @(Get-ChildItem (Split-Path -Parent $builtExe) -Filter '*.dll') $packageDir
    Copy-Dll $runtimeDlls $packageDir
    $vcDlls = @(Get-VcRuntimeDll (Get-VisualStudioRoot))
    $vcDlls | Where-Object { -not (Test-Path (Join-Path $packageDir $_.Name)) } | Copy-Item -Destination $packageDir
    if ($vcDlls.Count -eq 0) {
        Write-Warning 'VC++ runtime redistributable not found; the package relies on an installed Microsoft Visual C++ runtime.'
    }
    Copy-Item (Join-Path $tauriDir 'resources') (Join-Path $packageDir 'resources') -Recurse
    Copy-Item (Join-Path $repoRoot 'LICENSE') (Join-Path $packageDir 'LICENSE')
    Set-Content -Path (Join-Path $packageDir 'portable') -Value $portableMagic -NoNewline -Encoding utf8NoBOM
    if ($SkipDllCheck) {
        Write-Warning 'DLL import check skipped (-SkipDllCheck): the package may miss a DLL.'
    }
    else {
        Assert-ImportsResolved $packageDir (Get-VisualStudioRoot)
    }

    $sourceState = if ($dirty -or $untracked.Count -gt 0) {
        "commit $commit plus uncommitted changes (git diff HEAD sha256 $diffHash, $($untracked.Count) untracked files)"
    }
    else { "commit $commit (clean)" }
    $readme = @"
$productName $version - portable build for Windows x64
Source: $sourceState
Built: $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd HH:mm')) UTC

$productName is a personal fork of Handy (https://github.com/cjpais/Handy) by
CJ Pais and contributors, released under the MIT license (see LICENSE). Fork:
https://github.com/MaxLogic/Handy

Running
- Start $exeName. Keep the folder together: the DLLs and resources\ must stay
  next to the executable.
- The 'portable' file keeps all settings, models, history, recordings, logs and
  the WebView2 cache in Data\ inside this folder. Deleting 'portable' makes the
  app use %APPDATA%\eu.maxlogic.handy instead.
- It runs alongside upstream Handy: separate identifier (eu.maxlogic.handy),
  separate data and its own single-instance lock. Do not bind the same global
  shortcuts in both apps.
- Remote control: "$exeName" --toggle-transcription, --toggle-post-process,
  --cancel (see the project README for all flags).

First run
- If upstream Handy data exists in %APPDATA%\com.pais.handy, you are asked
  whether to copy settings and model files (history optional) into Data\.
  Upstream data is only read. "Not now" asks again on the next start;
  "Don't import" is final (copy files by hand later if needed). An import
  with failed items is offered again on the next start. Settings and
  history that would replace this app's own start unchecked.
- Both apps use the same default shortcuts. Close the original Handy or change
  its shortcuts before using $productName, or one key press records and
  pastes twice.

Updates
- This build never checks for or installs updates. To upgrade, replace the
  files of this folder except Data\ with a newer package. Back up
  Data\settings_store.json before trying an older build.

Requirements
- Windows 10/11 x64, WebView2 runtime, a Vulkan-capable GPU driver for GPU
  transcription (CPU fallback otherwise).
"@
    Set-Content -Path (Join-Path $packageDir 'README-portable.txt') -Value $readme -Encoding utf8NoBOM

    $dirtyTag = if ($dirty -or $untracked.Count -gt 0) { '-dirty' } else { '' }
    $zipPath = Join-Path $OutputDir ("Handy-MaxLogic-{0}-{1}{2}-portable-x64.zip" -f $version, $shortCommit, $dirtyTag)
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    Compress-Archive -Path $packageDir -DestinationPath $zipPath -CompressionLevel Optimal

    $zip = Get-Item $zipPath
    $hash = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -Path "$zipPath.sha256" -Value "$hash  $($zip.Name)" -Encoding ascii
    $exeHash = (Get-FileHash (Join-Path $packageDir $exeName) -Algorithm SHA256).Hash.ToLowerInvariant()

    Write-Output ''
    Write-Output "Package folder: $packageDir"
    Write-Output "Artifact:       $zipPath"
    Write-Output ("Size:           {0} bytes ({1:N1} MB)" -f $zip.Length, ($zip.Length / 1MB))
    Write-Output "SHA-256:        $hash"
    Write-Output "Exe SHA-256:    $exeHash"
    Write-Output "Source commit:  $commit"
    if ($dirty) { Write-Output "Diff SHA-256:   $diffHash (git diff HEAD --binary)" }
    if ($untracked.Count -gt 0) { Write-Output "Untracked:      $($untracked.Count) files (not in the diff hash)" }
}
finally {
    Pop-Location
}
