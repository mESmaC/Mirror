[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Smoke,
    [switch]$NoBuild,
    [switch]$Graphical,
    [switch]$NoDisplayDevice,
    [ValidateSet('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout')][string]$Fault = 'None',
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
$qemu = Get-MirrorQemu -RequestedPath $QemuPath -RepositoryRoot $repositoryRoot
$qemuDirectory = Split-Path -Parent $qemu
$codePath = Get-MirrorFirmware -RequestedPath $FirmwareCode -QemuDirectory $qemuDirectory -Names @('edk2-x86_64-code.fd', 'OVMF_CODE.fd', 'OVMF_CODE.4m.fd') -ParameterName 'FirmwareCode'
$varsTemplate = Get-MirrorFirmware -RequestedPath $FirmwareVars -QemuDirectory $qemuDirectory -Names @('edk2-i386-vars.fd', 'OVMF_VARS.fd', 'OVMF_VARS.4m.fd') -ParameterName 'FirmwareVars'

if (-not $NoBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release -Smoke:$Smoke
}

$buildDirectory = Join-Path $repositoryRoot 'build'
$espDirectory = Join-Path $buildDirectory 'esp'
foreach ($relativePath in @('EFI/BOOT/BOOTX64.EFI', 'kernel.elf', 'init.elf', 'limine.conf')) {
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
    default { $null }
}
if ($null -ne $testMode) { $bootConfig += "`n    cmdline: mirror.test=$testMode`n" }
[System.IO.File]::WriteAllText((Join-Path $espDirectory 'limine.conf'), $bootConfig)

# Never write to the QEMU installation's firmware template. A fresh local copy
# also prevents stale firmware boot entries from changing the smoke-test path.
$varsPath = Join-Path $buildDirectory 'OVMF_VARS.fd'
Copy-Item -LiteralPath $varsTemplate -Destination $varsPath -Force
$serialLog = Join-Path $buildDirectory 'serial.log'
$stdoutLog = Join-Path $buildDirectory 'qemu.stdout.log'
$stderrLog = Join-Path $buildDirectory 'qemu.stderr.log'

$qemuArguments = @(
    '-machine', 'q35,accel=tcg',
    '-cpu', 'qemu64',
    '-m', "$MemoryMiB",
    '-smp', '1',
    '-vga', $(if ($NoDisplayDevice) { 'none' } else { 'std' }),
    '-monitor', 'none',
    '-nic', 'none',
    '-no-reboot',
    '-drive', "if=pflash,unit=0,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $codePath)",
    '-drive', "if=pflash,unit=1,format=raw,file=$(ConvertTo-QemuOptionValue $varsPath)",
    '-drive', "if=none,id=mirror-esp,format=raw,readonly=on,file=fat:$(ConvertTo-QemuOptionValue $espDirectory)",
    '-device', 'virtio-blk-pci,drive=mirror-esp,bootindex=0'
)
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
$process = $null
try {
    $process = Start-Process -FilePath $qemu -ArgumentList $processArguments -WorkingDirectory $repositoryRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "Mirror boot smoke test exceeded $TimeoutSeconds seconds. See $serialLog and $stderrLog."
    }
    $process.WaitForExit()
    $process.Refresh()
    $qemuExitCode = $process.ExitCode
    $serial = [System.IO.File]::ReadAllText($serialLog)
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
    if ($serial -notmatch '(?m)^MIRROR_PCI_READY .*virtio_block=[1-9][0-9]*(?:\s|$)') {
        throw "Mirror did not discover its VirtIO boot disk. See $serialLog."
    }
    if ($NoDisplayDevice) {
        if ($serial -notmatch '(?m)^MIRROR_FRAMEBUFFER_SKIPPED reason=missing(?:\s|$)' -or
            $serial -notmatch '(?m)^MIRROR_PCI_READY .*display=0(?:\s|$)' -or
            $serial -match '(?m)^MIRROR_FRAMEBUFFER_(?:READY|PRESENTED)(?:\s|$)') {
            throw "Mirror did not complete the no-display fallback. See $serialLog."
        }
    }
    else {
        $markers += @('MIRROR_FRAMEBUFFER_READY', 'MIRROR_FRAMEBUFFER_PRESENTED')
        if ($serial -match '(?m)^MIRROR_FRAMEBUFFER_SKIPPED(?:\s|$)' -or
            $serial -notmatch '(?m)^MIRROR_PCI_READY .*display=[1-9][0-9]*(?:\s|$)') {
            throw "Mirror did not use its display device. See $serialLog."
        }
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
    elseif ($scenario -ne 0) {
        $userVector = if ($Fault -eq 'UserPrivileged') { 13 } else { 14 }
        if ($serial -notmatch "(?m)^MIRROR_USER_FAULT_CONTAINED vector=$userVector(?:\s|$)") {
            throw "Mirror did not contain the intended $Fault exception. See $serialLog."
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
    Write-Host "Mirror boot smoke test passed (fault=$Fault, no-display=$NoDisplayDevice, QEMU exit $expectedExitCode). Serial log: $serialLog"
    # Keep firmware's terminal control sequences in the raw log, not tool output.
    Write-Output (($serial -split '\r?\n' | Where-Object { $_ -match '^(MIRROR_|Memory map:|Direct map offset:|Page tables:)' }) -join [Environment]::NewLine)
}
finally {
    if ($null -ne $process) {
        if (-not $process.HasExited) {
            # Stop only the process started by this invocation.
            Stop-Process -InputObject $process -Force
            $process.WaitForExit()
        }
        $process.Dispose()
    }
}
