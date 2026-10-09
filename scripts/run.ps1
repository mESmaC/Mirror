[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Smoke,
    [switch]$NoBuild,
    [switch]$Graphical,
    [switch]$AgentDemo,
    [switch]$NoDisplayDevice,
    [switch]$NoTestDisk,
    [switch]$AhciDisk,
    [switch]$Xhci,
    [switch]$XhciKeyboard,
    [switch]$XhciMouse,
    [switch]$XhciPair,
    [switch]$XhciFullSpeed,
    [switch]$NoKeyboard,
    [switch]$NoHpet,
    [switch]$ExtendedCpu,
    [switch]$LegacyPic,
    [switch]$PitTimer,
    [ValidateRange(1, 64)][int]$CpuCount = 1,
    [ValidateSet('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout', 'BlockIoError', 'BlockWrite', 'BlockWriteError', 'BlockFlushError', 'BlockNoFlush', 'AhciIoError', 'AhciInitFailure', 'XhciInitFailure', 'XhciDeviceFailure', 'VmReadOnly', 'VmNx', 'VmReleased', 'VmGuard', 'UserX87', 'UserSimd', 'UserAvx', 'UserFsgsbase', 'ThreadLastExit', 'ThreadProcessExit', 'ThreadFault', 'SmpUnmap', 'SmpTlbTimeout', 'TimerCalibration', 'ClockActivation')][string]$Fault = 'None',
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 30,
    [ValidateRange(128, 32768)][int]$MemoryMiB = 256
)

function Assert-MirrorSmpMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(Mandatory = $true)][int]$RequestedCpus,
        [bool]$UseLegacyPic,
        [string]$FaultMode = 'None'
    )

    if ($Serial -match '(?m)^MIRROR_SMP_CPU_FAILED(?:\s|$)') {
        throw 'The SMP regression reported an unexpected CPU failure.'
    }
    if ([regex]::Matches($Serial, '(?m)^MIRROR_SMP_TESTS_OK\r?$').Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SMP_TESTS_OK(?:\s|$)').Count -ne 1) {
        throw 'Mirror must complete the SMP policy and mailbox checks exactly once.'
    }
    $sharedCpus = if ($UseLegacyPic) { 1 } else { $RequestedCpus }
    $sharedPattern = '(?m)^MIRROR_SHARED_VM_READY pages=512 cpus={0} rounds=4 reclaimed=3 borrowed_root=1\r?$' -f $sharedCpus
    $shared = [regex]::Matches($Serial, $sharedPattern)
    if ($shared.Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_READY(?:\s|$)').Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_TESTS_OK\r?$').Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_TESTS_OK(?:\s|$)').Count -ne 1 -or
        $Serial -match '(?m)^MIRROR_SHARED_VM_SKIPPED(?:\s|$)') {
        throw 'Shared kernel mappings must complete exact cross-CPU and borrowed-root reuse checks.'
    }
    if ($FaultMode -ne 'SmpTlbTimeout' -and $Serial -match '(?m)^MIRROR_(?:SMP_TLB_TIMEOUT_ARMED|SHARED_TIMEOUT_\w+)(?:\s|$)') {
        throw 'Unexpected TLB timeout injection.'
    }
    if ($UseLegacyPic) {
        if ($FaultMode -in @('SmpUnmap', 'SmpTlbTimeout')) {
            throw 'SMP memory fault probes require the APIC backend.'
        }
        if ([regex]::Matches($Serial, '(?m)^MIRROR_SMP_SKIPPED reason=legacy_pic online=1\r?$').Count -ne 1 -or
            [regex]::Matches($Serial, '(?m)^MIRROR_SMP_SKIPPED(?:\s|$)').Count -ne 1) {
            throw 'The legacy PIC policy must explicitly keep secondary CPUs offline.'
        }
        if ($Serial -match '(?m)^MIRROR_(?:SMP_(?:READY|CPU|IPI_OK|GUARD_OK|UNMAP_OK|TLB_TIMEOUT_ARMED)|SHARED_OFFLINE_OK)(?:\s|$)') {
            throw 'The legacy PIC regression unexpectedly started the SMP backend.'
        }
        return
    }

    if ($Serial -match '(?m)^MIRROR_SMP_SKIPPED(?:\s|$)') {
        throw 'The APIC regression unexpectedly skipped secondary-CPU startup.'
    }
    $readyPattern = '(?m)^MIRROR_SMP_READY discovered={0} online={0} bsp=(\d+) aps={1} userspace_cpus=1\r?$' -f $RequestedCpus, ($RequestedCpus - 1)
    $ready = [regex]::Matches($Serial, $readyPattern)
    if ($ready.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_SMP_READY(?:\s|$)').Count -ne 1) {
        throw "Mirror must bring exactly $RequestedCpus processors online while keeping userspace on the BSP."
    }
    if ([uint64]$ready[0].Groups[1].Value -ge 255) { throw 'Mirror reported an invalid BSP APIC ID.' }

    $cpuRecords = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_CPU slot=(\d+) apic=(\d+) stack=(0x[0-9a-f]+) cr3=(0x[0-9a-f]+)\r?$')
    $expectedSecondaries = $RequestedCpus - 1
    if ($cpuRecords.Count -ne $expectedSecondaries -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SMP_CPU(?:\s|$)').Count -ne $expectedSecondaries) {
        throw "Mirror must validate exactly $expectedSecondaries secondary CPU contexts."
    }
    $slots = [System.Collections.Generic.HashSet[int]]::new()
    $apicIds = [System.Collections.Generic.HashSet[uint32]]::new()
    $stacks = [System.Collections.Generic.HashSet[uint64]]::new()
    [void]$apicIds.Add([uint32]$ready[0].Groups[1].Value)
    $secondaryRoot = [uint64]0
    foreach ($cpuRecord in $cpuRecords) {
        $slot = [int]$cpuRecord.Groups[1].Value
        $apicIdentity = [uint32]$cpuRecord.Groups[2].Value
        $stackTop = [Convert]::ToUInt64($cpuRecord.Groups[3].Value.Substring(2), 16)
        $rootAddress = [Convert]::ToUInt64($cpuRecord.Groups[4].Value.Substring(2), 16)
        if ($slot -lt 1 -or $slot -ge $RequestedCpus -or -not $slots.Add($slot) -or
            $apicIdentity -ge 255 -or -not $apicIds.Add($apicIdentity) -or $stackTop -eq 0 -or ($stackTop % 16) -ne 0 -or
            -not $stacks.Add($stackTop) -or $rootAddress -eq 0 -or ($rootAddress % 4096) -ne 0) {
            throw 'Secondary CPU records must have unique slots, APIC IDs and aligned private stacks, with valid page-table roots.'
        }
        if ($secondaryRoot -eq 0) { $secondaryRoot = $rootAddress }
        elseif ($rootAddress -ne $secondaryRoot) {
            throw 'Secondary CPUs must use the same dedicated kernel page-table root.'
        }
    }

    $phases = @(
        @{ Name = 'boot'; Rounds = 8 },
        @{ Name = 'after_userspace'; Rounds = 4 },
        @{ Name = 'after_desktop'; Rounds = 4 }
    )
    if ([regex]::Matches($Serial, '(?m)^MIRROR_SMP_IPI_OK(?:\s|$)').Count -ne $phases.Count) {
        throw 'Mirror must complete exactly three IPI rendezvous phases.'
    }
    $previousPhase = $ready[0].Index
    foreach ($phase in $phases) {
        $phasePattern = '(?m)^MIRROR_SMP_IPI_OK phase={0} cpus={1} rounds={2}\r?$' -f $phase.Name, $RequestedCpus, $phase.Rounds
        $phaseRecords = [regex]::Matches($Serial, $phasePattern)
        if ($phaseRecords.Count -ne 1 -or $phaseRecords[0].Index -le $previousPhase) {
            throw "Mirror must verify $($phase.Rounds) ordered IPI rounds on all $RequestedCpus CPUs during $($phase.Name)."
        }
        if (($phase.Name -eq 'boot' -and $shared[0].Index -le $phaseRecords[0].Index) -or
            ($phase.Name -eq 'after_userspace' -and $shared[0].Index -ge $phaseRecords[0].Index)) {
            throw 'Shared mapping reuse checks must follow CPU startup and precede the completed userspace rendezvous.'
        }
        $previousPhase = $phaseRecords[0].Index
    }
    $guardRecords = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_GUARD_OK(?:\s|$)')
    $unmapRecords = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_UNMAP_OK(?:\s|$)')
    $offlineRecords = [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_OFFLINE_OK unchanged=1\r?$')
    if ([regex]::Matches($Serial, '(?m)^MIRROR_SHARED_OFFLINE_OK(?:\s|$)').Count -ne $offlineRecords.Count) {
        throw 'Malformed shared-mapping offline rejection marker.'
    }
    if ($FaultMode -eq 'SmpTlbTimeout') {
        if ($guardRecords.Count -ne 0 -or $unmapRecords.Count -ne 0 -or $offlineRecords.Count -ne 0) {
            throw 'The timeout probe must fail inside unmap before destructive fault tests or reclamation.'
        }
        $target = $RequestedCpus - 1
        $armedPattern = '(?m)^MIRROR_SMP_TLB_TIMEOUT_ARMED target={0}\r?$' -f $target
        $armed = [regex]::Matches($Serial, $armedPattern)
        $expected = [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_TIMEOUT_EXPECT address=0xffffd20000000000 frame=(0x[0-9a-f]+)\r?$')
        $panic = [regex]::Matches($Serial, '(?m)^MIRROR_PANIC:[^\r\n]*\r?\nkernel TLB shootdown acknowledgement timeout\r?$')
        if ($RequestedCpus -le 1 -or $armed.Count -ne 1 -or $expected.Count -ne 1 -or $panic.Count -ne 1 -or
            [regex]::Matches($Serial, '(?m)^MIRROR_SMP_TLB_TIMEOUT_ARMED(?:\s|$)').Count -ne 1 -or
            [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_TIMEOUT_\w+(?:\s|$)').Count -ne 1 -or
            [regex]::Matches($Serial, '(?im)^(?:MIRROR_PANIC|PANIC|\[panic\])(?::|\s|$)').Count -ne 1 -or
            [regex]::Matches($Serial, '(?m)^kernel TLB shootdown acknowledgement timeout\r?$').Count -ne 1 -or
            $armed[0].Index -le $previousPhase -or $expected[0].Index -le $armed[0].Index -or
            $panic[0].Index -le $expected[0].Index -or
            $Serial -match '(?m)^MIRROR_(?:PAGE_FAULT_OK|BOOT_OK|EXCEPTION)(?:\s|$)') {
            throw 'The missing-ACK test must panic inside shootdown after arming, before unmap returns or the frame is reclaimed.'
        }
        $retainedFrame = [Convert]::ToUInt64($expected[0].Groups[1].Value.Substring(2), 16)
        if ($retainedFrame -eq 0 -or ($retainedFrame % 4096) -ne 0) {
            throw 'The missing-ACK test must retain a valid allocated frame.'
        }
        return
    }
    if ($FaultMode -eq 'SmpUnmap') {
        $pattern = '(?m)^MIRROR_SMP_UNMAP_OK slot={0} vector=14 remaining={0}\r?$' -f ($RequestedCpus - 1)
        $records = [regex]::Matches($Serial, $pattern)
        if ($RequestedCpus -le 1 -or $guardRecords.Count -ne 0 -or $unmapRecords.Count -ne 1 -or
            $records.Count -ne 1 -or $records[0].Index -le $previousPhase -or $offlineRecords.Count -ne 1 -or
            $offlineRecords[0].Index -le $records[0].Index) {
            throw 'The released-alias probe must fault only its target and reject later shared mutations.'
        }
        return
    }
    if ($unmapRecords.Count -ne 0) { throw 'Unexpected released-alias fault injection.' }
    if ($RequestedCpus -gt 1 -and $offlineRecords.Count -ne 1) {
        throw 'A faulted secondary must prevent shared mapping mutations before PTE publication.'
    }
    if ($RequestedCpus -eq 1 -and $offlineRecords.Count -ne 0) {
        throw 'A uniprocessor boot unexpectedly rejected a missing secondary.'
    }
    if ($RequestedCpus -eq 1) {
        if ($guardRecords.Count -ne 0) {
            throw 'A single-CPU boot must not run a secondary guard-fault probe.'
        }
    }
    else {
        $guardPattern = '(?m)^MIRROR_SMP_GUARD_OK slot={0} vector=14 remaining={0}\r?$' -f ($RequestedCpus - 1)
        $guard = [regex]::Matches($Serial, $guardPattern)
        if ($guardRecords.Count -ne 1 -or $guard.Count -ne 1 -or $guard[0].Index -le $previousPhase) {
            throw 'A multicore boot must isolate one AP stack-guard fault after the ordinary IPI phases and verify every surviving CPU.'
        }
        if ($offlineRecords[0].Index -le $guard[0].Index) {
            throw 'Shared mutation rejection must be checked after the AP guard fault.'
        }
    }
}

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
if ($Fault -ne 'None' -and -not $Smoke) {
    throw '-Fault requires -Smoke; intentional faults are excluded from normal kernels.'
}
if ($Graphical -and ($Smoke -or $NoDisplayDevice)) {
    throw '-Graphical requires a normal boot with a display device.'
}
$writableBlock = $Fault -in @('BlockWrite', 'BlockWriteError', 'BlockFlushError', 'BlockNoFlush')
if ($AhciDisk -and $NoTestDisk) {
    throw '-AhciDisk requires the test disk; it cannot be combined with -NoTestDisk.'
}
if ($AhciDisk -and $Fault -like 'Block*') {
    throw '-AhciDisk cannot exercise legacy VirtIO block fault modes.'
}
if ($Fault -in @('AhciIoError', 'AhciInitFailure') -and -not $AhciDisk) {
    throw "-Fault $Fault requires -AhciDisk."
}
if ($Fault -eq 'XhciInitFailure' -and -not $Xhci) {
    throw '-Fault XhciInitFailure requires -Xhci.'
}
$usbFixtureCount = [int]$XhciKeyboard.IsPresent + [int]$XhciMouse.IsPresent + [int]$XhciPair.IsPresent
if ($usbFixtureCount -gt 1) {
    throw '-XhciKeyboard, -XhciMouse, and -XhciPair are mutually exclusive fixtures.'
}
if ($usbFixtureCount -ne 0 -and -not $Xhci) {
    throw 'USB fixtures require -Xhci.'
}
if ($XhciFullSpeed -and -not ($XhciKeyboard -or $XhciMouse)) {
    throw '-XhciFullSpeed requires one USB keyboard or mouse fixture.'
}
if ($usbFixtureCount -ne 0 -and $Fault -eq 'XhciInitFailure') {
    throw '-Fault XhciInitFailure requires an empty xHCI controller.'
}
if ($Fault -eq 'XhciDeviceFailure' -and (-not ($XhciKeyboard -or $XhciMouse) -or $NoHpet)) {
    throw '-Fault XhciDeviceFailure requires one USB keyboard or mouse and HPET.'
}
$usbFixture = if ($XhciKeyboard) { 'Keyboard' } elseif ($XhciMouse) { 'Mouse' } elseif ($XhciPair) { 'Pair' } else { 'None' }
if ($NoTestDisk -and ($Fault -eq 'BlockIoError' -or $writableBlock)) {
    throw "-Fault $Fault requires the test disk."
}
if ($Fault -in @('SmpUnmap', 'SmpTlbTimeout') -and ($CpuCount -lt 2 -or $LegacyPic)) {
    throw 'SMP memory fault probes require at least two CPUs and the APIC backend.'
}
if ($Fault -eq 'TimerCalibration' -and ($LegacyPic -or $PitTimer)) {
    throw 'Timer calibration fault injection requires APIC with automatic timer selection.'
}
if ($Fault -eq 'ClockActivation' -and $NoHpet) {
    throw 'Clock activation fault injection requires HPET hardware.'
}
$qemu = Get-MirrorQemu -RequestedPath $QemuPath -RepositoryRoot $repositoryRoot
$qemuDirectory = Split-Path -Parent $qemu
$codePath = Get-MirrorFirmware -RequestedPath $FirmwareCode -QemuDirectory $qemuDirectory -Names @('edk2-x86_64-code.fd', 'OVMF_CODE.fd', 'OVMF_CODE.4m.fd') -ParameterName 'FirmwareCode'
$varsTemplate = Get-MirrorFirmware -RequestedPath $FirmwareVars -QemuDirectory $qemuDirectory -Names @('edk2-i386-vars.fd', 'OVMF_VARS.fd', 'OVMF_VARS.4m.fd') -ParameterName 'FirmwareVars'

if (-not $NoBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release -Smoke:$Smoke -AgentDemo:$AgentDemo
}

$buildDirectory = Join-Path $repositoryRoot 'build'
$espDirectory = Join-Path $buildDirectory 'esp'
foreach ($relativePath in @('EFI/BOOT/BOOTX64.EFI', 'kernel.elf', 'init.elf', 'desktop.elf', 'agent.elf', 'app.elf', 'limine.conf')) {
    if (-not (Test-Path -LiteralPath (Join-Path $espDirectory $relativePath) -PathType Leaf)) {
        throw "The boot tree is incomplete: $relativePath. Run scripts/build.ps1 first."
    }
}

# All fault cases use the same smoke binary, selected by its boot command line.
# Always start from the repository config so -NoBuild cannot retain a stale mode.
$bootConfig = [System.IO.File]::ReadAllText((Join-Path $repositoryRoot 'boot/limine.conf'))
$testMode = switch ($Fault) {
    'PageFault' { 'page-fault' }
    'InvalidOpcode' { 'invalid-opcode' }
    'UserKernelRead' { 'user-kernel-read' }
    'UserCodeWrite' { 'user-code-write' }
    'UserNx' { 'user-nx' }
    'UserGuard' { 'user-guard' }
    'UserPrivileged' { 'user-privileged' }
    'UserBadStack' { 'user-bad-stack' }
    'UserBudget' { 'user-budget' }
    'IpcPeerExit' { 'ipc-peer-exit' }
    'IpcTimeout' { 'ipc-timeout' }
    'BlockIoError' { 'block-io-error' }
    'BlockWrite' { 'block-write' }
    'BlockWriteError' { 'block-write-error' }
    'BlockFlushError' { 'block-flush-error' }
    'BlockNoFlush' { 'block-no-flush' }
    'AhciIoError' { 'ahci-io-error' }
    'AhciInitFailure' { 'ahci-init-failure' }
    'XhciInitFailure' { 'xhci-init-failure' }
    'XhciDeviceFailure' { 'xhci-device-failure' }
    'VmReadOnly' { 'vm-read-only' }
    'VmNx' { 'vm-nx' }
    'VmReleased' { 'vm-released' }
    'VmGuard' { 'vm-guard' }
    'UserX87' { 'user-x87' }
    'UserSimd' { 'user-simd' }
    'UserAvx' { 'user-avx' }
    'UserFsgsbase' { 'user-fsgsbase' }
    'ThreadLastExit' { 'thread-last-exit' }
    'ThreadProcessExit' { 'thread-process-exit' }
    'ThreadFault' { 'thread-fault' }
    'SmpUnmap' { 'smp-unmap' }
    'SmpTlbTimeout' { 'smp-tlb-timeout' }
    'TimerCalibration' { 'timer-calibration' }
    'ClockActivation' { 'clock-activation' }
    default { $null }
}
$kernelArguments = @()
if ($null -ne $testMode) { $kernelArguments += "mirror.test=$testMode" }
if ($LegacyPic) { $kernelArguments += 'mirror.irq=pic' }
if ($PitTimer) { $kernelArguments += 'mirror.timer=pit' }
if ($kernelArguments.Count -gt 0) { $bootConfig += "`n    cmdline: $($kernelArguments -join ' ')`n" }
[System.IO.File]::WriteAllText((Join-Path $espDirectory 'limine.conf'), $bootConfig)

# Never write to the QEMU installation's firmware template. A fresh local copy
# also prevents stale firmware boot entries from changing the smoke-test path.
$varsPath = Join-Path $buildDirectory 'OVMF_VARS.fd'
Copy-Item -LiteralPath $varsTemplate -Destination $varsPath -Force
$serialLog = Join-Path $buildDirectory 'serial.log'
$stdoutLog = Join-Path $buildDirectory 'qemu.stdout.log'
$stderrLog = Join-Path $buildDirectory 'qemu.stderr.log'

$cpuModel = if ($ExtendedCpu) { 'max' } else { 'qemu64' }
$machine = 'q35,accel=tcg'
if ($NoKeyboard) { $machine += ',i8042=off,vmport=off' }
if ($NoHpet) { $machine += ',hpet=off' }
$qemuArguments = @(
    '-machine', $machine,
    '-cpu', $cpuModel,
    '-m', "$MemoryMiB",
    '-smp', "$CpuCount",
    '-vga', $(if ($NoDisplayDevice) { 'none' } else { 'std' }),
    '-monitor', 'none',
    '-nic', 'none',
    '-no-reboot',
    '-drive', "if=pflash,unit=0,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $codePath)",
    '-drive', "if=pflash,unit=1,format=raw,file=$(ConvertTo-QemuOptionValue $varsPath)",
    '-drive', "if=none,id=mirror-esp,format=raw,readonly=on,file=fat:$(ConvertTo-QemuOptionValue $espDirectory)",
    '-device', 'virtio-blk-pci,drive=mirror-esp,disable-legacy=on,bootindex=0'
)
if ($Xhci) { $qemuArguments += @('-device', 'qemu-xhci,id=mirror-xhci') }
# Pin the USB descriptor version and physical port assignment. Both HID models
# expose one configuration/interface; their interface protocols distinguish them.
# https://raw.githubusercontent.com/qemu/qemu/v10.1.0/hw/usb/dev-hid.c
$usbVersion = if ($XhciFullSpeed) { 1 } else { 2 }
if ($XhciKeyboard -or $XhciPair) {
    $qemuArguments += @('-device', "usb-kbd,bus=mirror-xhci.0,port=1,usb_version=$usbVersion")
}
if ($XhciMouse -or $XhciPair) {
    $mousePort = if ($XhciPair) { 2 } else { 1 }
    $qemuArguments += @('-device', "usb-mouse,bus=mirror-xhci.0,port=$mousePort,usb_version=$usbVersion")
}
if (-not $NoTestDisk) {
    $testDisk = Get-MirrorTestDisk -RepositoryRoot $repositoryRoot
    $testDiskHash = (Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash
    $attachedDisk = $testDisk
    if ($AhciDisk) {
        # QEMU ide_init_drive requires a writable backend for ATA hard disks:
        # https://raw.githubusercontent.com/qemu/qemu/v10.1.0/hw/ide/core.c
        # Mirror exposes only IDENTIFY/reads. Use a reset disposable copy and
        # verify its entire image afterward instead of trusting ATA write protect.
        $attachedDisk = Join-Path $buildDirectory 'ahci-test.img'
        Copy-Item -LiteralPath $testDisk -Destination $attachedDisk -Force
    }
    $testDevice = if ($AhciDisk) { 'ide-hd,drive=mirror-test,bus=ide.0,rerror=report' }
        else { 'virtio-blk-pci,drive=mirror-test,disable-modern=on,disable-legacy=off' }
    if ($writableBlock) {
        $writableDisk = New-MirrorWritableTestDisk -RepositoryRoot $repositoryRoot
        $fileNode = @{ driver = 'file'; filename = $writableDisk; cache = @{ 'no-flush' = $false } }
        if ($Fault -in @('BlockWriteError', 'BlockFlushError')) {
            $fileNode = @{ driver = 'blkdebug'; image = $fileNode }
            if ($Fault -eq 'BlockWriteError') {
                $fileNode['inject-error'] = @(@{ event = 'write_aio'; errno = 5; sector = 1234; once = $true })
            }
            else {
                # State 1 survives firmware flushes. The first data write arms
                # a single flush error in state 2; retries then reach the file.
                # https://www.qemu.org/docs/master/devel/testing/blkdebug.html
                $fileNode['set-state'] = @(@{ event = 'write_aio'; state = 1; new_state = 2 })
                $fileNode['inject-error'] = @(@{ event = 'flush_to_disk'; state = 2; errno = 5; once = $true })
            }
        }
        $testNode = @{
            driver = 'raw'; 'node-name' = 'mirror-test'; 'read-only' = $false
            cache = @{ 'no-flush' = $false }; file = $fileNode
        } | ConvertTo-Json -Compress -Depth 8
        $qemuArguments += @('-blockdev', $testNode)
        $testDevice += ',queue-size=256,werror=report,rerror=report'
        if ($Fault -eq 'BlockNoFlush') {
            # WCE is legacy FLUSH bit 9. Disable both sources of that feature
            # in QEMU's virtio_blk_get_features, not just the guest's mask.
            $testDevice += ',write-cache=off,config-wce=off,x-enable-wce-if-config-wce=off'
        }
        else { $testDevice += ',write-cache=on' }
    }
    elseif ($Fault -in @('BlockIoError', 'AhciIoError')) {
        # Inject into a data sector used by the kernel probe, away from firmware
        # partition discovery. JSON preserves paths without blkdebug URL colons.
        $testNode = @{
            driver = 'raw'; 'node-name' = 'mirror-test'; 'read-only' = (-not $AhciDisk)
            file = @{
                driver = 'blkdebug'
                image = @{ driver = 'file'; filename = $attachedDisk }
                'inject-error' = @(@{ event = 'read_aio'; errno = 5; sector = 1234; once = $true })
            }
        } | ConvertTo-Json -Compress -Depth 6
        $qemuArguments += @('-blockdev', $testNode)
    }
    else {
        $readOnlyOption = if ($AhciDisk) { 'off' } else { 'on' }
        $qemuArguments += @('-drive', "if=none,id=mirror-test,format=raw,readonly=$readOnlyOption,file=$(ConvertTo-QemuOptionValue $attachedDisk)")
    }
    $qemuArguments += @('-device', $testDevice)
}
if (-not $Graphical) { $qemuArguments += @('-display', 'none') }
if (Test-Path -LiteralPath (Join-Path $qemuDirectory 'share') -PathType Container) {
    $qemuArguments += @('-L', (Join-Path $qemuDirectory 'share'))
}

if (-not $Smoke) {
    Write-Host 'Booting Mirror. Serial output follows; press Ctrl+C to stop QEMU.'
    & $qemu @qemuArguments '-no-shutdown' '-serial' 'stdio'
    if ($LASTEXITCODE -ne 0) { throw "QEMU exited with code $LASTEXITCODE." }
    exit 0
}

[System.IO.File]::WriteAllText($serialLog, '')
$qemuArguments += @('-serial', "file:$serialLog", '-device', 'isa-debug-exit,iobase=0xf4,iosize=0x04')
$processArguments = @($qemuArguments | ForEach-Object { ConvertTo-WindowsProcessArgument $_ })
$start = [System.Diagnostics.ProcessStartInfo]::new()
$start.FileName = $qemu
$start.Arguments = $processArguments -join ' '
$start.WorkingDirectory = $repositoryRoot
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $start
$started = $false
$stdoutStream = $null
$stderrStream = $null
$stdoutCopy = $null
$stderrCopy = $null
try {
    $stdoutStream = [System.IO.File]::Create($stdoutLog)
    $stderrStream = [System.IO.File]::Create($stderrLog)
    if (-not $process.Start()) { throw 'Could not start QEMU.' }
    $started = $true
    # Drain both pipes concurrently so either stream can exceed the pipe buffer.
    # Owning the Process directly also preserves ExitCode on Windows PowerShell 5.1.
    $stdoutCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdoutStream)
    $stderrCopy = $process.StandardError.BaseStream.CopyToAsync($stderrStream)
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "Mirror boot smoke test exceeded $TimeoutSeconds seconds. See $serialLog and $stderrLog."
    }
    $process.WaitForExit()
    $qemuExitCode = $process.ExitCode
    $null = $stdoutCopy.GetAwaiter().GetResult()
    $null = $stderrCopy.GetAwaiter().GetResult()
    $stdoutStream.Dispose()
    $stdoutStream = $null
    $stderrStream.Dispose()
    $stderrStream = $null
    $serial = [System.IO.File]::ReadAllText($serialLog)
    if (-not $NoTestDisk -and (Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash -ne $testDiskHash) {
        throw 'The read-only block fixture was modified during the boot.'
    }
    if ($AhciDisk -and ((Get-Item -LiteralPath $attachedDisk).Length -ne 1048576 -or
            (Get-FileHash -LiteralPath $attachedDisk -Algorithm SHA256).Hash -ne $testDiskHash)) {
        throw 'The AHCI read-only probe modified its disposable disk image.'
    }
    $expectedTlbTimeout = $Fault -eq 'SmpTlbTimeout'
    if (-not $expectedTlbTimeout -and $serial -match '(?im)^(?:MIRROR_PANIC|PANIC|\[panic\])') {
        throw "Mirror reported a panic. See $serialLog."
    }
    $fatalKernelFault = $Fault -in @('PageFault', 'InvalidOpcode')
    $expectedExitCode = if ($fatalKernelFault -or $expectedTlbTimeout) { 35 } else { 33 }
    if ($qemuExitCode -ne $expectedExitCode) {
        $qemuError = [System.IO.File]::ReadAllText($stderrLog)
        throw "Mirror smoke test expected QEMU exit $expectedExitCode; got $qemuExitCode. $qemuError See $serialLog."
    }
    if ($writableBlock) {
        Assert-MirrorWritableTestDisk -OriginalPath $testDisk -WritablePath $writableDisk -WritesExpected ($Fault -ne 'BlockNoFlush')
    }
    $markers = @('MIRROR_FRAMES_OK', 'MIRROR_ALLOCATOR_TESTS_OK', 'MIRROR_FRAME_REUSE_OK', 'MIRROR_EXCEPTIONS_OK', 'MIRROR_PAGING_OK', 'MIRROR_HEAP_OK', 'MIRROR_TIMER_OK', 'MIRROR_HEAP_IRQ_OK', 'MIRROR_USER_PAGING_OK', 'MIRROR_ELF_TESTS_OK', 'MIRROR_USERSPACE_OK', 'MIRROR_USER_CLEANUP_OK', 'MIRROR_USER_LOAD_ROLLBACK_OK', 'MIRROR_SCHEDULER_OK', 'MIRROR_CAPABILITY_TESTS_OK', 'MIRROR_IPC_TESTS_OK', 'MIRROR_IPC_OK')
    $markers += @('MIRROR_PCI_TESTS_OK', 'MIRROR_PCI_READY', 'MIRROR_DRIVER_TESTS_OK', 'MIRROR_FRAMEBUFFER_TESTS_OK')
    $markers += @('MIRROR_ACPI_MAPPING_TESTS_OK', 'MIRROR_APIC_PREFLIGHT_TESTS_OK')
    $markers += @('MIRROR_SURFACE_TESTS_OK', 'MIRROR_USER_SURFACE_CLEANUP_OK')
    $markers += @('MIRROR_CONTIGUOUS_TESTS_OK', 'MIRROR_DMA_TESTS_OK', 'MIRROR_BLOCK_TESTS_OK')
    $markers += @('MIRROR_KEYBOARD_TESTS_OK', 'MIRROR_CONSOLE_TESTS_OK')
    $markers += @('MIRROR_PS2_TESTS_OK', 'MIRROR_MOUSE_TESTS_OK', 'MIRROR_POINTER_TESTS_OK')
    $markers += @('MIRROR_INPUT_TESTS_OK', 'MIRROR_SERVICE_POLICY_TESTS_OK')
    $markers += @('MIRROR_USER_VM_PAGING_OK', 'MIRROR_VM_TESTS_OK', 'MIRROR_USER_VM_OK', 'MIRROR_VM_CLEANUP_OK')
    $markers += @('MIRROR_FPU_READY', 'MIRROR_FPU_TESTS_OK', 'MIRROR_USER_FPU_OK')
    $markers += @('MIRROR_TLS_READY', 'MIRROR_TLS_KERNEL_OK', 'MIRROR_USER_TLS_OK')
    $markers += 'MIRROR_ACPI_READY'
    # Firmware must describe every configured CPU. The APIC backend then starts
    # each secondary with a private stack and verifies repeated IPI delivery.
    if ($serial -notmatch "(?m)^MIRROR_ACPI_READY [^\r\n]* cpus=$CpuCount enabled=$CpuCount(?:\s|$)") {
        throw "ACPI discovery must report exactly $CpuCount enabled processors."
    }
    if ($LegacyPic) {
        $markers += 'MIRROR_APIC_SKIPPED reason=boot_policy'
        if ($serial -match '(?m)^MIRROR_APIC_(?:READY|SPURIOUS_OK)(?:\s|$)') {
            throw 'The legacy PIC regression unexpectedly enabled the APIC backend.'
        }
    }
    else {
        $markers += @('MIRROR_APIC_READY', 'MIRROR_APIC_SPURIOUS_OK')
        $markers += @(
            'MIRROR_APIC_IRQ irq=0 gsi=2 vector=32 polarity=high trigger=edge',
            'MIRROR_APIC_IRQ irq=1 gsi=1 vector=33 polarity=high trigger=edge',
            'MIRROR_APIC_IRQ irq=12 gsi=12 vector=44 polarity=high trigger=edge'
        )
        if ($serial -notmatch '(?m)^MIRROR_APIC_READY mode=xapic bsp=\d+ ioapics=[1-9]\d* routes=3(?:\s|$)') {
            throw 'Mirror must initialize xAPIC and the three ISA interrupt routes from firmware topology.'
        }
    }
    Assert-MirrorTimer -Serial $serial -Smoke $true -UseLegacyPic ([bool]$LegacyPic) -ForcePit ([bool]$PitTimer) -FaultMode $Fault
    Assert-MirrorClock -Serial $serial -Smoke $true -WithoutHpet ([bool]$NoHpet) -FaultMode $Fault
    Assert-MirrorSmpMarkers -Serial $serial -RequestedCpus $CpuCount -UseLegacyPic ([bool]$LegacyPic) -FaultMode $Fault
    Assert-MirrorHeapGrowth -Serial $serial -RequestedCpus $CpuCount -Smoke $true -UseLegacyPic ([bool]$LegacyPic)
    Assert-MirrorDriverCatalog -Serial $serial -Smoke $true -LegacyDisk (-not $NoTestDisk -and -not $AhciDisk) -XhciAttached ([bool]$Xhci)
    Assert-MirrorAhciMarkers -Serial $serial -Attached ([bool]$AhciDisk) -Smoke $true -FaultMode $Fault
    Assert-MirrorXhciMarkers -Serial $serial -Attached ([bool]$Xhci) -UsbDevice $usbFixture -WithoutHpet ([bool]$NoHpet) -Smoke $true -FaultMode $Fault
    Assert-MirrorUsbMarkers -Serial $serial -Attached ([bool]$Xhci) -UsbDevice $usbFixture -FullSpeed ([bool]$XhciFullSpeed) -WithoutHpet ([bool]$NoHpet) -Smoke $true -FaultMode $Fault
    $markers += 'MIRROR_USER_SYNC_OK'
    if (@([regex]::Matches($serial, '(?m)^user: Mirror init: synchronization checks passed\\n\r?$')).Count -ne 4) {
        throw 'Both native processes must complete their synchronization checks before and after teardown/relaunch.'
    }
    if (@([regex]::Matches($serial, '(?m)^MIRROR_USER_SYNC_OK(?:\s|$)')).Count -ne 4) {
        throw 'Both native process runs must validate address waits, wakeups, and synchronization cleanup.'
    }
    if ($serial -notmatch '(?m)^MIRROR_TLS_READY[^\r\n]* fsgsbase=0(?:\s|$)') {
        throw 'Mirror must keep direct user FS/GS base instructions disabled.'
    }
    if ($ExtendedCpu -and $serial -notmatch '(?m)^MIRROR_TLS_READY[^\r\n]* cpuid_fsgsbase=1(?:\s|$)') {
        throw 'The extended CPU regression requires guest-visible FSGSBASE support.'
    }
    if (@([regex]::Matches($serial, '(?m)^user: Mirror init: TLS checks passed\\n\r?$')).Count -ne 4) {
        throw 'Both native processes must complete their TLS checks before and after teardown/relaunch.'
    }
    if (@([regex]::Matches($serial, '(?m)^MIRROR_USER_TLS_OK(?:\s|$)')).Count -ne 4) {
        throw 'Both native process runs must validate independent FS/GS context state.'
    }
    if (-not $fatalKernelFault) {
        $markers += 'MIRROR_USER_THREADS_OK'
        if (@([regex]::Matches($serial, '(?m)^user: Mirror init: thread checks passed\\n\r?$')).Count -ne 4) {
            throw 'Both native processes must complete their thread checks before and after teardown/relaunch.'
        }
        if (@([regex]::Matches($serial, '(?m)^MIRROR_USER_THREADS_OK(?:\s|$)')).Count -ne 4) {
            throw 'Both native process runs must validate shared resources and independent thread contexts.'
        }
    }
    if ($serial -notmatch '(?m)^MIRROR_FPU_READY .*osxsave=0(?:\s|$)') {
        throw 'Mirror must keep unmanaged XSAVE state disabled.'
    }
    if ($ExtendedCpu -and $serial -notmatch '(?m)^MIRROR_FPU_READY .*cpuid_avx=1(?:\s|$)') {
        throw 'The extended CPU regression requires guest-visible AVX support.'
    }
    if (@([regex]::Matches($serial, '(?m)^user: Mirror init: FPU checks passed\\n\r?$')).Count -ne 4) {
        throw 'Both native processes must complete their FPU checks before and after teardown/relaunch.'
    }
    if (@([regex]::Matches($serial, '(?m)^MIRROR_USER_FPU_OK(?:\s|$)')).Count -ne 4) {
        throw 'Both native process runs must validate independent floating-point and SIMD state.'
    }
    if (@([regex]::Matches($serial, '(?m)^user: Mirror init: VM checks passed\\n\r?$')).Count -ne 4) {
        throw 'Both native processes must complete their VM checks before and after teardown/relaunch.'
    }
    if (@([regex]::Matches($serial, '(?m)^MIRROR_VM_CLEANUP_OK(?:\s|$)')).Count -ne 2) {
        throw 'Both native process runs must reclaim their anonymous memory.'
    }
    if ($NoKeyboard -or $NoDisplayDevice) {
        $markers += 'MIRROR_DESKTOP_SKIPPED reason=missing_display_or_input'
        if ($serial -match '(?m)^MIRROR_(?:DESKTOP|AGENT|SHELL)_(?:READY|TESTS_OK|CLEANUP_OK)(?:\s|$)') {
            throw "Mirror unexpectedly ran a desktop without input/display. See $serialLog."
        }
    } elseif ($AgentDemo) {
        $markers += @('MIRROR_DESKTOP_TESTS_OK', 'MIRROR_DESKTOP_INPUT_TIMEOUT', 'MIRROR_DESKTOP_CLEANUP_OK reason=fault handles_reclaimed=1')
        $markers += @('MIRROR_AGENT_READY', 'MIRROR_AGENT_TESTS_OK', 'MIRROR_AGENT_CLEANUP_OK reason=exit handles_reclaimed=1 status=0')
        if ($serial -notmatch '(?m)^user: Mirror desktop: agent operations passed\\n\r?$') {
            throw 'Native desktop did not verify the agent operation results.'
        }
        if ($serial -match '(?m)^MIRROR_DESKTOP_SKIPPED(?:\s|$)') {
            throw "Mirror skipped its desktop lifecycle checks. See $serialLog."
        }
    } else {
        $markers += @('MIRROR_WINDOW_TESTS_OK', 'MIRROR_APP_LOAD_ROLLBACK_OK', 'MIRROR_SHELL_TESTS_OK', 'MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1')
        if ($serial -notmatch '(?m)^user: Mirror shell: checks passed\\n\r?$') {
            throw 'Native shell did not finish its application lifecycle checks.'
        }
        if ($serial -match '(?m)^MIRROR_AGENT_(?:READY|TESTS_OK)(?:\s|$)') {
            throw 'Default desktop unexpectedly started the agent demo.'
        }
        # A published busy frame can survive the loop until its next commit.
        # Bracket the supervisor's observation of the committed peer edit with
        # the actual busy handler diagnostics. The app's own text log follows
        # its commit and can be delayed until after the supervisor closes peers.
        $busy = [regex]::Match($serial, '(?m)^user: Mirror app: busy start ([1-9][0-9]*)\\n\r?$')
        $progress = [regex]::Match($serial, '(?m)^user: Mirror shell: busy peer progress ([1-9][0-9]*) ([1-9][0-9]*)\\n\r?$')
        $peerText = [regex]::Match($serial, '(?m)^user: Mirror app: text ([1-9][0-9]*) abc\\n\r?$')
        if (-not $busy.Success -or -not $progress.Success -or -not $peerText.Success -or
            $progress.Index -le $busy.Index -or
            $progress.Groups[1].Value -eq $progress.Groups[2].Value -or
            $progress.Groups[2].Value -ne $busy.Groups[1].Value -or
            $progress.Groups[1].Value -ne $peerText.Groups[1].Value) {
            throw 'Native shell did not show an independent app progressing during the busy probe.'
        }
        $busyPid = $busy.Groups[1].Value
        $stop = [regex]::Match($serial, "(?m)^(?:user: Mirror app: busy done $busyPid\\n\r?`$|MIRROR_APP_RECLAIMED pid=$busyPid(?:\s|`$))")
        if (-not $stop.Success -or $stop.Index -le $progress.Index) {
            throw 'The busy app stopped before the peer edit completed.'
        }
    }
    if ($NoKeyboard) {
        $markers += @('MIRROR_KEYBOARD_SKIPPED', 'MIRROR_MOUSE_SKIPPED')
        if ($serial -match '(?m)^MIRROR_(?:KEYBOARD|MOUSE)_READY(?:\s|$)') {
            throw "Mirror unexpectedly initialized the absent PS/2 controller. See $serialLog."
        }
    }
    else {
        $markers += @('MIRROR_KEYBOARD_READY', 'MIRROR_MOUSE_READY')
        if ($serial -match '(?m)^MIRROR_(?:KEYBOARD|MOUSE)_SKIPPED(?:\s|$)') {
            throw "Mirror skipped a PS/2 device. See $serialLog."
        }
    }
    if ($serial -match '(?m)^MIRROR_BLOCK_FAILED(?:\s|$)') {
        throw "Mirror block driver failed. See $serialLog."
    }
    if ($writableBlock) {
        Assert-MirrorWritableBlockMarkers -Serial $serial -FaultMode $Fault
    }
    elseif ($NoTestDisk -or $AhciDisk) {
        $markers += 'MIRROR_BLOCK_SKIPPED'
        if ($serial -match '(?m)^MIRROR_BLOCK_(?:READY|READ_OK|IO_ERROR_OK)(?:\s|$)') {
            throw "Mirror unexpectedly bound a block device. See $serialLog."
        }
    }
    else {
        $markers += @('MIRROR_BLOCK_READY', 'MIRROR_BLOCK_READ_OK', 'MIRROR_BLOCK_CLEANUP_OK')
        if ($serial -match '(?m)^MIRROR_BLOCK_SKIPPED(?:\s|$)') {
            throw "Mirror skipped the attached block fixture. See $serialLog."
        }
    }
    if (-not $writableBlock -and $serial -match '(?m)^MIRROR_BLOCK_(?:WRITABLE_READY|POLICY_OK|NO_FLUSH_OK|WRITE_OK|FLUSH_OK|DIRTY_SHUTDOWN_OK|PERSIST_OK|WRITE_ERROR_OK|FLUSH_ERROR_OK)(?:\s|$)') {
        throw "Mirror unexpectedly ran a writable storage probe. See $serialLog."
    }
    if ($Fault -eq 'BlockIoError') { $markers += 'MIRROR_BLOCK_IO_ERROR_OK' }
    elseif ($serial -match '(?m)^MIRROR_BLOCK_IO_ERROR_OK(?:\s|$)') {
        throw "Mirror unexpectedly reported a block I/O error probe. See $serialLog."
    }
    if ($serial -notmatch '(?m)^MIRROR_PCI_READY .*virtio_block=[1-9][0-9]*(?:\s|$)') {
        throw "Mirror did not discover its VirtIO boot disk. See $serialLog."
    }
    if ($NoDisplayDevice) {
        $markers += 'MIRROR_USER_SURFACE_SKIPPED reason=no_display'
        if ($serial -match '(?m)^MIRROR_USER_SURFACE_OK(?:\s|$)') {
            throw "Mirror granted a surface without a display. See $serialLog."
        }
        if ($serial -notmatch '(?m)^MIRROR_FRAMEBUFFER_SKIPPED reason=missing(?:\s|$)' -or
            $serial -notmatch '(?m)^MIRROR_PCI_READY .*display=0(?:\s|$)' -or
            $serial -match '(?m)^MIRROR_FRAMEBUFFER_(?:READY|PRESENTED)(?:\s|$)') {
            throw "Mirror did not complete the no-display fallback. See $serialLog."
        }
    }
    else {
        $markers += @('MIRROR_FRAMEBUFFER_READY', 'MIRROR_FRAMEBUFFER_PRESENTED')
        $markers += 'MIRROR_USER_SURFACE_OK'
        if ($serial -match '(?m)^MIRROR_USER_SURFACE_SKIPPED(?:\s|$)') {
            throw "Mirror skipped its userspace surface checks. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_FRAMEBUFFER_SKIPPED(?:\s|$)' -or
            $serial -notmatch '(?m)^MIRROR_PCI_READY .*display=[1-9][0-9]*(?:\s|$)') {
            throw "Mirror did not use its display device. See $serialLog."
        }
    }
    if (@([regex]::Matches($serial, '(?m)^MIRROR_USER_SURFACE_CLEANUP_OK handles_reclaimed=1\r?$')).Count -ne 2) {
        throw "Both native process runs must reclaim their surface handles. See $serialLog."
    }
    $scenario = switch ($Fault) {
        'UserKernelRead' { 1 }
        'UserCodeWrite' { 2 }
        'UserNx' { 3 }
        'UserGuard' { 4 }
        'UserPrivileged' { 5 }
        'UserBadStack' { 6 }
        'UserBudget' { 7 }
        'IpcPeerExit' { 8 }
        'IpcTimeout' { 9 }
        'VmReadOnly' { 10 }
        'VmNx' { 11 }
        'VmReleased' { 12 }
        'VmGuard' { 13 }
        'UserX87' { 14 }
        'UserSimd' { 15 }
        'UserAvx' { 16 }
        'UserFsgsbase' { 17 }
        'ThreadLastExit' { 18 }
        'ThreadProcessExit' { 19 }
        'ThreadFault' { 20 }
        default { 0 }
    }
    if ($serial -notmatch "(?m)^MIRROR_USERSPACE_OK scenario=$scenario(?:\s|$)") {
        throw "Mirror did not finish userspace scenario $scenario. See $serialLog."
    }
    if ($Fault -eq 'UserBadStack') {
        if ($serial -notmatch '(?m)^MIRROR_USER_RETURN_REJECTED(?:\s|$)') {
            throw "Mirror did not reject the invalid userspace return stack. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_USER_FAULT_CONTAINED(?:\s|$)') {
            throw "Mirror faulted instead of rejecting the invalid userspace return stack. See $serialLog."
        }
    }
    elseif ($Fault -eq 'UserBudget') {
        if ($serial -notmatch '(?m)^MIRROR_USER_BUDGET_CONTAINED(?:\s|$)') {
            throw "Mirror did not stop the CPU-bound process at its timer-interrupt budget. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_USER_FAULT_CONTAINED(?:\s|$)') {
            throw "Mirror faulted instead of containing the CPU-bound process. See $serialLog."
        }
    }
    elseif ($Fault -in @('IpcPeerExit', 'IpcTimeout')) {
        $ipcMarker = if ($Fault -eq 'IpcPeerExit') { 'MIRROR_IPC_PEER_CLOSED_OK' } else { 'MIRROR_IPC_TIMEOUT_OK' }
        if ($serial -notmatch "(?m)^$ipcMarker(?:\s|$)") {
            throw "Mirror did not complete the intended $Fault recovery. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_USER_FAULT_CONTAINED(?:\s|$)') {
            throw "Mirror faulted instead of completing the intended $Fault recovery. See $serialLog."
        }
    }
    elseif ($Fault -in @('ThreadLastExit', 'ThreadProcessExit')) {
        if ($serial -match '(?m)^MIRROR_USER_FAULT_CONTAINED(?:\s|$)') {
            throw "Mirror faulted instead of completing the intended $Fault lifecycle. See $serialLog."
        }
    }
    elseif ($scenario -ne 0) {
        $userVector = switch ($Fault) {
            'UserPrivileged' { 13 }
            'UserX87' { 16 }
            'UserSimd' { 19 }
            'UserAvx' { 6 }
            'UserFsgsbase' { 6 }
            'ThreadFault' { 6 }
            default { 14 }
        }
        if ($serial -notmatch "(?m)^MIRROR_USER_FAULT_CONTAINED vector=$userVector(?:\s|$)") {
            throw "Mirror did not contain the intended $Fault exception. See $serialLog."
        }
        if ($Fault -in @('VmReadOnly', 'VmNx', 'VmReleased', 'VmGuard')) {
            $vmError = switch ($Fault) { 'VmReadOnly' { '0x7' } 'VmNx' { '0x15' } default { '0x4' } }
            if ($serial -notmatch "(?m)^MIRROR_USER_FAULT_CONTAINED vector=14 error=$vmError cr2=0x102000000\r?`$") {
                throw "Mirror did not report the precise anonymous-memory protection fault for $Fault. See $serialLog."
            }
        }
        elseif ($Fault -in @('UserX87', 'UserSimd', 'UserAvx', 'UserFsgsbase', 'ThreadFault')) {
            if ($serial -notmatch "(?m)^MIRROR_USER_FAULT_CONTAINED vector=$userVector error=0x0(?:\s|`$)") {
                throw "Mirror did not report the precise processor-state exception for $Fault. See $serialLog."
            }
        }
    }
    elseif ($serial -match '(?m)^MIRROR_USER_FAULT_CONTAINED(?:\s|$)') {
        throw "Mirror reported an unexpected userspace fault. See $serialLog."
    }
    if ($Fault -ne 'UserBadStack' -and $serial -match '(?m)^MIRROR_USER_RETURN_REJECTED(?:\s|$)') {
        throw "Mirror unexpectedly rejected a userspace return stack. See $serialLog."
    }
    if ($Fault -ne 'UserBudget' -and $serial -match '(?m)^MIRROR_USER_BUDGET_CONTAINED(?:\s|$)') {
        throw "Mirror unexpectedly exhausted a process timer-interrupt budget. See $serialLog."
    }
    if ($Fault -ne 'IpcPeerExit' -and $serial -match '(?m)^MIRROR_IPC_PEER_CLOSED_OK(?:\s|$)') {
        throw "Mirror unexpectedly reported the IPC peer-exit scenario. See $serialLog."
    }
    if ($Fault -ne 'IpcTimeout' -and $serial -match '(?m)^MIRROR_IPC_TIMEOUT_OK(?:\s|$)') {
        throw "Mirror unexpectedly reported the IPC timeout scenario. See $serialLog."
    }
    $threadScenarios = @{
        ThreadLastExit = 'MIRROR_THREAD_LAST_EXIT_OK'
        ThreadProcessExit = 'MIRROR_THREAD_PROCESS_EXIT_OK'
        ThreadFault = 'MIRROR_THREAD_FAULT_OK'
    }
    foreach ($threadScenario in $threadScenarios.Keys) {
        $threadMarker = $threadScenarios[$threadScenario]
        $expectedCount = if ($Fault -eq $threadScenario) { 1 } else { 0 }
        if (@([regex]::Matches($serial, "(?m)^$threadMarker(?:\s|$)")).Count -ne $expectedCount) {
            throw "Mirror must report $threadMarker exactly $expectedCount time(s). See $serialLog."
        }
    }
    if (-not $expectedTlbTimeout -and -not $fatalKernelFault) {
        $markers += @('MIRROR_PAGE_FAULT_OK', 'MIRROR_BOOT_OK')
        if ($serial -match '(?im)^(?:EXCEPTION|MIRROR_EXCEPTION)(?:\s|:|$)') {
            throw "Mirror reported an unexpected exception. See $serialLog."
        }
    }
    elseif ($fatalKernelFault) {
        $vector = if ($Fault -eq 'PageFault') { 14 } else { 6 }
        $diagnostic = "(?m)^MIRROR_EXCEPTION vector=$vector error=0x0 rip=0x[0-9a-f]+ cr2="
        if ($Fault -eq 'PageFault') { $diagnostic += '0xffffc00000000000(?:\s|$)' }
        else { $diagnostic += '0x[0-9a-f]+(?:\s|$)' }
        if ($serial -notmatch $diagnostic -or $serial -notmatch "(?m)^MIRROR_TEST_EXPECT_FATAL vector=$vector(?:\s|$)") {
            throw "Mirror did not report the intended $Fault diagnostic. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_BOOT_OK(?:\s|$)') {
            throw "Fatal-fault test incorrectly reached normal boot completion. See $serialLog."
        }
    }
    foreach ($marker in $markers) {
        if ($serial -notmatch "(?m)^$marker(?:\s|$)") {
            throw "Mirror smoke test did not report $marker. See $serialLog."
        }
    }
    Write-Host "Mirror boot smoke test passed (fault=$Fault, ahci-disk=$AhciDisk, xhci=$Xhci, usb=$usbFixture, usb-full-speed=$XhciFullSpeed, no-display=$NoDisplayDevice, extended-cpu=$ExtendedCpu, cpus=$CpuCount, legacy-pic=$LegacyPic, pit-timer=$PitTimer, no-hpet=$NoHpet, QEMU exit $expectedExitCode). Serial log: $serialLog"
    # Keep firmware's terminal control sequences in the raw log, not tool output.
    Write-Output (($serial -split '\r?\n' | Where-Object { $_ -match '^(MIRROR_|Memory map:|Direct map offset:|Page tables:)' }) -join [Environment]::NewLine)
}
finally {
    try {
        if ($started -and -not $process.HasExited) {
            # Stop only the process started by this invocation.
            Stop-Process -InputObject $process -Force
            $process.WaitForExit()
        }
        # Preserve complete diagnostics after both ordinary exits and timeouts.
        if ($null -ne $stdoutCopy) { $null = $stdoutCopy.GetAwaiter().GetResult() }
        if ($null -ne $stderrCopy) { $null = $stderrCopy.GetAwaiter().GetResult() }
    }
    finally {
        if ($null -ne $stdoutStream) { $stdoutStream.Dispose() }
        if ($null -ne $stderrStream) { $stderrStream.Dispose() }
        $process.Dispose()
    }
}
