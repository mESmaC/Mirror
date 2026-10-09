[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Smoke,
    [switch]$NoBuild,
    [switch]$Graphical,
    [switch]$AgentDemo,
    [switch]$NoDisplayDevice,
    [switch]$NoTestDisk,
    [switch]$NoKeyboard,
    [switch]$ExtendedCpu,
    [switch]$LegacyPic,
    [ValidateRange(1, 64)][int]$CpuCount = 1,
    [ValidateSet('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout', 'BlockIoError', 'VmReadOnly', 'VmNx', 'VmReleased', 'VmGuard', 'UserX87', 'UserSimd', 'UserAvx', 'UserFsgsbase', 'ThreadLastExit', 'ThreadProcessExit', 'ThreadFault')][string]$Fault = 'None',
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 30,
    [ValidateRange(128, 32768)][int]$MemoryMiB = 256
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
if ($Fault -ne 'None' -and -not $Smoke) {
    throw '-Fault requires -Smoke; intentional faults are excluded from normal kernels.'
}
if ($Graphical -and ($Smoke -or $NoDisplayDevice)) {
    throw '-Graphical requires a normal boot with a display device.'
}
if ($NoTestDisk -and $Fault -eq 'BlockIoError') {
    throw '-Fault BlockIoError requires the test disk.'
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
    default { $null }
}
$kernelArguments = @()
if ($null -ne $testMode) { $kernelArguments += "mirror.test=$testMode" }
if ($LegacyPic) { $kernelArguments += 'mirror.irq=pic' }
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
$qemuArguments = @(
    '-machine', $(if ($NoKeyboard) { 'q35,accel=tcg,i8042=off,vmport=off' } else { 'q35,accel=tcg' }),
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
if (-not $NoTestDisk) {
    $testDisk = Get-MirrorTestDisk -RepositoryRoot $repositoryRoot
    $testDiskHash = (Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash
    if ($Fault -eq 'BlockIoError') {
        # Inject into a data sector used by the kernel probe, away from firmware
        # partition discovery. JSON preserves paths without blkdebug URL colons.
        $testNode = @{
            driver = 'raw'; 'node-name' = 'mirror-test'; 'read-only' = $true
            file = @{
                driver = 'blkdebug'
                image = @{ driver = 'file'; filename = $testDisk }
                'inject-error' = @(@{ event = 'read_aio'; errno = 5; sector = 1234; once = $true })
            }
        } | ConvertTo-Json -Compress -Depth 6
        $qemuArguments += @('-blockdev', $testNode)
    }
    else {
        $qemuArguments += @('-drive', "if=none,id=mirror-test,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $testDisk)")
    }
    $qemuArguments += @('-device', 'virtio-blk-pci,drive=mirror-test,disable-modern=on,disable-legacy=off')
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
    if ($serial -match '(?im)^(?:MIRROR_PANIC|PANIC|\[panic\])') {
        throw "Mirror reported a panic. See $serialLog."
    }
    $fatalKernelFault = $Fault -in @('PageFault', 'InvalidOpcode')
    $expectedExitCode = if ($fatalKernelFault) { 35 } else { 33 }
    if ($qemuExitCode -ne $expectedExitCode) {
        $qemuError = [System.IO.File]::ReadAllText($stderrLog)
        throw "Mirror smoke test expected QEMU exit $expectedExitCode; got $qemuExitCode. $qemuError See $serialLog."
    }
    $markers = @('MIRROR_FRAMES_OK', 'MIRROR_ALLOCATOR_TESTS_OK', 'MIRROR_FRAME_REUSE_OK', 'MIRROR_EXCEPTIONS_OK', 'MIRROR_PAGING_OK', 'MIRROR_HEAP_OK', 'MIRROR_TIMER_OK', 'MIRROR_HEAP_IRQ_OK', 'MIRROR_USER_PAGING_OK', 'MIRROR_ELF_TESTS_OK', 'MIRROR_USERSPACE_OK', 'MIRROR_USER_CLEANUP_OK', 'MIRROR_USER_LOAD_ROLLBACK_OK', 'MIRROR_SCHEDULER_OK', 'MIRROR_CAPABILITY_TESTS_OK', 'MIRROR_IPC_TESTS_OK', 'MIRROR_IPC_OK')
    $markers += @('MIRROR_PCI_TESTS_OK', 'MIRROR_PCI_READY', 'MIRROR_FRAMEBUFFER_TESTS_OK')
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
    # Firmware must describe every configured CPU, although only the BSP
    # executes kernel code until secondary-processor startup is implemented.
    if ($serial -notmatch "(?m)^MIRROR_ACPI_READY [^\r\n]* cpus=$CpuCount enabled=$CpuCount(?:\s|$)") {
        throw "ACPI discovery must report exactly $CpuCount enabled processors."
    }
    if ($LegacyPic) {
        $markers += 'MIRROR_APIC_SKIPPED reason=boot_policy'
        if ($serial -notmatch '(?m)^MIRROR_TIMER_READY source=pit irq=0 vector=32 [^\r\n]* controller=pic(?:\s|$)') {
            throw 'The legacy PIC boot policy must deliver PIT interrupts through the PIC.'
        }
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
        if ($serial -notmatch '(?m)^MIRROR_TIMER_READY source=pit irq=0 vector=32 [^\r\n]* controller=ioapic(?:\s|$)') {
            throw 'The APIC regression must deliver PIT interrupts through the I/O APIC.'
        }
    }
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
    if ($NoTestDisk) {
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
    if (-not $fatalKernelFault) {
        $markers += @('MIRROR_PAGE_FAULT_OK', 'MIRROR_BOOT_OK')
        if ($serial -match '(?im)^(?:EXCEPTION|MIRROR_EXCEPTION)(?:\s|:|$)') {
            throw "Mirror reported an unexpected exception. See $serialLog."
        }
    }
    else {
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
    Write-Host "Mirror boot smoke test passed (fault=$Fault, no-display=$NoDisplayDevice, extended-cpu=$ExtendedCpu, cpus=$CpuCount, legacy-pic=$LegacyPic, QEMU exit $expectedExitCode). Serial log: $serialLog"
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
