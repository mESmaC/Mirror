[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('mirror-lane-entry-' + [guid]::NewGuid().ToString('N'))
$fixtureScripts = Join-Path $fixtureRoot 'scripts'
New-Item -ItemType Directory -Path $fixtureScripts | Out-Null
$cases = 0
$global:MirrorLaneFixtureCargoCalled = $false
$global:MirrorLaneFixtureCargoAllowed = $false
$global:MirrorLaneFixtureCargoArguments = @()
function cargo {
    $global:MirrorLaneFixtureCargoCalled = $true
    if (-not $global:MirrorLaneFixtureCargoAllowed) { throw 'Unexpected Cargo invocation in rejected-argument fixture.' }
    $global:MirrorLaneFixtureCargoArguments = @($args)
    $global:LASTEXITCODE = 0
}
try {
    # Invoke the real packaging entry with a synthetic build, so no compiler,
    # disk image, VM or shared lease is involved in this host-only fixture.
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'package.ps1') -Destination $fixtureScripts
    [IO.File]::WriteAllText((Join-Path $fixtureScripts 'common.ps1'), "Set-StrictMode -Version Latest`n`$ErrorActionPreference='Stop'`n")
    $stub = @'
param([switch]$Release)
$root = Split-Path -Parent $PSScriptRoot
$build = Join-Path $root 'build'
New-Item -ItemType Directory -Path $build -Force | Out-Null
[IO.File]::WriteAllText((Join-Path $build 'build.called'), 'called')
[IO.File]::WriteAllText((Join-Path $build 'mirror.img'), 'new-image')
[IO.File]::WriteAllText((Join-Path $build 'mirror.vmdk'), 'new-descriptor')
'@
    [IO.File]::WriteAllText((Join-Path $fixtureScripts 'build.ps1'), $stub)
    $dist = Join-Path $fixtureRoot 'build/dist'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    $manifest = Join-Path $dist 'mirror.package.json'
    [IO.File]::WriteAllText($manifest, '{"old":"valid"}')
    & (Join-Path $fixtureScripts 'package.ps1')
    if (Test-Path -LiteralPath $manifest) { throw 'Ordinary packaging retained stale lane provenance.' }
    if ([IO.File]::ReadAllText((Join-Path $dist 'mirror.img')) -cne 'new-image' -or [IO.File]::ReadAllText((Join-Path $dist 'mirror.vmdk')) -cne 'new-descriptor') {
        throw 'Ordinary packaging failed its synthetic generation copy.'
    }
    $cases++

    # Fail the second publication after the first succeeds. Old provenance
    # must already be gone; a mixed pair is never a valid lane package.
    [IO.File]::WriteAllText($manifest, '{"old":"valid"}')
    $descriptor = Join-Path $dist 'mirror.vmdk'
    $held = [IO.File]::Open($descriptor, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $failed = $false
    try { & (Join-Path $fixtureScripts 'package.ps1') }
    catch { $failed = $true }
    finally { $held.Dispose() }
    if (-not $failed -or (Test-Path -LiteralPath $manifest)) { throw 'Partial publication retained valid provenance.' }
    $cases++

    # Check the actual thin dispatch with paths containing spaces. Cargo itself
    # remains synthetic; no compiler or real lease is started by these fixtures.
    $global:MirrorLaneFixtureCargoAllowed = $true
    foreach ($mode in @('debug', 'release', 'recover')) {
        $entryArguments = @{ Lane = $true; LaneRoot = (Join-Path $fixtureRoot 'lane with spaces'); LockOwner = 'fixture/task'; LaneSlot = 'qemu' }
        if ($mode -eq 'recover') { $entryArguments.Recover = 'package-123' }
        if ($mode -eq 'release') { $entryArguments.Release = $true }
        & (Join-Path $fixtureScripts 'package.ps1') @entryArguments
        $actual = $global:MirrorLaneFixtureCargoArguments
        $separator = [Array]::IndexOf($actual, '--')
        if ($separator -le 0) { throw 'Lane entry omitted the Cargo/controller argument separator.' }
        $hostArguments = @($actual[0..($separator - 1)])
        if (@($hostArguments | Where-Object { $_ -ceq '--release' }).Count -ne 1) { throw "Lane $mode must use an optimized host controller before '--'." }
        $expected = @('package', '--checkout', $fixtureRoot, '--lane-root', $entryArguments.LaneRoot, '--owner', 'fixture/task')
        if ($mode -eq 'recover') { $expected += @('--recover','package-123') }
        if ($mode -eq 'release') { $expected += '--release' }
        $expected += @('--slot', 'qemu')
        if (($actual[($separator + 1)..($actual.Length - 1)] -join "`n") -cne ($expected -join "`n") -or -not ($hostArguments -contains '--locked') -or -not ($hostArguments -contains '--offline')) { throw 'Lane entry changed the bounded controller argument array or guest profile.' }
        $cases++
    }
    $global:MirrorLaneFixtureCargoAllowed = $false
    foreach ($arguments in @(
        @{ Lane = $true; Perf = 'boot'; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task' },
        @{ Lane = $true; LockOwner = 'fixture/task' },
        @{ LaneRoot = $fixtureRoot },
        @{ Lane = $true; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; Recover = 'package-1'; Release = $true },
        @{ Lane = $true; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; Recover = '' },
        @{ Lane = $true; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; CargoHome = ' ' },
        @{ Lane = $true; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; LimineArchive = '' },
        @{ Recover = '' },
        @{ LaneSlot = 'qemu' },
        @{ Lane = $true; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; LaneSlot = 'qemu3' }
    )) {
        $buildCalled = Join-Path $fixtureRoot 'build/build.called'
        Remove-Item -LiteralPath $buildCalled -ErrorAction SilentlyContinue
        $global:MirrorLaneFixtureCargoCalled = $false
        $failed = $false
        try { & (Join-Path $fixtureScripts 'package.ps1') @arguments }
        catch { $failed = $true }
        if (-not $failed) { throw 'Contradictory lane entry arguments were accepted.' }
        if ($global:MirrorLaneFixtureCargoCalled -or (Test-Path -LiteralPath $buildCalled)) { throw 'Rejected arguments reached build/controller dispatch.' }
        $cases++
    }
    foreach ($arguments in @(
        @{ Vmx = 'absent.vmx'; LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; Case = 'boot' },
        @{ LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; Recover = 'run-1' },
        @{ LaneRoot = $fixtureRoot; LockOwner = 'fixture/task'; Recover = 'run-1'; PowerOff = $true }
    )) {
        $message = ''
        try { & (Join-Path $PSScriptRoot 'vmrun.ps1') @arguments }
        catch { $message = $_.Exception.Message }
        if ($message -notmatch 'disabled together pending 0015d D11') { throw "VM entry did not fail at the shared D11 gate: $message" }
        $cases++
    }
    Write-Host "All $cases host lane PowerShell entry fixtures passed (synthetic files only)."
}
finally {
    $resolved = (Resolve-Path -LiteralPath $fixtureRoot).Path
    $temporaryRoot = (Resolve-Path -LiteralPath ([IO.Path]::GetTempPath())).Path.TrimEnd('\')
    if ([IO.Path]::GetDirectoryName($resolved) -cne $temporaryRoot -or [IO.Path]::GetFileName($resolved) -notmatch '^mirror-lane-entry-[0-9a-f]{32}$') {
        throw 'Refusing to remove an unexpected fixture root.'
    }
    foreach ($entry in @(Get-Item -LiteralPath $resolved -Force) + @(Get-ChildItem -LiteralPath $resolved -Recurse -Force)) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Fixture cleanup refuses a reparse point.' }
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
