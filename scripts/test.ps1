[CmdletBinding()]
param(
    [switch]$AgentDemo,
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $repositoryRoot
try {
    & cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Rust formatting check failed.' }
    & cargo test --locked -p mirror-desktop-protocol -p mirror-window-protocol -p mirror-vm-protocol -p mirror-thread-protocol -p mirror-sync -p mirror-acpi --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Native protocol, synchronization, and ACPI parser tests failed.' }
    & cargo clippy --locked --workspace -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Normal kernel Clippy check failed.' }
    & cargo clippy --locked --workspace --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Smoke kernel Clippy check failed.' }

    $logDirectory = Join-Path $repositoryRoot $(if ($AgentDemo) { 'build/test-agent-logs' } else { 'build/test-logs' })
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    foreach ($release in @($false, $true)) {
        $profile = if ($release) { 'release' } else { 'debug' }
        $memoryMiB = if ($release) { 256 } else { 128 }
        & (Join-Path $PSScriptRoot 'build.ps1') -Smoke -Release:$release -AgentDemo:$AgentDemo
        # UserSimd remains a strict manual #XM probe: current QEMU TCG does not
        # deliver that unmasked SSE arithmetic exception (QEMU issue 215).
        foreach ($fault in @('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout', 'BlockIoError', 'VmReadOnly', 'VmNx', 'VmReleased', 'VmGuard', 'UserX87', 'UserAvx', 'UserFsgsbase', 'ThreadLastExit', 'ThreadProcessExit', 'ThreadFault')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-$fault.serial.log") -Force
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-$fault.stderr.log") -Force
        }
        # Expose AVX and FSGSBASE through CPUID as well as exercising the
        # conservative CPU model. The kernel must keep both features disabled.
        foreach ($fault in @('None', 'UserAvx', 'UserFsgsbase')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -ExtendedCpu -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-ExtendedCpu-$fault.serial.log") -Force
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-ExtendedCpu-$fault.stderr.log") -Force
        }
        # Parse all four enabled CPUs and route device IRQs to the BSP. This
        # validates topology discovery; secondary processors remain offline.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-CpuCount4.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-CpuCount4.stderr.log") -Force
        # Explicit boot policy must keep the legacy PIC/PIT backend functional,
        # including userspace preemption and PS/2 input. OVMF still needs APIC
        # hardware during firmware startup, so retain the CPU's APIC feature.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -LegacyPic -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-LegacyPic.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-LegacyPic.stderr.log") -Force
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoDisplayDevice -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoDisplayDevice.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoDisplayDevice.stderr.log") -Force
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoTestDisk -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoTestDisk.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoTestDisk.stderr.log") -Force
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoKeyboard -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoKeyboard.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoKeyboard.stderr.log") -Force

        & (Join-Path $PSScriptRoot 'build.ps1') -Release:$release -AgentDemo:$AgentDemo
        & (Join-Path $PSScriptRoot 'capture.ps1') -NoBuild -Release:$release -AgentDemo:$AgentDemo -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        foreach ($artifact in @('serial.log', 'stderr.log', 'png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/normal-boot.$artifact") -Destination (Join-Path $logDirectory "$profile-console.$artifact") -Force
        }
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/normal-console.png') -Destination (Join-Path $logDirectory "$profile-console-input.png") -Force
        foreach ($artifact in @('normal-pointer.png', 'cursor-away.png', 'cursor-restored.png', 'desktop.png', 'desktop-input.png', 'desktop-cursor-away.png', 'desktop-cursor-restored.png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
        }
        $applicationArtifacts = if ($AgentDemo) { @('desktop-agent.png') } else {
            @('desktop-windows.png', 'desktop-moved.png', 'desktop-minimized.png', 'desktop-reopened.png', 'desktop-counter.png')
        }
        foreach ($artifact in $applicationArtifacts) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
        }
    }

    # Leave the boot tree ready for a normal run after the test cases finish.
    & (Join-Path $PSScriptRoot 'build.ps1')
    Write-Host "All 62 kernel/userspace/device smoke cases and debug/release desktop/console captures passed. Logs: $logDirectory"
}
finally {
    Pop-Location
}
