[CmdletBinding()]
param(
    [switch]$Release,
    [string]$Perf,
    [switch]$Lane,
    [string]$LaneRoot,
    [string]$LockOwner,
    [string]$Recover,
    [string]$CargoHome,
    [string]$LimineArchive,
    [ValidateSet('qemu', 'qemu2')][string]$LaneSlot
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$laneOptionNames = @('LaneRoot', 'LockOwner', 'Recover', 'CargoHome', 'LimineArchive', 'LaneSlot')
foreach ($option in $laneOptionNames) {
    if ($PSBoundParameters.ContainsKey($option) -and [string]::IsNullOrWhiteSpace([string]$PSBoundParameters[$option])) {
        throw "-$option requires a non-empty value."
    }
}
if ($Lane) {
    if ($PSBoundParameters.ContainsKey('Perf')) { throw 'Lane packages use the tokenless default flavor.' }
    if ([string]::IsNullOrWhiteSpace($LaneRoot) -or [string]::IsNullOrWhiteSpace($LockOwner)) {
        throw 'Lane requires -LaneRoot and -LockOwner nick/task.'
    }
    if ($Recover -and $Release) { throw 'Recover cannot select a build profile.' }
    $laneArguments = @('run', '--release', '--locked', '--offline', '--target', 'x86_64-pc-windows-msvc', '--manifest-path', (Join-Path $repositoryRoot 'tools/perf-probe/Cargo.toml'), '--bin', 'perf-probe', '--', 'package', '--checkout', $repositoryRoot, '--lane-root', $LaneRoot, '--owner', $LockOwner)
    if ($Release) { $laneArguments += '--release' }
    if ($Recover) { $laneArguments += @('--recover', $Recover) }
    if ($CargoHome) { $laneArguments += @('--cargo-home', $CargoHome) }
    if ($LimineArchive) { $laneArguments += @('--limine-archive', $LimineArchive) }
    if ($LaneSlot) { $laneArguments += @('--slot', $LaneSlot) }
    # Optimize the host controller's bounded image hashing in every guest profile
    # and recovery. The guest -Release choice is forwarded only after '--'.
    # Host-only bootstrap; the Rust controller acquires its own machine lease.
    Push-Location -LiteralPath $repositoryRoot
    try {
        & cargo @laneArguments
        if ($LASTEXITCODE -ne 0) { throw "Lane package controller exited $LASTEXITCODE; consult the printed LaneRoot record." }
    }
    finally { Pop-Location }
    return
}
if (@($laneOptionNames | Where-Object { $PSBoundParameters.ContainsKey($_) }).Count -ne 0) {
    throw 'Lane options require -Lane.'
}
$profile = if ($Release) { 'release' } else { 'debug' }
$perfToken = if ($PSBoundParameters.ContainsKey('Perf')) { ConvertTo-MirrorPerfToken -Items $Perf } else { $null }

# VMs boot copies in build/dist/, so a later build or test run that rewrites
# build/mirror.img never changes a VM's disk (spec 0015a). Packages use the
# default kernel flavor, the one for daily use (ledger S7a).
& (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release
$distDirectory = Join-Path $repositoryRoot 'build/dist'
if ($null -ne $perfToken) {
    $distDirectory = Join-Path $distDirectory 'perf'
    New-Item -ItemType Directory -Path $distDirectory -Force | Out-Null
    # This invocation owns a fresh private tree. The daily staged config and
    # build/dist image remain untouched; only this copy gets the opt-in token.
    $stageDirectory = Join-Path $distDirectory ('stage-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stageDirectory | Out-Null
    try {
        $dailyEsp = Join-Path $repositoryRoot 'build/esp'
        $perfEsp = Join-Path $stageDirectory 'esp'
        Copy-Item -LiteralPath $dailyEsp -Destination $perfEsp -Recurse
        $dailyConfigPath = Join-Path $dailyEsp 'limine.conf'
        $dailyConfig = [IO.File]::ReadAllText($dailyConfigPath)
        if ($dailyConfig -match 'mirror\.perf=' -or $dailyConfig -match '(?m)^\s*cmdline\s*:') {
            throw 'The daily Limine template must be tokenless and have no cmdline override.'
        }
        if ([regex]::Matches($dailyConfig, '(?m)^/').Count -ne 1) {
            throw 'Performance packaging requires exactly one Limine boot entry.'
        }
        $perfConfig = [regex]::Replace($dailyConfig, '(?m)^\s*resolution\s*:[^\r\n]*\r?\n?', '')
        $perfConfig = $perfConfig.TrimEnd() + "`n    resolution: 1920x1080`n    cmdline: $perfToken`n"
        if ([regex]::Matches($perfConfig, 'mirror\.perf=').Count -ne 1) {
            throw 'Performance packaging must supply exactly one performance token.'
        }
        [IO.File]::WriteAllText((Join-Path $perfEsp 'limine.conf'), $perfConfig, [Text.UTF8Encoding]::new($false))
        $programs = @('kernel.elf', 'init.elf', 'desktop.elf', 'app.elf')
        $dailyHashes = @{}
        foreach ($name in $programs) {
            $dailyHashes[$name] = (Get-FileHash -LiteralPath (Join-Path $dailyEsp $name) -Algorithm SHA256).Hash
            if ((Get-FileHash -LiteralPath (Join-Path $perfEsp $name) -Algorithm SHA256).Hash -ne $dailyHashes[$name]) {
                throw "Performance staging changed $name."
            }
        }
        $imageTool = Join-Path $repositoryRoot 'tools/mirror-image/target/x86_64-pc-windows-msvc/release/mirror-image.exe'
        $perfImage = Join-Path $stageDirectory 'mirror.img'
        $perfVmdk = Join-Path $stageDirectory 'mirror.vmdk'
        & $imageTool disk --esp $perfEsp --out $perfImage --vmdk $perfVmdk --perf
        if ($LASTEXITCODE -ne 0) { throw "Performance image creation failed with exit code $LASTEXITCODE." }
        & $imageTool verify --esp $perfEsp $perfImage
        if ($LASTEXITCODE -ne 0) { throw "Performance image verification failed with exit code $LASTEXITCODE." }
        # Verification reads the embedded files back against the private ESP;
        # the hashes then prove both packages use the same compiled programs.
        foreach ($name in $programs) {
            foreach ($esp in @($dailyEsp, $perfEsp)) {
                if ((Get-FileHash -LiteralPath (Join-Path $esp $name) -Algorithm SHA256).Hash -ne $dailyHashes[$name]) {
                    throw "The daily/performance $name hash changed during packaging."
                }
            }
        }
        if ([IO.File]::ReadAllText($dailyConfigPath) -cne $dailyConfig) {
            throw 'The daily Limine config changed during performance packaging.'
        }
        # Invalidate provenance before replacing either member of this generation.
        $packageRecord = Join-Path $distDirectory 'mirror.package.json'
        if (Test-Path -LiteralPath $packageRecord) { Remove-Item -LiteralPath $packageRecord -Force }
        # Both artifacts are already complete and verified on the same volume.
        foreach ($name in @('mirror.img', 'mirror.vmdk')) {
            Move-Item -LiteralPath (Join-Path $stageDirectory $name) -Destination (Join-Path $distDirectory $name) -Force
        }
        Write-Host "Mirror $profile default performance disk packaged in ${distDirectory}: $perfToken, requested 1920x1080; kernel/module SHA256 values match daily staging."
    }
    finally {
        # Resolve and check the exact owned directory before recursive cleanup.
        $resolvedDist = (Resolve-Path -LiteralPath $distDirectory).Path
        $resolvedStage = (Resolve-Path -LiteralPath $stageDirectory).Path
        if ([IO.Path]::GetDirectoryName($resolvedStage) -cne $resolvedDist) {
            throw 'Refusing to remove a performance stage outside the performance package directory.'
        }
        foreach ($entry in @(Get-Item -LiteralPath $resolvedStage -Force) + @(Get-ChildItem -LiteralPath $resolvedStage -Recurse -Force)) {
            if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing to remove a performance stage containing a link: $($entry.FullName)"
            }
        }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
    return
}
New-Item -ItemType Directory -Path $distDirectory -Force | Out-Null
$packageRecord = Join-Path $distDirectory 'mirror.package.json'
if (Test-Path -LiteralPath $packageRecord) { Remove-Item -LiteralPath $packageRecord -Force }
# Copy the image before the descriptor that names it. Each copy goes to a
# temporary name first, so a failed copy never leaves a partial artifact.
foreach ($name in @('mirror.img', 'mirror.vmdk')) {
    $source = Join-Path $repositoryRoot "build/$name"
    $destination = Join-Path $distDirectory $name
    $temporary = "$destination.tmp"
    try {
        Copy-Item -LiteralPath $source -Destination $temporary -Force
        Move-Item -LiteralPath $temporary -Destination $destination -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}
Write-Host "Mirror $profile default disk image packaged in ${distDirectory}: mirror.img, mirror.vmdk"
