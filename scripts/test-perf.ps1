[CmdletBinding()]
param()

. (Join-Path $PSScriptRoot 'common.ps1')
if ($PSVersionTable.PSVersion.Major -ne 5 -or $PSVersionTable.PSVersion.Minor -ne 1) {
    throw 'Run these host fixtures with Windows PowerShell 5.1.'
}
$checks = 0
function Assert-Rejected {
    param([scriptblock]$Action)
    $rejected = $false
    try { & $Action }
    catch { $rejected = $true }
    if (-not $rejected) { throw 'A corrupt perf fixture was accepted.' }
    $script:checks++
}

foreach ($items in @('boot', 'mem', '1', 'boot,1,burn', 'agent,telemetry,flip,1,burn,boot')) {
    if ((ConvertTo-MirrorPerfToken $items) -cne "mirror.perf=$items") { throw 'Perf host grammar changed the supplied item order.' }
    $checks++
}
foreach ($items in @('', 'BOOT', 'boot,', ',mem', 'mem,,boot', '1,1', '1,mem', 'burn', 'boot,unknown', 'boot mem', "boot`tmem", "boot`nmem", ('boot' + [char]0x0b + 'mem'), ('m' + [char]0xe9 + 'm'), ('boot,' + ('a' * 64)))) {
    Assert-Rejected { ConvertTo-MirrorPerfToken $items }
}
Assert-Rejected { ConvertTo-MirrorPerfToken $null }

$mem = @'
Mirror kernel 0.1.0 | x86_64
MIRROR_PERF_ON v=1 mem=1 input=0 burn=0
MIRROR_PERF_T event=clock ns=100 since_entry_ns=none
MIRROR_FRAMEBUFFER_READY width=1280 height=800
MIRROR_PERF_T event=display ns=200
MIRROR_PERF_T event=input ns=300
MIRROR_PERF_T event=userspace ns=400
MIRROR_PERF_MEM ns=450 available_kib=100 pool_kib=400
MIRROR_PERF_T event=frame ns=500
MIRROR_PERF_T event=shell_ready ns=600
MIRROR_PERF_MEM ns=10000000450 available_kib=96 pool_kib=400
MIRROR_PERF_MEM ns=20000000450 available_kib=92 pool_kib=400
'@
Assert-MirrorPerfMarkers -Serial $mem -Items mem -ExpectedMemSamples 3
$checks++
Assert-MirrorPerfMarkers -Serial (([char]27 + '[04;01H' + [char]27 + '[05;01H') + $mem) -Items mem -ExpectedMemSamples 3
$checks++
Assert-MirrorPerfMarkers -Serial ('limine: Loading module boot():/app.elf...' + [char]27 + '[04;01H' + [char]27 + '[05;01H' + $mem) -Items mem -ExpectedMemSamples 3
$checks++
Assert-MirrorPerfMarkers -Serial (($mem -replace '\r?\n', "`r`n")) -Items mem -ExpectedMemSamples 3
$checks++
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($mem.Replace('event=frame ns=500', "event=frame ns=500`rgarbage")) -Items mem }
# Family clocks are independent: buffered MEM need not globally follow the
# timestamps of the preceding milestone, but its own cadence remains exact.
Assert-MirrorPerfMarkers -Serial ($mem.Replace('event=shell_ready ns=600', 'event=shell_ready ns=10000001000')) -Items mem
$checks++
Assert-MirrorPerfMarkers -Serial ($mem.Replace('available_kib=100 pool_kib=400', 'available_kib=none pool_kib=none')) -Items mem
$checks++
foreach ($corrupt in @(
        $mem.Replace('v=1 mem=1 input=0 burn=0', 'v=1 input=0 mem=1 burn=0'),
        $mem.Replace('v=1 mem=1 input=0 burn=0', 'v=1 mem=1 input=0 burn=0 extra=1'),
        $mem.Replace('event=input ns=300', 'event=input ns=199'),
        $mem.Replace('event=input ns=300', 'event=bogus ns=300'),
        $mem.Replace('event=input ns=300', 'event=input ns=none'),
        $mem.Replace('event=clock ns=100 since_entry_ns=none', 'event=clock ns=100'),
        $mem.Replace('event=display ns=200', 'event=display ns=200 since_entry_ns=none'),
        $mem.Replace('event=display ns=200', 'event=display ns=18446744073709551616'),
        $mem.Replace('event=display ns=200', 'event=display ns=18446744073709551615'),
        $mem.Replace('available_kib=100 pool_kib=400', 'available_kib=404 pool_kib=400'),
        $mem.Replace('available_kib=100 pool_kib=400', 'available_kib=101 pool_kib=400'),
        $mem.Replace('available_kib=100 pool_kib=400', 'available_kib=none pool_kib=400'),
        $mem.Replace('ns=10000000450', 'ns=10000000449'),
        $mem.Replace('ns=20000000450', 'ns=10000000450'),
        $mem.Replace('MIRROR_PERF_T event=frame ns=500', "MIRROR_PERF_T event=frame ns=500`nMIRROR_PERF_T event=frame ns=500"),
        ($mem + "`nMIRROR_PERF_NEW field=1"),
        ($mem + "`nMIRROR_PERF_OFF reason=invalid"),
        ($mem + "`nMIRROR_PERF_INPUT sequence=0"),
        ($mem + "`nMIRROR_PERF_WINDOW state=done"),
        ($mem + "`nMIRROR_PERF_BURN active=0"),
        ($mem.Replace('MIRROR_PERF_T event=userspace ns=400', 'MIRROR_PERF_T event=frame ns=400')),
        ($mem.Replace('MIRROR_PERF_MEM ns=20000000450', "MIRROR_CONSOLE_READY input=ps2 display=1`nMIRROR_PERF_MEM ns=20000000450")))) {
    Assert-Rejected { Assert-MirrorPerfMarkers -Serial $corrupt -Items mem }
}
Assert-Rejected { Assert-MirrorPerfMarkers -Serial $mem -Items boot }
Assert-Rejected { Assert-MirrorPerfMarkers -Serial $mem -Items mem -ExpectedMemSamples 2 }
Assert-Rejected { Assert-MirrorNoPerf $mem }
Assert-MirrorNoPerf "Mirror kernel 0.1.0 | x86_64`nMIRROR_BOOT_OK`n"
$checks++

$smoke = @'
Mirror kernel 0.1.0 | x86_64
MIRROR_PERF_ON v=1 mem=1 input=1 burn=1
MIRROR_TRACE_ON
MIRROR_PERF_T event=clock ns=100 since_entry_ns=none
MIRROR_FRAMEBUFFER_READY width=1280 height=800
MIRROR_PERF_T event=display ns=200
MIRROR_PERF_T event=input ns=300
MIRROR_PERF_SKIPPED item=input reason=smoke
MIRROR_PERF_SKIPPED item=mem reason=smoke
MIRROR_PERF_SKIPPED item=burn reason=smoke
'@
Assert-MirrorPerfMarkers -Serial $smoke -Items 'boot,1,burn' -Flavor Smoke
$checks++
$noClock = @'
Mirror kernel 0.1.0 | x86_64
MIRROR_PERF_ON v=1 mem=1 input=1 burn=1
MIRROR_PERF_T event=clock ns=none since_entry_ns=none
MIRROR_FRAMEBUFFER_READY width=1280 height=800
MIRROR_PERF_T event=display ns=none
MIRROR_PERF_T event=input ns=none
MIRROR_PERF_SKIPPED item=input reason=no_clock
MIRROR_PERF_SKIPPED item=mem reason=no_clock
MIRROR_PERF_SKIPPED item=burn reason=no_clock
MIRROR_PERF_T event=userspace ns=none
MIRROR_PERF_MEM ns=none available_kib=100 pool_kib=400
MIRROR_PERF_T event=frame ns=none
MIRROR_PERF_T event=shell_ready ns=none
'@
Assert-MirrorPerfMarkers -Serial $noClock -Items '1,burn' -WithoutHpet $true -ExpectedMemSamples 1
$checks++
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($noClock + "`nMIRROR_PERF_MEM ns=none available_kib=100 pool_kib=400") -Items '1,burn' -WithoutHpet $true }
$noSession = ($noClock -split "`n" | Where-Object { $_ -notmatch '^MIRROR_PERF_(?:T event=(?:userspace|frame|shell_ready)|MEM)' }) -join "`n"
Assert-MirrorPerfMarkers -Serial $noSession -Items '1,burn' -WithoutHpet $true -WithoutSession $true -ExpectedMemSamples 0
$checks++
$noDisplay = ($noSession -split "`n" | Where-Object { $_ -notmatch '^MIRROR_(?:FRAMEBUFFER_READY|PERF_T event=display)' }) -join "`n"
Assert-MirrorPerfMarkers -Serial ($noDisplay + "`nMIRROR_DESKTOP_SKIPPED reason=missing_display_or_input") -Items '1,burn' -WithoutHpet $true -WithoutDisplay $true
$checks++
$noDevices = $noSession.Replace('ns=none since_entry_ns=none', 'ns=100 since_entry_ns=none').Replace('event=display ns=none', 'event=display ns=200').Replace('event=input ns=none', 'event=input ns=300').Replace('reason=no_clock', 'reason=no_session')
Assert-MirrorPerfMarkers -Serial $noDevices -Items '1,burn' -WithoutSession $true
$checks++
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($noSession.Replace('reason=no_clock', 'reason=no_session')) -Items '1,burn' -WithoutHpet $true -WithoutSession $true }
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($noClock.Replace('item=input reason=no_clock', 'item=burn reason=no_clock').Replace('item=burn reason=no_clock', 'item=input reason=no_clock')) -Items '1,burn' -WithoutHpet $true }
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($noSession + "`nMIRROR_PERF_MEM ns=none available_kib=100 pool_kib=400") -Items '1,burn' -WithoutHpet $true -WithoutSession $true }
# Runtime unavailable skips must follow input and precede userspace, even when
# all records are otherwise correct and all three requested flags are set.
$unavailable = $mem.Replace('input=0 burn=0', 'input=1 burn=1').Replace('MIRROR_PERF_T event=userspace ns=400', "MIRROR_PERF_SKIPPED item=input reason=unavailable`nMIRROR_PERF_SKIPPED item=burn reason=unavailable`nMIRROR_PERF_T event=userspace ns=400")
Assert-MirrorPerfMarkers -Serial $unavailable -Items '1,burn'
$checks++
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($unavailable.Replace("MIRROR_PERF_SKIPPED item=burn reason=unavailable`nMIRROR_PERF_T event=userspace ns=400", "MIRROR_PERF_T event=userspace ns=400`nMIRROR_PERF_SKIPPED item=burn reason=unavailable")) -Items '1,burn' }
$prefix = "Mirror kernel 0.1.0 | x86_64`nMIRROR_PERF_ON v=1 mem=0 input=0 burn=0`nMIRROR_PERF_T event=clock ns=100 since_entry_ns=none"
Assert-MirrorPerfMarkers -Serial $prefix -Items boot -FatalEndpoint clock
$checks++
$invalid = "Mirror kernel 0.1.0 | x86_64`nMIRROR_PERF_OFF reason=invalid"
Assert-MirrorPerfMarkers -Serial $invalid -Invalid
$checks++
Assert-Rejected { Assert-MirrorPerfMarkers -Serial ($invalid + "`nMIRROR_PERF_T event=clock ns=none since_entry_ns=none") -Invalid }

# Run exact public script copies with an isolated fail-if-reached build/boot
# seam. This proves malformed public arguments are rejected before operations,
# without launching a compiler or VM if a validation regression is introduced.
$fixtureDirectory = Join-Path ([IO.Path]::GetTempPath()) ('mirror-perf-host-' + [guid]::NewGuid().ToString('N'))
$fixtureFiles = @('common.ps1', 'run.ps1', 'capture.ps1', 'package.ps1', 'build.ps1')
New-Item -ItemType Directory -Path $fixtureDirectory | Out-Null
try {
    foreach ($name in $fixtureFiles | Where-Object { $_ -ne 'build.ps1' }) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $fixtureDirectory $name)
    }
    [IO.File]::AppendAllText((Join-Path $fixtureDirectory 'common.ps1'), "`nfunction Get-MirrorQemu { throw 'BUILD_OR_BOOT_REACHED' }`n")
    [IO.File]::WriteAllText((Join-Path $fixtureDirectory 'build.ps1'), "throw 'BUILD_OR_BOOT_REACHED'")
    foreach ($name in @('run.ps1', 'capture.ps1', 'package.ps1')) {
        $path = Join-Path $fixtureDirectory $name
        foreach ($items in @('', 'bogus', 'burn', 'mem,1', 'boot,boot', "boot`nmem")) {
            $message = $null
            try { & $path -Perf $items }
            catch { $message = $_.Exception.Message }
            if ($null -eq $message -or $message -eq 'BUILD_OR_BOOT_REACHED' -or $message -notmatch '(?i)perf') { throw "Public $name failed to reject malformed Perf before build/boot: $message" }
            $checks++
        }
        $message = $null
        try { & $path -Perf }
        catch { $message = $_.Exception.Message }
        if ($null -eq $message -or $message -eq 'BUILD_OR_BOOT_REACHED') { throw "Bare -Perf reached an operation in $name." }
        $checks++
    }
    foreach ($case in @(
            @{ Script = 'run.ps1'; Args = @{ Smoke = $true; Fault = 'PerfInvalid'; Perf = 'boot' } },
            @{ Script = 'capture.ps1'; Args = @{ Hygiene = $true; Perf = 'boot' } },
            @{ Script = 'capture.ps1'; Args = @{ Hygiene = $true; PerfCapture = $true } },
            @{ Script = 'capture.ps1'; Args = @{ PerfCapture = $true } },
            @{ Script = 'capture.ps1'; Args = @{ PerfScenario = 'Markers'; Perf = 'boot' } })) {
        $path = Join-Path $fixtureDirectory $case.Script
        $arguments = $case.Args
        $message = $null
        try { & $path @arguments }
        catch { $message = $_.Exception.Message }
        if ($null -eq $message -or $message -eq 'BUILD_OR_BOOT_REACHED' -or $message -notmatch '(?i)perf') { throw "Invalid perf option combination reached build/boot: $message" }
        $checks++
    }
}
finally {
    # Remove only the five known files, then the empty directory (no recursion).
    foreach ($name in $fixtureFiles) {
        $path = Join-Path $fixtureDirectory $name
        if (Test-Path -LiteralPath $path -PathType Leaf) { Remove-Item -LiteralPath $path -Force }
    }
    Remove-Item -LiteralPath $fixtureDirectory
}
Write-Host "All $checks Windows PowerShell 5.1 perf host fixtures passed."
