Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-MirrorToolchain {
    param([string]$RustcVersion)

    # A RUSTUP_TOOLCHAIN variable, a rustup directory override or another rustc
    # on PATH silently replaces rust-toolchain.toml, so check the compiler itself.
    $repositoryRoot = Split-Path -Parent $PSScriptRoot
    $toolchain = Get-Content -LiteralPath (Join-Path $repositoryRoot 'rust-toolchain.toml') -Raw
    $channel = [regex]::Match($toolchain, '(?m)^channel\s*=\s*"(\d+\.\d+\.\d+)"')
    if (-not $channel.Success) {
        throw 'rust-toolchain.toml must pin an exact channel such as "1.99.0".'
    }
    $expected = $channel.Groups[1].Value
    $expectedMinor = $expected -replace '\.\d+$', ''
    $manifest = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
    $rustVersion = [regex]::Matches($manifest, '(?m)^rust-version\s*=\s*"([^"]*)"')
    if ($rustVersion.Count -ne 1) {
        throw 'Cargo.toml must declare exactly one workspace rust-version.'
    }
    $declared = $rustVersion[0].Groups[1].Value
    if ($PSBoundParameters.ContainsKey('RustcVersion')) {
        $actual = $RustcVersion
    }
    else {
        Push-Location -LiteralPath $repositoryRoot
        try {
            $actual = (@(& rustc --version) -join "`n").Trim()
            if ($LASTEXITCODE -ne 0) { throw "rustc --version failed with exit code $LASTEXITCODE." }
        }
        finally {
            Pop-Location
        }
    }
    if (-not $actual.StartsWith("rustc $expected ", [StringComparison]::Ordinal) -or $declared -ne $expectedMinor) {
        throw "Rust toolchain mismatch: rust-toolchain.toml pins $expected, so rustc must report 'rustc $expected' and Cargo.toml rust-version must be $expectedMinor. Found '$actual' and rust-version $declared. A RUSTUP_TOOLCHAIN variable, a 'rustup override' directory override or another rustc on PATH replaces the pin; a toolchain bump changes channel and rust-version in the same commit."
    }
}

function Assert-MirrorDriverCatalog {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Smoke,
        [bool]$LegacyDisk,
        [bool]$XhciAttached
    )

    foreach ($record in [regex]::Matches($Serial, '(?m)^(MIRROR_DRIVER_[^\s]*)[^\r\n]*')) {
        if ($record.Groups[1].Value -notin @('MIRROR_DRIVER_DEVICE', 'MIRROR_DRIVER_CATALOG', 'MIRROR_DRIVER_TESTS_OK')) {
            throw 'Unexpected driver catalog marker.'
        }
    }
    if ([regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_TESTS_OK(?:\s|$)').Count -ne [int]$Smoke -or
        [regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_TESTS_OK\r?$').Count -ne [int]$Smoke) {
        throw 'Driver matching self-tests must run exactly once in smoke builds only.'
    }
    $catalog = [regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_CATALOG retained=([0-9]+) matched=([0-9]+) unsupported=([0-9]+) omitted=([0-9]+)\r?$')
    $pci = [regex]::Matches($Serial, '(?m)^MIRROR_PCI_READY functions=([0-9]+) bridges=([0-9]+) virtio_block=([0-9]+) display=([0-9]+) virtio_gpu=([0-9]+) retained=([0-9]+) omitted=([0-9]+) reads=([0-9]+)\r?$')
    if ($catalog.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_CATALOG(?:\s|$)').Count -ne 1 -or
        $pci.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_PCI_READY(?:\s|$)').Count -ne 1) {
        throw 'Driver diagnostics require exactly one complete PCI discovery and catalog summary.'
    }
    $retained = [uint64]$catalog[0].Groups[1].Value
    $matched = [uint64]$catalog[0].Groups[2].Value
    $unsupported = [uint64]$catalog[0].Groups[3].Value
    $omitted = [uint64]$catalog[0].Groups[4].Value
    # The kernel retains at most 256 functions (0013-D7) and only counts the rest.
    if ($retained -eq 0 -or $retained -gt 256 -or $matched -gt $retained -or $unsupported -ne $retained - $matched -or
        $retained -ne [uint64]$pci[0].Groups[6].Value -or $omitted -ne [uint64]$pci[0].Groups[7].Value -or
        $retained + $omitted -ne [uint64]$pci[0].Groups[1].Value -or $catalog[0].Index -le $pci[0].Index) {
        throw 'Driver catalog counts must preserve the complete bounded PCI inventory.'
    }
    $devices = [regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_DEVICE bdf=([0-9a-f]{2}):([0-9a-f]{2})\.([0-7]) id=([0-9a-f]{4}):([0-9a-f]{4}) class=([0-9a-f]{2}):([0-9a-f]{2}):([0-9a-f]{2}) driver=(virtio-blk-legacy|ahci-sata|xhci-host|none) match=(exact|class|none)\r?$')
    if ($devices.Count -ne $retained -or [regex]::Matches($Serial, '(?m)^MIRROR_DRIVER_DEVICE(?:\s|$)').Count -ne $retained) {
        throw 'Every retained PCI function must have exactly one complete driver catalog record.'
    }
    $previousBdf = -1
    $legacy = 0
    $ahci = 0
    $xhci = 0
    $unknown = 0
    $modernBoot = 0
    $nic = 0
    $otherNetwork = 0
    foreach ($device in $devices) {
        $bus = [Convert]::ToInt32($device.Groups[1].Value, 16)
        $slot = [Convert]::ToInt32($device.Groups[2].Value, 16)
        $function = [int]$device.Groups[3].Value
        $bdf = $bus * 256 + $slot * 8 + $function
        if ($slot -gt 31 -or $bdf -le $previousBdf -or
            $device.Index -le $pci[0].Index -or $device.Index -ge $catalog[0].Index) {
            throw 'Driver device records must preserve unique PCI discovery order before the summary.'
        }
        $previousBdf = $bdf
        $identity = $device.Groups[4].Value + ':' + $device.Groups[5].Value
        $class = $device.Groups[6].Value + ':' + $device.Groups[7].Value + ':' + $device.Groups[8].Value
        $driver = $device.Groups[9].Value
        $kind = $device.Groups[10].Value
        if ($driver -eq 'virtio-blk-legacy') {
            if ($identity -ne '1af4:1001' -or $kind -ne 'exact') { throw 'Legacy VirtIO must match its exact supported identity.' }
            $legacy++
        }
        elseif ($driver -eq 'ahci-sata') {
            if ($class -ne '01:06:01' -or $kind -ne 'class') { throw 'AHCI fallback must match the standard SATA programming interface.' }
            $ahci++
        }
        elseif ($driver -eq 'xhci-host') {
            if ($class -ne '0c:03:30' -or $kind -ne 'class') { throw 'xHCI fallback must match the standard USB host programming interface.' }
            $xhci++
        }
        else {
            if ($kind -ne 'none') { throw 'Unsupported hardware must remain explicitly unmatched.' }
            $unknown++
            if ($identity -eq '1af4:1042') { $modernBoot++ }
        }
        # Every device profile attaches one Intel 82545EM, which stays unmatched
        # until its driver exists (spec 0014). No other network controller may appear.
        if ($class.StartsWith('02:', [StringComparison]::Ordinal)) {
            if ($identity -eq '8086:100f' -and $class -eq '02:00:00' -and $driver -eq 'none' -and $kind -eq 'none') { $nic++ }
            else { $otherNetwork++ }
        }
    }
    if ($nic -ne 1 -or $otherNetwork -ne 0) {
        throw 'The device profile must expose exactly one unmatched Intel 82545EM NIC (id=8086:100f class=02:00:00 driver=none match=none) and no other network controller.'
    }
    if ($legacy -ne [int]$LegacyDisk -or $ahci -ne 1 -or $xhci -ne [int]$XhciAttached -or $modernBoot -ne 1 -or
        $matched -ne $legacy + $ahci + $xhci -or $unsupported -ne $unknown) {
        throw 'The fixture must expose its expected fallback candidates and preserve the unsupported modern VirtIO boot device.'
    }
}

function Assert-MirrorXhciMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Attached,
        [ValidateSet('None', 'Keyboard', 'Mouse', 'Pair')][string]$UsbDevice = 'None',
        [bool]$WithoutHpet,
        [bool]$Smoke,
        [string]$FaultMode = 'None'
    )

    if ($UsbDevice -ne 'None' -and -not $Attached) {
        throw 'A connected USB device requires an attached xHCI controller.'
    }
    if ($FaultMode -eq 'XhciInitFailure' -and (-not $Attached -or -not $Smoke -or $UsbDevice -ne 'None')) {
        throw 'xHCI initialization failure requires smoke with an attached empty controller.'
    }
    $clockAvailable = -not $WithoutHpet -and $FaultMode -ne 'ClockActivation'
    $singleDevice = $UsbDevice -in @('Keyboard', 'Mouse')
    if ($FaultMode -eq 'XhciDeviceFailure' -and (-not $Attached -or -not $Smoke -or -not $singleDevice -or -not $clockAvailable)) {
        throw 'USB device failure requires smoke with one attached device and an available reset clock.'
    }
    $active = $Attached -and $UsbDevice -ne 'Pair' -and (-not $singleDevice -or $clockAvailable)
    $expected = @{}
    if ($Smoke) { $expected.MIRROR_XHCI_TESTS_OK = '' }
    if ($active) {
        $expected.MIRROR_XHCI_READY = 'ports=([1-9][0-9]*) slots=([1-9][0-9]*)'
        $expected.MIRROR_XHCI_COMMAND_OK = 'completed=520 command_wraps=2 event_wraps=2'
        $expected.MIRROR_XHCI_CLEANUP_OK = 'dma_reclaimed=1'
        if ($FaultMode -eq 'XhciInitFailure') {
            $expected.MIRROR_XHCI_INIT_ERROR_OK = 'dma_reclaimed=1'
        }
    }
    else {
        $expected.MIRROR_XHCI_SKIPPED = if (-not $Attached) { 'reason=no_controller' }
            elseif ($UsbDevice -eq 'Pair') { 'reason=connected_ports' } else { 'reason=clock_required' }
    }
    foreach ($record in [regex]::Matches($Serial, '(?m)^(MIRROR_XHCI_[^\s]*)[^\r\n]*')) {
        if (-not $expected.ContainsKey($record.Groups[1].Value)) {
            throw "Unexpected xHCI marker: $($record.Groups[1].Value)."
        }
    }
    foreach ($marker in $expected.Keys) {
        $suffix = if ($expected[$marker].Length -gt 0) { ' ' + $expected[$marker] } else { '' }
        $pattern = '(?m)^{0}{1}\r?$' -f $marker, $suffix
        $records = [regex]::Matches($Serial, $pattern)
        if ([regex]::Matches($Serial, "(?m)^$marker(?:\s|$)").Count -ne 1 -or $records.Count -ne 1) {
            throw "xHCI scenario requires exactly one complete $marker record."
        }
        if ($marker -eq 'MIRROR_XHCI_READY' -and
            ([uint64]$records[0].Groups[1].Value -gt 32 -or [uint64]$records[0].Groups[2].Value -gt 255)) {
            throw 'xHCI ports and device slots must fit the supported controller limits.'
        }
    }
    if ($active) {
        $ready = $Serial.IndexOf('MIRROR_XHCI_READY ', [StringComparison]::Ordinal)
        $commands = $Serial.IndexOf('MIRROR_XHCI_COMMAND_OK ', [StringComparison]::Ordinal)
        $cleanup = $Serial.IndexOf('MIRROR_XHCI_CLEANUP_OK ', [StringComparison]::Ordinal)
        if ($commands -le $ready -or $cleanup -le $commands) {
            throw 'xHCI command completion must follow controller readiness and precede DMA cleanup.'
        }
        if ($FaultMode -eq 'XhciInitFailure') {
            $recovery = $Serial.IndexOf('MIRROR_XHCI_INIT_ERROR_OK ', [StringComparison]::Ordinal)
            if ($recovery -ge $ready) {
                throw 'xHCI initialization failure must reclaim DMA before a successful reopen.'
            }
        }
    }
    if ($Smoke) {
        $tests = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_TESTS_OK\r?$').Index
        $selection = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_(?:INIT_ERROR_OK|READY|SKIPPED)(?:\s|$)').Index
        if ($tests -ge $selection) {
            throw 'xHCI policy tests must precede controller selection or injected initialization failure.'
        }
    }
}

function Assert-MirrorUsbMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Attached,
        [ValidateSet('None', 'Keyboard', 'Mouse', 'Pair')][string]$UsbDevice = 'None',
        [bool]$FullSpeed,
        [bool]$WithoutHpet,
        [bool]$Smoke,
        [string]$FaultMode = 'None'
    )

    $clockAvailable = -not $WithoutHpet -and $FaultMode -ne 'ClockActivation'
    $enumerated = $Attached -and $UsbDevice -in @('Keyboard', 'Mouse') -and $clockAvailable
    $expected = @{}
    if ($Smoke) { $expected.MIRROR_USB_TESTS_OK = '' }
    if ($enumerated) {
        # QEMU USB2 HID fixtures share an identity. The interface protocol and
        # endpoint packet size must distinguish keyboard from mouse.
        $protocol = if ($UsbDevice -eq 'Keyboard') { 1 } else { 2 }
        $packet = if ($UsbDevice -eq 'Keyboard') { 8 } else { 4 }
        $speed = if ($FullSpeed) { 'full' } else { 'high' }
        $ep0 = if ($FullSpeed) { 8 } else { 64 }
        $interval = if ($FullSpeed) { 10 } else { 7 }
        $expected.MIRROR_USB_DEVICE = "port=([1-9][0-9]*) slot=([1-9][0-9]*) address=([1-9][0-9]*) speed=$speed vid=0627 pid=0001 ep0=$ep0 configs=1"
        $expected.MIRROR_USB_CONFIGURATION = 'value=1 interfaces=1 bytes=34'
        $expected.MIRROR_USB_HID_CANDIDATE = "protocol=$protocol interface=0 endpoint=81 packet=$packet interval=$interval"
        $expected.MIRROR_USB_ENUMERATION_OK = if ($Smoke) { 'transfers=176 short_packets=172 control_wraps=2 slot_released=1' }
            else { 'transfers=4 short_packets=0 control_wraps=0 slot_released=1' }
        if ($FaultMode -eq 'XhciDeviceFailure') {
            $expected.MIRROR_USB_DEVICE_ERROR_OK = 'dma_reclaimed=1'
        }
    }
    elseif ($Attached -and $UsbDevice -eq 'None') {
        $expected.MIRROR_USB_SKIPPED = 'reason=no_device'
    }
    foreach ($record in [regex]::Matches($Serial, '(?m)^(MIRROR_USB_[^\s]*)[^\r\n]*')) {
        if (-not $expected.ContainsKey($record.Groups[1].Value)) {
            throw "Unexpected USB marker: $($record.Groups[1].Value)."
        }
    }
    $records = @{}
    foreach ($marker in $expected.Keys) {
        $suffix = if ($expected[$marker].Length -gt 0) { ' ' + $expected[$marker] } else { '' }
        $matchedRecords = [regex]::Matches($Serial, ('(?m)^{0}{1}\r?$' -f $marker, $suffix))
        if ([regex]::Matches($Serial, "(?m)^$marker(?:\s|$)").Count -ne 1 -or $matchedRecords.Count -ne 1) {
            throw "USB scenario requires exactly one complete $marker record."
        }
        $records[$marker] = $matchedRecords[0]
    }
    if ($Smoke) {
        $selection = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_(?:INIT_ERROR_OK|READY|SKIPPED)(?:\s|$)')
        if (-not $selection.Success -or $records.MIRROR_USB_TESTS_OK.Index -ge $selection.Index) {
            throw 'USB parser tests must precede controller selection.'
        }
    }
    if ($enumerated -or ($Attached -and $UsbDevice -eq 'None')) {
        $commands = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_COMMAND_OK(?:\s|$)')
        $cleanup = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_CLEANUP_OK(?:\s|$)')
        if (-not $commands.Success -or -not $cleanup.Success) {
            throw 'USB discovery requires completed controller commands and final DMA cleanup.'
        }
        $previous = $commands.Index
        $sequence = if ($enumerated) {
            if ($FaultMode -eq 'XhciDeviceFailure') { 'MIRROR_USB_DEVICE_ERROR_OK' }
            'MIRROR_USB_DEVICE'
            'MIRROR_USB_CONFIGURATION'
            'MIRROR_USB_HID_CANDIDATE'
            'MIRROR_USB_ENUMERATION_OK'
        }
        else { 'MIRROR_USB_SKIPPED' }
        foreach ($marker in $sequence) {
            if ($records[$marker].Index -le $previous -or $records[$marker].Index -ge $cleanup.Index) {
                throw 'USB discovery and recovery records must follow controller commands and precede final DMA cleanup in order.'
            }
            $previous = $records[$marker].Index
        }
    }
    if ($enumerated) {
        $ready = [regex]::Match($Serial, '(?m)^MIRROR_XHCI_READY ports=([1-9][0-9]*) slots=([1-9][0-9]*)\r?$')
        if (-not $ready.Success -or
            [uint64]$records.MIRROR_USB_DEVICE.Groups[1].Value -gt [uint64]$ready.Groups[1].Value -or
            [uint64]$records.MIRROR_USB_DEVICE.Groups[2].Value -gt [uint64]$ready.Groups[2].Value -or
            [uint64]$records.MIRROR_USB_DEVICE.Groups[3].Value -gt 127) {
            throw 'USB port and slot must belong to the controller, with a valid assigned USB address.'
        }
    }
}

function Assert-MirrorAhciMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Attached,
        [bool]$Smoke,
        [string]$FaultMode = 'None'
    )

    if (-not $Smoke -and $FaultMode -ne 'None') {
        throw 'Fault injection exists only in smoke kernels.'
    }
    # Smoke kernels recognize the fixture and run the full read workload. Trace
    # and default kernels only read sector 0 and compare nothing (spec 0010-D5),
    # so their workload marker is PROBE_OK and READ_OK is unexpected.
    $workload = if ($Smoke) { 'MIRROR_AHCI_READ_OK' } else { 'MIRROR_AHCI_PROBE_OK' }
    $expected = @{}
    if ($Smoke) { $expected.MIRROR_AHCI_TESTS_OK = '' }
    if ($Attached) {
        $expected.MIRROR_AHCI_READY = 'sectors=2048 readonly=1 port=0'
        $expected[$workload] = if ($Smoke) { 'sectors=2048 reads=262 bounds_rejected=2' } else { 'fixture=0 reads=1' }
        $expected.MIRROR_AHCI_CLEANUP_OK = 'dma_reclaimed=1'
        if ($FaultMode -eq 'AhciIoError') {
            $expected.MIRROR_AHCI_IO_ERROR_OK = 'retry=1 output_preserved=1'
        }
        if ($FaultMode -eq 'AhciInitFailure') {
            $expected.MIRROR_AHCI_INIT_ERROR_OK = 'dma_reclaimed=1'
        }
    }
    else {
        $expected.MIRROR_AHCI_SKIPPED = 'reason=no_disk'
    }
    foreach ($record in [regex]::Matches($Serial, '(?m)^(MIRROR_AHCI_[^\s]*)[^\r\n]*')) {
        if (-not $expected.ContainsKey($record.Groups[1].Value)) {
            throw "Unexpected AHCI marker: $($record.Groups[1].Value)."
        }
    }
    foreach ($marker in $expected.Keys) {
        $suffix = if ($expected[$marker].Length -gt 0) { ' ' + [regex]::Escape($expected[$marker]) } else { '' }
        $pattern = '(?m)^{0}{1}\r?$' -f $marker, $suffix
        if ([regex]::Matches($Serial, "(?m)^$marker(?:\s|$)").Count -ne 1 -or
            [regex]::Matches($Serial, $pattern).Count -ne 1) {
            throw "AHCI scenario requires exactly one '$marker $($expected[$marker])'."
        }
    }
    if ($Attached) {
        $ready = $Serial.IndexOf('MIRROR_AHCI_READY ', [StringComparison]::Ordinal)
        $reads = $Serial.IndexOf("$workload ", [StringComparison]::Ordinal)
        $cleanup = $Serial.IndexOf('MIRROR_AHCI_CLEANUP_OK ', [StringComparison]::Ordinal)
        if ($reads -le $ready -or $cleanup -le $reads) {
            throw 'AHCI reads must follow device readiness and precede DMA cleanup.'
        }
        if ($FaultMode -eq 'AhciIoError') {
            $recovery = $Serial.IndexOf('MIRROR_AHCI_IO_ERROR_OK ', [StringComparison]::Ordinal)
            if ($recovery -le $ready -or $recovery -ge $reads) {
                throw 'AHCI error recovery must precede the successful read workload.'
            }
        }
        if ($FaultMode -eq 'AhciInitFailure') {
            $recovery = $Serial.IndexOf('MIRROR_AHCI_INIT_ERROR_OK ', [StringComparison]::Ordinal)
            if ($recovery -ge $ready) {
                throw 'AHCI initialization failure must reclaim DMA before a successful reopen.'
            }
        }
    }
}

function Assert-MirrorTraceMarker {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][bool]$Expected
    )

    # test-trace kernels (trace and smoke) print one bare MIRROR_TRACE_ON in
    # _start before PCI discovery. Default kernels never print it (spec 0010).
    $names = [regex]::Matches($Serial, '(?m)^MIRROR_TRACE_ON(?:\s|$)')
    if (-not $Expected) {
        if ($names.Count -ne 0) {
            throw 'A default kernel must not print MIRROR_TRACE_ON; this is a trace or smoke build.'
        }
        return
    }
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_TRACE_ON\r?$')
    if ($names.Count -ne 1 -or $records.Count -ne 1) {
        throw 'A test-trace kernel must print exactly one bare MIRROR_TRACE_ON line.'
    }
    $pci = [regex]::Match($Serial, '(?m)^MIRROR_PCI_READY(?:\s|$)')
    if ($pci.Success -and $records[0].Index -ge $pci.Index) {
        throw 'MIRROR_TRACE_ON must precede PCI discovery.'
    }
}

function Assert-MirrorSchedReady {
    param([Parameter(Mandatory = $true)][string]$Serial)

    # Every flavor reports the process and thread tables once, with their
    # fixed capacities, before the first session loads a process or the
    # desktop is skipped (spec 0030a).
    $names = [regex]::Matches($Serial, '(?m)^MIRROR_SCHED_READY(?:\s|$)')
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_SCHED_READY processes=64 threads=256 max_threads=16 regions=64\r?$')
    if ($names.Count -ne 1 -or $records.Count -ne 1) {
        throw 'Mirror must report MIRROR_SCHED_READY processes=64 threads=256 max_threads=16 regions=64 exactly once.'
    }
    $first = [regex]::Match($Serial, '(?m)^(?:Native process: |MIRROR_DESKTOP_SKIPPED(?:\s|$))')
    if ($first.Success -and $first.Index -le $records[0].Index) {
        throw 'MIRROR_SCHED_READY must precede the first session.'
    }
}

function Assert-MirrorKernelVa {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][int]$OnlineCpus,
        [Parameter(Mandatory = $true)][bool]$Smoke
    )

    # Spec 0050-D4. Every flavor pins the kernel VA window's tables before SMP
    # startup seals kernel mappings and reports the window exactly once.
    $names = [regex]::Matches($Serial, '(?m)^MIRROR_KVA_READY(?:\s|$)')
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_KVA_READY base=0xffffd40000000000 size=33554432\r?$')
    if ($names.Count -ne 1 -or $records.Count -ne 1) {
        throw 'Mirror must report MIRROR_KVA_READY base=0xffffd40000000000 size=33554432 exactly once.'
    }
    $startup = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_(?:READY|SKIPPED)(?:\s|$)')
    if ($startup.Count -ne 1 -or $records[0].Index -ge $startup[0].Index) {
        throw 'MIRROR_KVA_READY must precede SMP startup.'
    }
    # Only smoke kernels run the window's self-test (0050, acceptance test 4),
    # after SMP startup, so it probes every online CPU, and before the
    # scheduler report. It fills all 455 stack spans and trims them in 33
    # batches of up to 256 pages.
    $tests = [regex]::Matches($Serial, '(?m)^MIRROR_KVA_TESTS_OK(?:\s|$)')
    if ($Smoke) {
        $pattern = '(?m)^MIRROR_KVA_TESTS_OK cpus={0} spans=455 trimmed=455 batches=33 reuse=1 rollback=1 degraded=1\r?$' -f $OnlineCpus
        $valid = [regex]::Matches($Serial, $pattern)
        $sched = [regex]::Match($Serial, '(?m)^MIRROR_SCHED_READY(?:\s|$)')
        if ($tests.Count -ne 1 -or $valid.Count -ne 1 -or $valid[0].Index -le $startup[0].Index -or
            -not $sched.Success -or $valid[0].Index -ge $sched.Index) {
            throw "A smoke kernel must pass the kernel VA self-test on $OnlineCpus CPU(s) exactly once, after SMP startup and before the scheduler report."
        }
    }
    elseif ($tests.Count -ne 0) {
        throw 'Only smoke kernels run the kernel VA self-test.'
    }
}

function Assert-MirrorKernelStacks {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [string]$FaultMode = 'None'
    )

    # Spec 0050-D4, smoke kernels only. The first init-pair session runs with
    # the kernel VA window's shootdown transport withheld and says so once,
    # after the table-bounds test (test 4). Each pair session's drain then
    # reports its thread stacks once, with its full field list: threads= is
    # the per-process peak of live stacks, 16 at the primary's capacity check;
    # watermark_pages= the deepest high-water mark, 1 to 16 pages; built= the
    # stacks the session built fresh; and pool= the pool after the trim. The
    # first session skips its trim, so its pool holds every stack it built,
    # 17 to 32 for the interleaved pair. The second reuses that pool, so
    # together they built at most 32, and its trim empties the pool. Returns
    # the guard page that ThreadStackOverflow reports, or $null.
    $bounds = [regex]::Match($Serial, '(?m)^MIRROR_SCHED_BOUNDS_OK(?:\s|$)')
    $degraded = [regex]::Matches($Serial, '(?m)^MIRROR_KVA_DEGRADED(?:\s|$)')
    $degradedValid = [regex]::Matches($Serial, '(?m)^MIRROR_KVA_DEGRADED session=1\r?$')
    if ($degraded.Count -ne 1 -or $degradedValid.Count -ne 1 -or -not $bounds.Success -or
        $degradedValid[0].Index -le $bounds.Index) {
        throw 'A smoke kernel must report MIRROR_KVA_DEGRADED session=1 exactly once, after the table-bounds test.'
    }
    $lines = [regex]::Matches($Serial, '(?m)^MIRROR_KSTACK_OK(?:\s|$)')
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_KSTACK_OK threads=(\d+) watermark_pages=(\d+) built=(\d+) pool=(\d+)\r?$')
    if ($lines.Count -ne 2 -or $records.Count -ne 2 -or $records[0].Index -le $degradedValid[0].Index) {
        throw 'A smoke kernel must report MIRROR_KSTACK_OK threads= watermark_pages= built= pool= exactly twice, once per init-pair session, after MIRROR_KVA_DEGRADED.'
    }
    foreach ($record in $records) {
        $watermark = [int]$record.Groups[2].Value
        if ($watermark -lt 1 -or $watermark -gt 16) {
            throw 'Each init-pair session must peak at 16 live thread stacks in one process, with a high-water mark of 1 to 16 pages.'
        }
    }
    # 0054-D5's scenarios replace the relaunch pair with two single-threaded
    # processes: only the battery pair peaks at 16 live stacks.
    $scenarioPair = $FaultMode -in @('OomOrder', 'ClassLimits')
    if ([int]$records[0].Groups[1].Value -ne 16 -or
        [int]$records[1].Groups[1].Value -ne $(if ($scenarioPair) { 1 } else { 16 })) {
        throw 'Each init-pair session must peak at 16 live thread stacks in one process.'
    }
    $firstBuilt = [int]$records[0].Groups[3].Value
    $secondBuilt = [int]$records[1].Groups[3].Value
    if ($firstBuilt -lt 17 -or $firstBuilt -gt 32 -or [int]$records[0].Groups[4].Value -ne $firstBuilt) {
        throw 'The degraded session must build 17 to 32 thread stacks and keep every one pooled.'
    }
    if ([int]$records[1].Groups[4].Value -ne 0 -or $firstBuilt + $secondBuilt -gt 32) {
        throw 'The second session must reuse the pool, building at most 32 stacks in all, and trim it empty.'
    }
    # Spec 0050, acceptance test 3: only ThreadStackOverflow reports the guard
    # page below its stack, once, after both sessions and before it announces
    # the fatal fault.
    $guards = [regex]::Matches($Serial, '(?m)^MIRROR_KSTACK_GUARD(?:\s|$)')
    if ($FaultMode -ne 'ThreadStackOverflow') {
        if ($guards.Count -ne 0) {
            throw 'Only the ThreadStackOverflow case reports MIRROR_KSTACK_GUARD.'
        }
        return $null
    }
    $guard = [regex]::Matches($Serial, '(?m)^MIRROR_KSTACK_GUARD guard=(0x[0-9a-f]+)\r?$')
    $expect = [regex]::Matches($Serial, '(?m)^MIRROR_TEST_EXPECT_FATAL(?:\s|$)')
    if ($guards.Count -ne 1 -or $guard.Count -ne 1 -or $expect.Count -ne 1 -or
        $guard[0].Index -le $records[1].Index -or $guard[0].Index -ge $expect[0].Index) {
        throw 'ThreadStackOverflow must report MIRROR_KSTACK_GUARD once, after both sessions and before announcing its fault.'
    }
    $address = [Convert]::ToUInt64($guard[0].Groups[1].Value.Substring(2), 16)
    $window = [Convert]::ToUInt64('ffffd40000000000', 16)
    if ($address -lt $window -or $address -ge $window + 33554432 -or ($address % 4096) -ne 0) {
        throw 'The thread stack guard must be a page of the kernel VA window.'
    }
    return $address
}

function Assert-MirrorCpuData {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][bool]$Smoke
    )

    # Spec 0050-D4. After SMP startup every flavor checks that each online CPU
    # runs with its own CpuData in GS and reports it exactly once, after
    # MIRROR_KVA_READY. cpus= is recomputed from the SMP line: online= of
    # MIRROR_SMP_READY, or 1 when MIRROR_SMP_SKIPPED kept the APs offline.
    $startup = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_(?:READY|SKIPPED)(?:\s|$)')
    if ($startup.Count -ne 1) {
        throw 'Mirror must report SMP startup exactly once before its per-CPU data.'
    }
    $smpReady = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_READY discovered=\d+ online=(\d+) ')
    $smpSkipped = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_SKIPPED reason=\S+ online=1\r?$')
    if ($smpReady.Count -eq 1 -and $smpSkipped.Count -eq 0) {
        $cpus = [int]$smpReady[0].Groups[1].Value
    }
    elseif ($smpReady.Count -eq 0 -and $smpSkipped.Count -eq 1) {
        $cpus = 1
    }
    else {
        throw 'Mirror reported no SMP startup line to count online CPUs from.'
    }
    $names = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_DATA_READY(?:\s|$)')
    $records = [regex]::Matches($Serial, ('(?m)^MIRROR_CPU_DATA_READY cpus={0}\r?$' -f $cpus))
    if ($names.Count -ne 1 -or $records.Count -ne 1) {
        throw "Mirror must report MIRROR_CPU_DATA_READY cpus=$cpus exactly once."
    }
    $kva = [regex]::Match($Serial, '(?m)^MIRROR_KVA_READY(?:\s|$)')
    if (-not $kva.Success -or $records[0].Index -le $kva.Index -or $records[0].Index -le $startup[0].Index) {
        throw 'MIRROR_CPU_DATA_READY must follow MIRROR_KVA_READY and SMP startup.'
    }
    # Only smoke kernels send OP_READ_CPU_DATA to the BSP and every online AP
    # (0050, acceptance test 2), just before the report.
    $tests = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_DATA_TESTS_OK(?:\s|$)')
    if ($Smoke) {
        $valid = [regex]::Matches($Serial, ('(?m)^MIRROR_CPU_DATA_TESTS_OK cpus={0}\r?$' -f $cpus))
        if ($tests.Count -ne 1 -or $valid.Count -ne 1 -or $valid[0].Index -le $startup[0].Index -or
            $valid[0].Index -ge $records[0].Index) {
            throw "A smoke kernel must prove per-CPU data on $cpus CPU(s) exactly once, before MIRROR_CPU_DATA_READY."
        }
    }
    elseif ($tests.Count -ne 0) {
        throw 'Only smoke kernels run the per-CPU data probe.'
    }
}

function Assert-MirrorStacks {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][int]$OnlineCpus,
        [Parameter(Mandatory = $true)][bool]$Smoke
    )

    # Spec 0016a. After SMP startup every flavor prints one MIRROR_CPU_STACKS
    # line per online CPU in slot order, the BSP first, and then one
    # MIRROR_STACKS_READY line. Each CPU's TSS points IST1-3 at its own guarded
    # stacks, and its CR4.MCE equals its CPUID MCE bit. QEMU's qemu64 and max
    # models both report MCE, so the bit must be set. Only smoke kernels run
    # the NMI and #MC IST probe. Returns the guard page below the BSP's RSP0
    # stack.
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_STACKS cpu=(\d+) ist=3 mce=([01]) cpuid_mce=([01])\r?$')
    if ($records.Count -ne $OnlineCpus -or [regex]::Matches($Serial, '(?m)^MIRROR_CPU_STACKS(?:\s|$)').Count -ne $OnlineCpus) {
        throw "Mirror must report guarded stacks with three IST stacks for exactly $OnlineCpus online CPU(s)."
    }
    $startup = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_(?:READY|SKIPPED)(?:\s|$)')
    if ($startup.Count -ne 1 -or $records[0].Index -le $startup[0].Index) {
        throw 'The per-CPU stack reports must follow SMP startup.'
    }
    for ($cpu = 0; $cpu -lt $OnlineCpus; $cpu++) {
        $record = $records[$cpu]
        if ([int]$record.Groups[1].Value -ne $cpu) {
            throw 'The per-CPU stack reports must list cpu=0..N-1 in order.'
        }
        if ($record.Groups[3].Value -ne '1' -or $record.Groups[2].Value -ne $record.Groups[3].Value) {
            throw "CPU $cpu must report CPUID MCE support and set CR4.MCE to match it."
        }
    }
    $ready = [regex]::Matches($Serial, '(?m)^MIRROR_STACKS_READY scheduler=256 rsp0=64 ist=3 rsp0_guard=(0x[0-9a-f]+)\r?$')
    if ($ready.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_STACKS_READY(?:\s|$)').Count -ne 1 -or
        $ready[0].Index -le $records[$OnlineCpus - 1].Index) {
        throw 'Mirror must report the BSP stack region once, after the per-CPU stack reports.'
    }
    $guard = [Convert]::ToUInt64($ready[0].Groups[1].Value.Substring(2), 16)
    if ($guard -lt [Convert]::ToUInt64('ffff800000000000', 16) -or ($guard % 4096) -ne 0) {
        throw 'The RSP0 guard page must be a page-aligned kernel-half address.'
    }
    $istProbes = [regex]::Matches($Serial, '(?m)^MIRROR_IST_OK(?:\s|$)').Count
    if ($Smoke) {
        if ($istProbes -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_IST_OK nmi=2 mc=3\r?$').Count -ne 1) {
            throw 'A smoke kernel must run NMI on IST2 and #MC on IST3 exactly once.'
        }
    }
    elseif ($istProbes -ne 0) {
        throw 'Only smoke kernels run the NMI and #MC IST probe.'
    }
    return $guard
}

function Assert-MirrorHardening {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][int]$OnlineCpus,
        [Parameter(Mandatory = $true)][bool]$SmepSmap,
        [Parameter(Mandatory = $true)][bool]$Umip
    )

    # Spec 0016b. After SMP startup every flavor prints one MIRROR_CPU_HARDENING
    # line per online CPU in slot order, the BSP first, all with EFER.SCE clear.
    # smep, smap and umip must equal CR4 bits 20, 21 and 11, recomputed here
    # from cr4. Each CPU sets SMEP, SMAP and UMIP exactly where its CPUID
    # reports them. An AP whose CPUID set differs from the BSP's fails startup,
    # so every line reports the same set. The CPU model fixes that set, as
    # $SmepSmap and $Umip say: the default qemu64,+smep,+smap reports SMEP and
    # SMAP, max reports all three, and plain qemu64 reports none (0016-D6).
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_HARDENING cpu=(\d+) cr4=(0x[0-9a-f]+) smep=([01]) smap=([01]) umip=([01]) sce=0 cpuid_smep=([01]) cpuid_smap=([01]) cpuid_umip=([01])\r?$')
    if ($records.Count -ne $OnlineCpus -or [regex]::Matches($Serial, '(?m)^MIRROR_CPU_HARDENING(?:\s|$)').Count -ne $OnlineCpus) {
        throw "Mirror must report CPU protections with EFER.SCE clear for exactly $OnlineCpus online CPU(s)."
    }
    $startup = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_(?:READY|SKIPPED)(?:\s|$)')
    if ($startup.Count -ne 1 -or $records[0].Index -le $startup[0].Index) {
        throw 'The per-CPU protection reports must follow SMP startup.'
    }
    $expected = if ($SmepSmap) { '1' } else { '0' }
    $expectedUmip = if ($Umip) { '1' } else { '0' }
    for ($cpu = 0; $cpu -lt $OnlineCpus; $cpu++) {
        $record = $records[$cpu]
        if ([int]$record.Groups[1].Value -ne $cpu) {
            throw 'The per-CPU protection reports must list cpu=0..N-1 in order.'
        }
        $cr4 = [Convert]::ToUInt64($record.Groups[2].Value.Substring(2), 16)
        foreach ($field in @(@{ Group = 3; Bit = 20 }, @{ Group = 4; Bit = 21 }, @{ Group = 5; Bit = 11 })) {
            if ($record.Groups[$field.Group].Value -ne [string](($cr4 -shr $field.Bit) -band 1)) {
                throw "CPU $cpu reported a protection that its CR4 value does not hold."
            }
        }
        foreach ($group in 3..5) {
            if ($record.Groups[$group].Value -ne $record.Groups[$group + 3].Value) {
                throw "CPU $cpu must set SMEP, SMAP and UMIP exactly where CPUID reports them."
            }
        }
        foreach ($group in 6..8) {
            if ($record.Groups[$group].Value -ne $records[0].Groups[$group].Value) {
                throw "CPU $cpu reported CPUID protections that differ from the BSP's."
            }
        }
        if ($record.Groups[6].Value -ne $expected -or $record.Groups[7].Value -ne $expected -or
            $record.Groups[8].Value -ne $expectedUmip) {
            throw "CPU $cpu must report cpuid_smep=$expected cpuid_smap=$expected cpuid_umip=$expectedUmip on this CPU model."
        }
    }
}

function Assert-MirrorCpuVulnerability {
    param([Parameter(Mandatory = $true)][string]$Serial)

    # Spec 0016b. Every flavor prints one MIRROR_CPU_VULN line for the BSP after
    # the per-CPU protection reports, and applies no mitigation (0016-D5). The
    # verdicts are recomputed here from the vendor and arch_caps: AMD is not
    # affected by Meltdown or MDS, Intel without IA32_ARCH_CAPABILITIES is
    # affected by both, and any other vendor is unknown. Intel with the MSR
    # depends on its RDCL_NO and MDS_NO bits, which the line does not carry, so
    # only a known verdict is required there. QEMU's qemu64 and max models both
    # identify as AuthenticAMD, set the CPUID hypervisor bit and do not report
    # IA32_ARCH_CAPABILITIES.
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_VULN vendor=(intel|amd|other) hypervisor=([01]) arch_caps=([01]) meltdown=(affected|not_affected|unknown) mds=(affected|not_affected|unknown) mitigation=none\r?$')
    if ($records.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_CPU_VULN(?:\s|$)').Count -ne 1) {
        throw 'Mirror must report its speculative-execution exposure exactly once, with no mitigation.'
    }
    $record = $records[0]
    $protections = [regex]::Matches($Serial, '(?m)^MIRROR_CPU_HARDENING(?:\s|$)')
    if ($protections.Count -eq 0 -or $record.Index -le $protections[$protections.Count - 1].Index) {
        throw 'The speculative-execution report must follow the per-CPU protection reports.'
    }
    $vendor = $record.Groups[1].Value
    $verdicts = @($record.Groups[4].Value, $record.Groups[5].Value)
    $expected = switch ($vendor) {
        'amd' { @('not_affected', 'not_affected') }
        'other' { @('unknown', 'unknown') }
        default { if ($record.Groups[3].Value -eq '0') { @('affected', 'affected') } else { $null } }
    }
    if ($null -ne $expected) {
        if ($verdicts[0] -ne $expected[0] -or $verdicts[1] -ne $expected[1]) {
            throw "vendor=$vendor arch_caps=$($record.Groups[3].Value) must report meltdown=$($expected[0]) mds=$($expected[1])."
        }
    }
    elseif ($verdicts -contains 'unknown') {
        throw 'An Intel CPU with IA32_ARCH_CAPABILITIES must report a known Meltdown and MDS verdict.'
    }
    if ($vendor -ne 'amd' -or $record.Groups[2].Value -ne '1' -or $record.Groups[3].Value -ne '0') {
        throw "QEMU's CPU models must report vendor=amd hypervisor=1 arch_caps=0."
    }
}

function Assert-MirrorClock {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Smoke,
        [bool]$WithoutHpet,
        [string]$FaultMode = 'None',
        [int]$InitRuns = 4
    )

    $injected = $FaultMode -eq 'ClockActivation'
    if ($injected -and (-not $Smoke -or $WithoutHpet)) {
        throw 'Clock activation fault injection requires smoke with HPET present.'
    }
    $available = -not $WithoutHpet -and -not $injected
    # Smoke runs both init processes twice. Trace and default kernels run no
    # init program (spec 0010-D3), so they report no user clock checks.
    $userCount = if ($Smoke) { $InitRuns } else { 0 }
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

function Assert-MirrorReset {
    # Spec 0058a, closed-world over MIRROR_ACPI_RESET_* and MIRROR_RESET_*:
    # QEMU's FADT reset register at discovery, the smoke policy check, and at
    # most one restart, whose lines end the log. A smoke reset case (-Fault
    # Reset*) restarts with cause=test; a capture types restart at the console.
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [bool]$Smoke,
        [string]$FaultMode = 'None',
        [bool]$WithoutKeyboard,
        [bool]$WithoutHpet,
        [bool]$ConsoleRestart
    )

    $first = switch ($FaultMode) {
        'Reset' { 0 }
        'ResetFromKbc' { 1 }
        'ResetFromCf9' { 2 }
        'ResetFromTripleFault' { 3 }
        'ResetSuppressed' { 0 }
        default { $null }
    }
    if (($null -ne $first -and -not $Smoke) -or ($ConsoleRestart -and ($Smoke -or $null -ne $first))) {
        throw 'A reset fault needs a smoke kernel, and a console restart a trace or default one.'
    }
    # q35 describes ICH9's RST_CNT with 0x0F, revision 3 or later (test 4).
    $expected = [System.Collections.Generic.List[string]]::new()
    $expected.Add('MIRROR_ACPI_RESET_READY revision=([0-9]+) port=0xcf9 value=0xf')
    if ($Smoke) { $expected.Add('MIRROR_RESET_POLICY_OK') }
    $restarted = $ConsoleRestart -or $null -ne $first
    if ($ConsoleRestart) {
        $expected.Add('MIRROR_RESET_BEGIN cause=console')
        $expected.Add('MIRROR_RESET_ATTEMPT method=fadt')
    }
    elseif ($null -ne $first) {
        $expected.Add('MIRROR_RESET_BEGIN cause=test')
        # Every write but a suppressed one resets QEMU, ending the log there.
        $suppressed = $FaultMode -eq 'ResetSuppressed'
        $clock = if ($WithoutHpet) { 'pit' } else { 'hpet' }
        $methods = @(
            @{ Name = 'fadt'; Writes = @('port=0xcf9 value=0xb', 'port=0xcf9 value=0xf') },
            @{ Name = 'kbc'; Writes = @('port=0x64 value=0xfe') },
            @{ Name = 'cf9'; Writes = @('port=0xcf9 value=0x2', 'port=0xcf9 value=0x6') }
        )
        $reset = $false
        for ($index = 0; $index -lt $methods.Count -and -not $reset; $index++) {
            $name = $methods[$index].Name
            if ($index -lt $first) {
                $expected.Add("MIRROR_RESET_SKIPPED method=$name reason=test_skip")
            }
            elseif ($name -eq 'kbc' -and $WithoutKeyboard) {
                $expected.Add('MIRROR_RESET_SKIPPED method=kbc reason=absent')
            }
            else {
                $expected.Add("MIRROR_RESET_ATTEMPT method=$name")
                if (-not $suppressed) { $reset = $true }
                else {
                    foreach ($write in $methods[$index].Writes) {
                        $expected.Add("MIRROR_RESET_SUPPRESSED method=$name $write")
                    }
                    $expected.Add("MIRROR_RESET_TIMEOUT method=$name elapsed_ms=([0-9]+) clock=$clock")
                }
            }
        }
        if (-not $reset) { $expected.Add('MIRROR_RESET_ATTEMPT method=triple_fault') }
    }
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_(?:ACPI_RESET|RESET)_[^\r\n]*')
    if ($records.Count -ne $expected.Count) {
        throw "Mirror must print exactly $($expected.Count) reset line(s) ($($expected -join '; ')); it printed $($records.Count)."
    }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        $line = $records[$index].Value
        $match = [regex]::Match($line, "^$($expected[$index])`$")
        if (-not $match.Success) { throw "Reset line $($index + 1) must be '$($expected[$index])'; it is '$line'." }
        if ($index -eq 0 -and [uint64]$match.Groups[1].Value -lt 2) {
            throw "The FADT reset register needs revision 2 or later: '$line'."
        }
        if ($line.StartsWith('MIRROR_RESET_TIMEOUT ')) {
            $elapsed = [uint64]$match.Groups[1].Value
            if ($elapsed -lt 500 -or $elapsed -gt 750) {
                throw "Each reset wait must last 500 to 750 ms; '$line'."
            }
        }
    }
    if ($restarted) {
        # Nothing follows the method that restarted the machine.
        $markers = [regex]::Matches($Serial, '(?m)^MIRROR_[^\r\n]*')
        if ($markers[$markers.Count - 1].Index -ne $records[$records.Count - 1].Index) {
            throw "The restart's last line must be the log's last marker; found '$($markers[$markers.Count - 1].Value)' after it."
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
        [bool]$UseLegacyPic,
        [int]$InitRuns = 4
    )

    # 0059-D1/D4: the boot and after_userspace growths must succeed in every
    # flavor, so their skip records always fail the run. boot-smoke's capacity
    # self-test reports its one accepted AtCap skip (heap.rs capacity test); any
    # other skip, or that one outside smoke, fails.
    $skipped = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROW_SKIPPED phase=(boot|after_userspace|auto) reason=(LimitExceeded|OutOfFrames)\r?$')
    if ([regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROW_SKIPPED(?:\s|$)').Count -ne $skipped.Count) {
        throw 'Every heap growth skip record must be complete.'
    }
    foreach ($skip in $skipped) {
        if ($skip.Groups[1].Value -ne 'auto' -or $skip.Groups[2].Value -cne 'LimitExceeded') {
            throw 'Heap growth may skip only the capacity self-test auto phase with LimitExceeded.'
        }
    }
    if ($skipped.Count -ne [int]$Smoke) {
        throw 'Only a smoke boot reports exactly one capacity self-test heap growth skip.'
    }
    $cpus = if ($UseLegacyPic) { 1 } else { $RequestedCpus }
    $rounds = if ($Smoke) { 4 } else { 1 }
    # Each run_pair reports both processes; smoke repeats the workload once.
    # Trace and default kernels run no init program (spec 0010-D3).
    $userCount = if ($Smoke) { $InitRuns } else { 0 }
    $initial = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_READY size=1048576 free=1048576\r?$')
    $sharedPattern = '(?m)^MIRROR_SHARED_VM_READY pages=512 cpus={0} rounds={1} reclaimed=3 borrowed_root=1\r?$' -f $cpus, $rounds
    $shared = [regex]::Matches($Serial, $sharedPattern)
    $user = [regex]::Matches($Serial, '(?m)^MIRROR_USERSPACE_OK scenario=\d+\r?$')
    if ($initial.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_READY(?:\s|$)').Count -ne 1 -or
        $shared.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_READY(?:\s|$)').Count -ne 1 -or
        $user.Count -ne $userCount -or [regex]::Matches($Serial, '(?m)^MIRROR_USERSPACE_OK(?:\s|$)').Count -ne $userCount -or
        $shared[0].Index -le $initial[0].Index) {
        throw 'Heap validation requires one initial heap, shared-window checks, and all userspace completions.'
    }
    # 0059-D4: walk every growth record in order (the boot and after_userspace
    # commits, plus 0059's phase=auto safe-point records in trace/smoke builds),
    # recomputing each committed size from the one before. Exactly one boot and
    # one after_userspace record, in order; at most 13 auto records (16 MiB cap).
    $grown = [regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROWN phase=(boot|after_userspace|auto) size=(\d+) used=(\d+) free=(\d+) max=16777216 added_pages=(\d+)\r?$')
    $phaseIndex = @{}
    $autoCount = 0
    $size = 1048576
    foreach ($record in $grown) {
        $phase = $record.Groups[1].Value
        $recordedSize = [uint64]$record.Groups[2].Value
        $used = [uint64]$record.Groups[3].Value
        $free = [uint64]$record.Groups[4].Value
        $added = [uint64]$record.Groups[5].Value
        if ($phase -eq 'auto') { $autoCount += 1 }
        elseif ($phaseIndex.ContainsKey($phase)) { throw "Heap growth phase $phase printed twice." }
        if ($added -ne 256 -or $recordedSize -ne $size + $added * 4096 -or $free -ne $recordedSize - $used) {
            throw 'Heap growth accounting does not match the committed capacity.'
        }
        if ($phase -ne 'auto') { $phaseIndex[$phase] = $record.Index }
        $size = $recordedSize
    }
    if (-not $phaseIndex.ContainsKey('boot') -or -not $phaseIndex.ContainsKey('after_userspace') -or
        $phaseIndex['boot'] -ge $phaseIndex['after_userspace'] -or $autoCount -gt 13) {
        throw 'Heap validation requires the boot and after_userspace commits in order, with at most 13 auto records.'
    }
    if ([regex]::Matches($Serial, '(?m)^MIRROR_HEAP_GROWN(?:\s|$)').Count -ne $grown.Count) {
        throw 'Every heap growth record must be complete.'
    }
    foreach ($phaseName in @('boot', 'after_userspace')) {
        $recordIndex = $phaseIndex[$phaseName]
        # Without an init workload there is nothing to bracket; the phase order
        # above still places both commits after the shared-window checks.
        if ($userCount -gt 0 -and
            (($phaseName -eq 'boot' -and $recordIndex -ge $user[0].Index) -or
            ($phaseName -eq 'after_userspace' -and $recordIndex -le $user[$userCount - 1].Index))) {
            throw 'Heap commits must bracket the native userspace workload.'
        }
        if ($Smoke) {
            $testPattern = '(?m)^MIRROR_HEAP_GROW_TESTS_OK phase={0} acquisition_rollback=1 mapped_rollback=1 live=1 large=1 guards=1 borrowed_root=1\r?$' -f $phaseName
            $tests = [regex]::Matches($Serial, $testPattern)
            if ($tests.Count -ne 1 -or $tests[0].Index -ge $recordIndex -or
                ($phaseName -eq 'after_userspace' -and $tests[0].Index -le $user[$userCount - 1].Index)) {
                throw "Heap growth regression checks did not precede phase $phaseName."
            }
        }
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
        if ($limitCount -ne 1 -or $limit.Count -ne 1 -or $limit[0].Index -le $phaseIndex['after_userspace'] -or
            ($skipped.Count -eq 1 -and $skipped[0].Index -ge $limit[0].Index)) {
            throw 'Heap capacity checks must follow both growth commits and the capacity skip record exactly once.'
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

function Assert-MirrorFramePool {
    param([Parameter(Mandatory = $true)][string]$Serial)

    # 0054-D3: exactly one complete pool record per boot, in every flavor. The
    # script recomputes every derived field: the refcount array is two bytes
    # per frame in its own page-aligned span beside the 2-bit bitmap's span.
    $records = [regex]::Matches($Serial, '(?m)^MIRROR_FRAME_POOL total=([0-9]+) allocated=([0-9]+) metadata=([0-9]+) free=([0-9]+) refcount_meta=([0-9]+)\r?$')
    if ($records.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_FRAME_POOL(?:\s|$)').Count -ne 1) {
        throw 'Mirror must report its frame pool exactly once, with every field.'
    }
    $total = [uint64]$records[0].Groups[1].Value
    $allocated = [uint64]$records[0].Groups[2].Value
    $metadata = [uint64]$records[0].Groups[3].Value
    $free = [uint64]$records[0].Groups[4].Value
    $refcountMeta = [uint64]$records[0].Groups[5].Value
    $bitmapFrames = [uint64][decimal]::Ceiling([decimal][decimal]::Ceiling($total / 4) / 4096)
    $refcountFrames = [uint64][decimal]::Ceiling(($total * 2) / 4096)
    if ($refcountMeta -ne $total * 2 -or $metadata -ne $bitmapFrames + $refcountFrames -or
        $free -ne $total - $allocated - $metadata) {
        throw 'Frame pool accounting does not match the bitmap and refcount spans.'
    }
    # 0054-D6: the reserve floor follows the pool record once per boot, in
    # every flavor, recomputed from the pool's total by the sizing rule.
    $reserve = [regex]::Matches($Serial, '(?m)^MIRROR_MEM_RESERVE frames=([0-9]+)\r?$')
    if ($reserve.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_MEM_RESERVE(?:\s|$)').Count -ne 1 -or
        $reserve[0].Index -le $records[0].Index) {
        throw 'Mirror must report its memory reserve exactly once, after the frame pool.'
    }
    $floor = [uint64][math]::Floor($total / 16)
    if ($floor -gt 4096) { $floor = 4096 }
    if ([uint64]$reserve[0].Groups[1].Value -ne $floor) {
        throw 'The memory reserve does not match the floor sizing rule.'
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

function Get-MirrorDeviceArguments {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('V1', 'LegacyVirtio')][string]$Profile,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [switch]$NoTestDisk
    )
    # The QEMU device profiles of spec 0014. Both present the v1 NIC; they differ
    # only in the storage fixture. The caller keeps -nic none, the firmware-only
    # boot disk and the variant switches, and chooses the fixture's backend
    # (-drive or -blockdev) for the node id mirror-test.
    if ($Profile -eq 'LegacyVirtio' -and $NoTestDisk) {
        throw '-DeviceProfile LegacyVirtio attaches the legacy VirtIO test disk; it cannot be combined with -NoTestDisk.'
    }
    $root = (Resolve-Path -LiteralPath $RepositoryRoot).ProviderPath
    # The Intel 82545EM (8086:100f) on a user-mode backend, which avoids QEMU's
    # "has no peer" warning. restrict=on keeps the guest off the host network,
    # and an empty romfile keeps the iPXE option ROM out of firmware startup.
    $nicArguments = [string[]]@('-netdev', 'user,id=mirror-net,restrict=on', '-device', 'e1000-82545em,netdev=mirror-net,romfile=')
    $ahciFixture = $Profile -eq 'V1' -and -not $NoTestDisk
    $fixtureDevice = $null
    $fixtureImage = $null
    if ($ahciFixture) {
        # QEMU ide_init_drive requires a writable backend for ATA hard disks:
        # https://raw.githubusercontent.com/qemu/qemu/v10.1.0/hw/ide/core.c
        # Mirror exposes only IDENTIFY/reads. Attach a reset disposable copy, which
        # the caller verifies in full afterward instead of trusting ATA write protect.
        $source = Get-MirrorTestDisk -RepositoryRoot $root
        $fixtureImage = Join-Path (Join-Path $root 'build') 'ahci-test.img'
        Copy-Item -LiteralPath $source -Destination $fixtureImage -Force
        $fixtureDevice = 'ide-hd,drive=mirror-test,bus=ide.0,rerror=report'
    }
    elseif (-not $NoTestDisk) {
        $fixtureImage = Get-MirrorTestDisk -RepositoryRoot $root
        $fixtureDevice = 'virtio-blk-pci,drive=mirror-test,disable-modern=on,disable-legacy=off'
    }
    return [pscustomobject]@{
        NicArguments = $nicArguments
        FixtureDevice = $fixtureDevice
        FixtureImage = $fixtureImage
        AhciFixture = [bool]$ahciFixture
    }
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
