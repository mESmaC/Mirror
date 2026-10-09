Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

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
