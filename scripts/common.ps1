Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-MirrorClock {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Smoke,
        [bool]$WithoutHpet,
        [string]$FaultMode = 'None'
    )

    $injected = $FaultMode -eq 'ClockActivation'
    if ($injected -and (-not $Smoke -or $WithoutHpet)) {
        throw 'Clock activation fault injection requires smoke with HPET present.'
    }
    $available = -not $WithoutHpet -and -not $injected
    $userCount = if ($Smoke) { 4 } else { 2 }
    $expected = @{
        MIRROR_ACPI_HPET_READY = [int](-not $WithoutHpet)
        MIRROR_ACPI_HPET_SKIPPED = [int]$WithoutHpet
        MIRROR_CLOCK_POLICY_OK = [int]$Smoke
        MIRROR_CLOCK_COUNTER_OK = [int]($Smoke -and $available)
        MIRROR_CLOCK_READY = [int]$available
        MIRROR_CLOCK_SKIPPED = [int](-not $available)
        MIRROR_CLOCK_INJECTED = [int]$injected
        MIRROR_CLOCK_OK = [int]($Smoke -and $available)
        MIRROR_CLOCK_TEST_SKIPPED = [int]($Smoke -and -not $available)
        MIRROR_USER_CLOCK_OK = $userCount
    }
    # Count raw names as well as complete records, so malformed duplicates or
    # a different clock source cannot silently satisfy a positive substring.
    foreach ($record in [regex]::Matches($Serial, '(?m)^(MIRROR_(?:CLOCK|ACPI_HPET|USER_CLOCK)_[^\s]*)[^\r\n]*')) {
        if (-not $expected.ContainsKey($record.Groups[1].Value)) { throw 'Unexpected clock marker.' }
    }
    foreach ($name in $expected.Keys) {
        if ([regex]::Matches($Serial, ('(?m)^{0}(?:\s|$)' -f $name)).Count -ne $expected[$name]) {
            throw "Clock marker count mismatch: $name."
        }
    }
    $timers = [uint64]0
    if ($WithoutHpet) {
        $discovery = [regex]::Matches($Serial, '(?m)^MIRROR_ACPI_HPET_SKIPPED reason=missing\r?$')
        if ($discovery.Count -ne 1) { throw 'Missing HPET must be reported by ACPI discovery.' }
    }
    else {
        $discovery = [regex]::Matches($Serial, '(?m)^MIRROR_ACPI_HPET_READY address=0x([0-9a-f]+) block_id=0x([0-9a-f]+) number=([0-9]+) minimum_tick=([0-9]+) protection=([0-9]+)\r?$')
        if ($discovery.Count -ne 1) { throw 'Missing or malformed ACPI HPET descriptor.' }
        $address = [Convert]::ToUInt64($discovery[0].Groups[1].Value, 16)
        $identity = [Convert]::ToUInt32($discovery[0].Groups[2].Value, 16)
        if ($address -eq 0 -or $address % 4096 -ne 0 -or $address -gt ([uint64]::MaxValue - 4096) -or
            ($identity -band 255) -eq 0 -or ($identity -band 8192) -eq 0 -or ($identity -band 16384) -ne 0 -or
            [uint64]$discovery[0].Groups[3].Value -gt 255 -or [uint64]$discovery[0].Groups[4].Value -gt 65535 -or
            [uint64]$discovery[0].Groups[5].Value -gt 2) {
            throw 'ACPI HPET address, identity, or descriptor fields are invalid.'
        }
        $timers = (($identity -shr 8) -band 31) + 1
    }
    $previous = $discovery[0].Index
    if ($Smoke) {
        $policy = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_POLICY_OK\r?$')
        if ($policy.Count -ne 1 -or $policy[0].Index -le $previous) { throw 'Clock policy checks must follow discovery and precede selection.' }
        $previous = $policy[0].Index
    }
    if ($injected) {
        $injection = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_INJECTED active=1 cleanup=1\r?$')
        if ($injection.Count -ne 1 -or $injection[0].Index -le $previous) { throw 'Clock activation injection must prove active-counter cleanup.' }
        $previous = $injection[0].Index
    }
    if ($Smoke -and $available) {
        $counter = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_COUNTER_OK width=64 rollover=1 reset=1\r?$')
        if ($counter.Count -ne 1 -or $counter[0].Index -le $previous) { throw 'Clock smoke must prove64-bit counter rollover and reset before publication.' }
        $previous = $counter[0].Index
    }
    if ($available) {
        $selection = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_READY source=hpet width=64 period_fs=([0-9]+) resolution_ns=([0-9]+) timers=([0-9]+) interrupts=0 legacy=0\r?$')
        if ($selection.Count -ne 1) { throw 'Missing or malformed HPET clock readiness.' }
        $period = [uint64]$selection[0].Groups[1].Value
        $resolution = [uint64]$selection[0].Groups[2].Value
        if ($period -lt 1000000 -or $period -gt 100000000 -or
            $resolution -ne [uint64][decimal]::Ceiling([decimal]$period / 1000000) -or
            [uint64]$selection[0].Groups[3].Value -ne $timers) {
            throw 'HPET period, rounded resolution, and comparator count must match the validated descriptor.'
        }
    }
    else {
        $reason = if ($injected) { 'injected_clock_failure' } else { 'no_hpet' }
        $selection = [regex]::Matches($Serial, ('(?m)^MIRROR_CLOCK_SKIPPED reason={0}\r?$' -f $reason))
        if ($selection.Count -ne 1) { throw 'Clock absence or activation cleanup requires its exact fallback reason.' }
    }
    if ($selection[0].Index -le $previous) { throw 'Clock selection precedes discovery, policy, or required cleanup.' }
    $previous = $selection[0].Index
    if ($Smoke) {
        if ($available) {
            $test = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_OK elapsed_ns=([0-9]+) irq_ticks=0 if_restored=0\r?$')
            if ($test.Count -ne 1 -or [uint64]$test[0].Groups[1].Value -lt 20000000) { throw 'Clock must advance at least 20 ms with IRQs disabled and ticks unchanged.' }
        }
        else {
            $test = [regex]::Matches($Serial, '(?m)^MIRROR_CLOCK_TEST_SKIPPED reason=unavailable\r?$')
            if ($test.Count -ne 1) { throw 'Unavailable clock must explicitly skip hardware elapsed-time testing.' }
        }
        if ($test[0].Index -le $previous) { throw 'Clock hardware check precedes backend selection.' }
        $previous = $test[0].Index
    }
    $state = [int]$available
    $users = [regex]::Matches($Serial, ('(?m)^MIRROR_USER_CLOCK_OK pid=([0-9]+) available={0}\r?$' -f $state))
    $message = if ($available) { 'checks passed' } else { 'unavailable' }
    $messages = [regex]::Matches($Serial, ('(?m)^user: Mirror init: clock {0}\\n\r?$' -f $message))
    if ($users.Count -ne $userCount -or $messages.Count -ne $userCount -or
        [regex]::Matches($Serial, '(?m)^user: Mirror init: clock(?:\s|$)').Count -ne $userCount) {
        throw 'Every init process must report the expected clock ABI path exactly once.'
    }
    $pids = [System.Collections.Generic.HashSet[uint64]]::new()
    for ($index = 0; $index -lt $userCount; $index++) {
        $pidValue = [uint64]$users[$index].Groups[1].Value
        if ($pidValue -eq 0 -or -not $pids.Add($pidValue) -or
            $messages[$index].Index -le $previous -or $users[$index].Index -le $messages[$index].Index) {
            throw 'Clock ABI evidence must follow initialization and use distinct positive process IDs.'
        }
    }
}

function Assert-MirrorTimer {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Smoke,
        [bool]$UseLegacyPic,
        [bool]$ForcePit,
        [string]$FaultMode = 'None'
    )

    $injected = $FaultMode -eq 'TimerCalibration'
    if ($injected -and (-not $Smoke -or $UseLegacyPic -or $ForcePit)) {
        throw 'Timer calibration fault injection requires smoke with automatic APIC timer selection.'
    }
    $calibrationRequired = -not $UseLegacyPic -and -not $ForcePit -and -not $injected
    $skipReason = if ($UseLegacyPic) { 'legacy_pic' } elseif ($ForcePit) { 'boot_policy' } elseif ($injected) { 'injected_calibration_failure' } else { $null }
    $readyCount = [regex]::Matches($Serial, '(?m)^MIRROR_TIMER_READY(?:\s|$)').Count
    $calibrationCount = [regex]::Matches($Serial, '(?m)^MIRROR_LAPIC_TIMER_CALIBRATED(?:\s|$)').Count
    $injectionCount = [regex]::Matches($Serial, '(?m)^MIRROR_LAPIC_TIMER_INJECTED(?:\s|$)').Count
    $skipCount = [regex]::Matches($Serial, '(?m)^MIRROR_LAPIC_TIMER_SKIPPED(?:\s|$)').Count
    $policyCount = [regex]::Matches($Serial, '(?m)^MIRROR_TIMER_POLICY_OK(?:\s|$)').Count
    $testCount = [regex]::Matches($Serial, '(?m)^MIRROR_TIMER_OK(?:\s|$)').Count
    if ($readyCount -ne 1 -or $calibrationCount -ne [int]$calibrationRequired -or
        $injectionCount -ne [int]$injected -or $skipCount -ne [int]($null -ne $skipReason)) {
        throw 'Timer selection must publish exactly one backend and the expected calibration/fallback records.'
    }
    $previous = -1
    $selectionStart = -1
    $reload = [uint64]0
    if ($calibrationRequired) {
        $pattern = '(?m)^MIRROR_LAPIC_TIMER_CALIBRATED samples=3 reference_hz=1193182 reference_count=23864 divide=16 elapsed_min=([0-9]+) elapsed_median=([0-9]+) elapsed_max=([0-9]+) reload=([0-9]+)\r?$'
        $calibration = [regex]::Matches($Serial, $pattern)
        if ($calibration.Count -ne 1) { throw 'Missing or malformed local APIC timer calibration.' }
        $minimum = [uint64]$calibration[0].Groups[1].Value
        $median = [uint64]$calibration[0].Groups[2].Value
        $maximum = [uint64]$calibration[0].Groups[3].Value
        $reload = [uint64]$calibration[0].Groups[4].Value
        if ($minimum -eq 0 -or $minimum -gt $median -or $median -gt $maximum -or $maximum -ge [uint32]::MaxValue -or
            ($maximum - $minimum) * 100 -gt $median * 25) {
            throw 'Local APIC timer calibration samples are invalid, unsorted, or unstable.'
        }
        # Decimal arithmetic preserves exact integer rounding even for maximum
        # 32-bit samples. Round ties upward, matching the kernel's integer ratio.
        $denominator = [decimal](23864 * 100)
        $expectedReload = [uint64][decimal]::Floor(([decimal]$median * 1193182 + $denominator / 2) / $denominator)
        if ($reload -eq 0 -or $reload -gt [uint32]::MaxValue -or $reload -ne $expectedReload) {
            throw 'Local APIC timer reload does not match the calibrated PIT reference.'
        }
        $previous = $calibration[0].Index
        $selectionStart = $previous
    }
    if ($injected) {
        $injection = [regex]::Matches($Serial, '(?m)^MIRROR_LAPIC_TIMER_INJECTED active=1 cleanup=1\r?$')
        if ($injection.Count -ne 1) {
            throw 'Timer failure injection must verify cleanup of an active masked counter.'
        }
        $previous = $injection[0].Index
        $selectionStart = $previous
    }
    if ($null -ne $skipReason) {
        $skip = [regex]::Matches($Serial, ('(?m)^MIRROR_LAPIC_TIMER_SKIPPED reason={0}\r?$' -f $skipReason))
        if ($skip.Count -ne 1 -or $skip[0].Index -le $previous) {
            throw 'Timer fallback reason is missing, malformed, or precedes calibration cleanup.'
        }
        $previous = $skip[0].Index
        if ($selectionStart -lt 0) { $selectionStart = $previous }
        $controller = if ($UseLegacyPic) { 'pic' } else { 'ioapic' }
        $readyPattern = '(?m)^MIRROR_TIMER_READY source=pit irq=0 vector=32 hz~100 divisor=11932 controller={0}\r?$' -f $controller
    }
    else {
        $readyPattern = '(?m)^MIRROR_TIMER_READY source=lapic irq=local vector=241 hz~100 divide=16 initial={0} controller=lapic\r?$' -f $reload
    }
    $ready = [regex]::Matches($Serial, $readyPattern)
    if ($ready.Count -ne 1 -or $ready[0].Index -le $previous) {
        throw 'Timer readiness must match and follow the validated backend selection.'
    }
    if ($Smoke) {
        $policy = [regex]::Matches($Serial, '(?m)^MIRROR_TIMER_POLICY_OK\r?$')
        $test = [regex]::Matches($Serial, '(?m)^MIRROR_TIMER_OK ticks=([0-9]+) if_restored=0\r?$')
        if ($policyCount -ne 1 -or $policy.Count -ne 1 -or $testCount -ne 1 -or $test.Count -ne 1 -or
            $policy[0].Index -ge $selectionStart -or
            $test[0].Index -le $ready[0].Index -or [uint64]$test[0].Groups[1].Value -lt 3) {
            throw 'Timer smoke checks must validate policy before selection and hardware delivery after setup.'
        }
    }
    elseif ($policyCount -ne 0 -or $testCount -ne 0) {
        throw 'Normal boot unexpectedly ran smoke-only timer checks.'
    }
}

function Assert-MirrorHeapGrowth {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][int]$RequestedCpus,
        [bool]$Smoke,
        [bool]$UseLegacyPic
    )

    if ($Serial -match '(?m)^MIRROR_HEAP_GROW_SKIPPED(?:\s|$)') {
        throw 'Heap growth was skipped.'
    }
    $cpus = if ($UseLegacyPic) { 1 } else { $RequestedCpus }
    $rounds = if ($Smoke) { 4 } else { 1 }
    # Each run_pair reports both processes; smoke repeats the workload once.
    $userCount = if ($Smoke) { 4 } else { 2 }
    $initial = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_READY size=1048576 free=1048576\r?$')
    $sharedPattern = '(?m)^MIRROR_SHARED_VM_READY pages=512 cpus={0} rounds={1} reclaimed=3 borrowed_root=1\r?$' -f $cpus, $rounds
    $shared = [regex]::Matches($Serial, $sharedPattern)
    $user = [regex]::Matches($Serial, '(?m)^MIRROR_USERSPACE_OK scenario=\d+\r?$')
    if ($initial.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_READY(?:\s|$)').Count -ne 1 -or
        $shared.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_READY(?:\s|$)').Count -ne 1 -or
        $user.Count -ne $userCount -or [regex]::Matches($Serial, '(?m)^MIRROR_USERSPACE_OK(?:\s|$)').Count -ne $userCount -or
        $shared[0].Index -le $initial[0].Index -or
        [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROWN(?:\s|$)').Count -ne 2) {
        throw 'Heap validation requires one initial heap, shared-window checks, all userspace completions, and two growth commits.'
    }
    $previous = $shared[0].Index
    foreach ($phase in @(@{ Name = 'boot'; Size = 2097152 }, @{ Name = 'after_userspace'; Size = 3145728 })) {
        $pattern = '(?m)^MIRROR_HEAP_GROWN phase={0} size={1} used=(\d+) free=(\d+) max=16777216 added_pages=256\r?$' -f $phase.Name, $phase.Size
        $records = [regex]::Matches($Serial, $pattern)
        if ($records.Count -ne 1 -or $records[0].Index -le $previous) {
            throw "Missing, malformed, duplicate, or out-of-order heap growth phase $($phase.Name)."
        }
        $used = [uint64]$records[0].Groups[1].Value
        $free = [uint64]$records[0].Groups[2].Value
        if ($used -gt $phase.Size -or $free -ne ($phase.Size - $used)) {
            throw 'Heap growth accounting does not match the committed capacity.'
        }
        if (($phase.Name -eq 'boot' -and $records[0].Index -ge $user[0].Index) -or
            ($phase.Name -eq 'after_userspace' -and $records[0].Index -le $user[$userCount - 1].Index)) {
            throw 'Heap commits must bracket the native userspace workload.'
        }
        if ($Smoke) {
            $testPattern = '(?m)^MIRROR_HEAP_GROW_TESTS_OK phase={0} acquisition_rollback=1 mapped_rollback=1 live=1 large=1 guards=1 borrowed_root=1\r?$' -f $phase.Name
            $tests = [regex]::Matches($Serial, $testPattern)
            if ($tests.Count -ne 1 -or $tests[0].Index -le $previous -or $tests[0].Index -ge $records[0].Index -or
                ($phase.Name -eq 'after_userspace' -and $tests[0].Index -le $user[$userCount - 1].Index)) {
                throw "Heap growth regression checks did not precede phase $($phase.Name)."
            }
        }
        $previous = $records[0].Index
    }
    $testCount = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROW_TESTS_OK(?:\s|$)').Count
    $pagingCount = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_PAGING_OK(?:\s|$)').Count
    $limitCount = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_LIMIT_OK(?:\s|$)').Count
    if ($Smoke) {
        $pattern = '(?m)^MIRROR_HEAP_PAGING_OK cross_table=1 atomic=1 cpus={0} reclaimed=2\r?$' -f $cpus
        $paging = [regex]::Matches($Serial, $pattern)
        $firstTest = $Serial.IndexOf('MIRROR_HEAP_GROW_TESTS_OK phase=boot ', [StringComparison]::Ordinal)
        if ($testCount -ne 2 -or $pagingCount -ne 1 -or $paging.Count -ne 1 -or
            $paging[0].Index -le $shared[0].Index -or $paging[0].Index -ge $firstTest) {
            throw 'Heap tests must cover both growth commits and atomic mapping across a page-table boundary.'
        }
        $limit = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_LIMIT_OK size=16777216 limit_rejected=1 guards=1 reclaimed_allocations=1\r?$')
        if ($limitCount -ne 1 -or $limit.Count -ne 1 -or $limit[0].Index -le $previous) {
            throw 'Heap capacity checks must follow both growth commits exactly once.'
        }
        if (-not $UseLegacyPic) {
            $desktopPattern = '(?m)^MIRROR_SMP_IPI_OK phase=after_desktop cpus={0} rounds=4\r?$' -f $cpus
            $desktop = [regex]::Matches($Serial, $desktopPattern)
            if ($desktop.Count -ne 1 -or $limit[0].Index -le $desktop[0].Index) {
                throw 'Heap capacity checks must follow the final SMP rendezvous.'
            }
        }
        $faults = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_(?:GUARD_OK|UNMAP_OK|TLB_TIMEOUT_ARMED)(?:\s|$)')
        foreach ($fault in $faults) {
            if ($fault.Index -le $limit[0].Index) {
                throw 'Heap capacity checks must precede destructive SMP tests.'
            }
        }
    }
    elseif ($testCount -ne 0 -or $pagingCount -ne 0 -or $limitCount -ne 0) {
        throw 'Normal boot unexpectedly ran injected heap growth tests.'
    }
}

function ConvertTo-QemuOptionValue {
    param([Parameter(Mandatory = $true)][string]$Value)
    # QEMU's comma-separated option syntax escapes a literal comma by doubling it.
    return $Value.Replace(',', ',,')
}

function ConvertTo-WindowsProcessArgument {
    param([AllowEmptyString()][string]$Argument)
    # Start-Process joins ArgumentList into a Windows command line. Quote each
    # argument using the Windows C-runtime rules, including trailing backslashes.
    if ($Argument.Length -eq 0) { return '""' }
    if ($Argument -notmatch '[\s"]') { return $Argument }
    $escaped = [regex]::Replace($Argument, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Get-MirrorQemu {
    param([string]$RequestedPath, [string]$RepositoryRoot)

    if ($RequestedPath) {
        $candidate = $RequestedPath
        if (Test-Path -LiteralPath $candidate -PathType Container) {
            $candidate = Join-Path $candidate 'qemu-system-x86_64.exe'
        }
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "QEMU executable does not exist: $candidate"
        }
        return (Resolve-Path -LiteralPath $candidate).Path
    }

    $localQemu = Join-Path $RepositoryRoot '.tools/qemu/qemu-system-x86_64.exe'
    if (Test-Path -LiteralPath $localQemu -PathType Leaf) {
        return (Resolve-Path -LiteralPath $localQemu).Path
    }
    $command = Get-Command 'qemu-system-x86_64.exe' -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    throw 'QEMU was not found. Set QEMU_PATH to its executable or directory, pass -QemuPath, or place it in .tools/qemu/.'
}

function Get-MirrorFirmware {
    param(
        [string]$RequestedPath,
        [string]$QemuDirectory,
        [string[]]$Names,
        [string]$ParameterName
    )

    if ($RequestedPath) {
        if (-not (Test-Path -LiteralPath $RequestedPath -PathType Leaf)) {
            throw "Firmware does not exist: $RequestedPath"
        }
        return (Resolve-Path -LiteralPath $RequestedPath).Path
    }
    $searchDirectories = @(
        $QemuDirectory,
        (Join-Path $QemuDirectory 'share'),
        (Join-Path $QemuDirectory 'share/qemu'),
        (Join-Path $QemuDirectory 'pc-bios')
    )
    foreach ($directory in $searchDirectories) {
        foreach ($name in $Names) {
            $candidate = Join-Path $directory $name
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return (Resolve-Path -LiteralPath $candidate).Path
            }
        }
    }
    throw "UEFI firmware was not found beside QEMU. Pass -$ParameterName with the appropriate OVMF file."
}

function Get-MirrorTestDisk {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)
    # A disposable fixture only. Never open a physical drive or enroll storage.
    $directory = Join-Path $RepositoryRoot 'build'
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $path = Join-Path $directory 'block-test.img'
    $expectedHash = 'D0475F48263EA2DBCDF7A40C76BEA53F39C08977888CDFD6582427268134E318'
    if ((Test-Path -LiteralPath $path -PathType Leaf) -and
        (Get-Item -LiteralPath $path).Length -eq 1048576 -and
        (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq $expectedHash) {
        return $path
    }
    $bytes = [byte[]]::new(1048576)
    $signature = [byte[]]@(77, 73, 82, 82, 79, 82, 0, 1)
    for ($sector = 0; $sector -lt 2048; $sector++) {
        $start = $sector * 512
        for ($index = 0; $index -lt 8; $index++) {
            $bytes[$start + $index] = [byte](([uint64]$sector -shr (8 * $index)) -band 255)
            $bytes[$start + 8 + $index] = $signature[$index]
        }
        for ($index = 16; $index -lt 512; $index++) {
            $bytes[$start + $index] = [byte]((($sector * 73) -bxor ($sector -shr 8) -bxor ($index * 29) -bxor 165) -band 255)
        }
    }
    [System.IO.File]::WriteAllBytes($path, $bytes)
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $expectedHash) {
        throw 'Generated block fixture does not match its expected SHA-256.'
    }
    return $path
}

function New-MirrorWritableTestDisk {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)
    # Reset this separate, disposable image on every run. The source fixture
    # and the firmware boot disk are never exposed as writable storage.
    $source = Get-MirrorTestDisk -RepositoryRoot $RepositoryRoot
    $path = Join-Path $RepositoryRoot 'build/block-write-test.img'
    Copy-Item -LiteralPath $source -Destination $path -Force
    return $path
}

function Assert-MirrorWritableTestDisk {
    param(
        [Parameter(Mandatory = $true)][string]$OriginalPath,
        [Parameter(Mandatory = $true)][string]$WritablePath,
        [Parameter(Mandatory = $true)][bool]$WritesExpected
    )
    $expected = [System.IO.File]::ReadAllBytes($OriginalPath)
    if ($expected.Length -ne 1048576) { throw 'Unexpected original block fixture size.' }
    if ($WritesExpected) {
        # Independently reconstruct the last generation for every target LBA.
        # The runner pins the device queue to 256 entries; the guest performs
        # twice that many writes plus three, interleaved with readback.
        $sectors = @(0, 1234, 2047)
        $generations = @(0, 0, 0)
        for ($request = 0; $request -lt 256 * 2 + 3; $request++) {
            $generations[$request % 3] = $request + 1
        }
        $signature = [System.Text.Encoding]::ASCII.GetBytes('MIRRORW1')
        for ($slot = 0; $slot -lt $sectors.Count; $slot++) {
            $sector = [uint64]$sectors[$slot]
            $generation = [uint64]$generations[$slot]
            $start = [int]$sector * 512
            for ($index = 0; $index -lt 8; $index++) {
                $expected[$start + $index] = [byte](($sector -shr (8 * $index)) -band 255)
                $expected[$start + 8 + $index] = $signature[$index]
                $expected[$start + 16 + $index] = [byte](($generation -shr (8 * $index)) -band 255)
            }
            for ($index = 24; $index -lt 512; $index++) {
                $expected[$start + $index] = [byte]((($sector * 73) -bxor ($generation * 131) -bxor ($index * 29) -bxor 90) -band 255)
            }
        }
    }
    $hash = [System.Security.Cryptography.SHA256]::Create()
    try { $expectedHash = [BitConverter]::ToString($hash.ComputeHash($expected)).Replace('-', '') }
    finally { $hash.Dispose() }
    if ((Get-Item -LiteralPath $WritablePath).Length -ne $expected.Length -or
        (Get-FileHash -LiteralPath $WritablePath -Algorithm SHA256).Hash -ne $expectedHash) {
        throw 'The writable block fixture does not match the complete expected post-boot image.'
    }
}

function Assert-MirrorWritableBlockMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][string]$FaultMode
    )
    $withoutFlush = $FaultMode -eq 'BlockNoFlush'
    $flush = if ($withoutFlush) { 0 } else { 1 }
    $expected = @{
        MIRROR_BLOCK_WRITABLE_READY = "sectors=2048 queue=256 readonly=0 flush=$flush"
        MIRROR_BLOCK_POLICY_OK = 'writes_denied=1 flush_denied=1'
        MIRROR_BLOCK_CLEANUP_OK = 'dma_reclaimed=1'
    }
    if ($withoutFlush) {
        $expected.MIRROR_BLOCK_NO_FLUSH_OK = 'writes_denied=1 dirty=0'
    }
    else {
        $expected.MIRROR_BLOCK_WRITE_OK = 'sectors=2048 writes=515 queue=256 bounds_rejected=2'
        $expected.MIRROR_BLOCK_FLUSH_OK = 'dirty=0'
        $expected.MIRROR_BLOCK_DIRTY_SHUTDOWN_OK = 'dma_reclaimed=1'
        $expected.MIRROR_BLOCK_PERSIST_OK = 'sectors=3 neighbors=4'
    }
    if ($FaultMode -eq 'BlockWriteError') {
        $expected.MIRROR_BLOCK_WRITE_ERROR_OK = 'retry=1 input_preserved=1'
    }
    if ($FaultMode -eq 'BlockFlushError') {
        $expected.MIRROR_BLOCK_FLUSH_ERROR_OK = 'retry=1 dirty_preserved=1'
    }
    foreach ($marker in @('MIRROR_BLOCK_WRITABLE_READY', 'MIRROR_BLOCK_POLICY_OK', 'MIRROR_BLOCK_CLEANUP_OK',
            'MIRROR_BLOCK_NO_FLUSH_OK', 'MIRROR_BLOCK_WRITE_OK', 'MIRROR_BLOCK_FLUSH_OK',
            'MIRROR_BLOCK_DIRTY_SHUTDOWN_OK', 'MIRROR_BLOCK_PERSIST_OK',
            'MIRROR_BLOCK_WRITE_ERROR_OK', 'MIRROR_BLOCK_FLUSH_ERROR_OK')) {
        $records = [regex]::Matches($Serial, "(?m)^$marker(?:\s|$)")
        if ($expected.ContainsKey($marker)) {
            $pattern = '(?m)^{0} {1}\r?$' -f $marker, [regex]::Escape($expected[$marker])
            if ($records.Count -ne 1 -or [regex]::Matches($Serial, $pattern).Count -ne 1) {
                throw "Writable storage scenario $FaultMode requires exactly one '$marker $($expected[$marker])'."
            }
        }
        elseif ($records.Count -ne 0) {
            throw "Writable storage scenario $FaultMode unexpectedly reported $marker."
        }
    }
    if ($Serial -match '(?m)^MIRROR_BLOCK_(?:READY|READ_OK|PROBE_OK|SKIPPED|IO_ERROR_OK)(?:\s|$)') {
        throw 'A writable storage scenario unexpectedly ran the ordinary read-only probe.'
    }
}
