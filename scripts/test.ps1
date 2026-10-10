[CmdletBinding()]
param(
    [switch]$AgentDemo,
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$smokeBoots = 0

function Save-MirrorSmokeLogs {
    param([Parameter(Mandatory = $true)][string]$Name)
    # Count each smoke boot as its logs are kept, so the summary reports the
    # boots that actually ran instead of a hard-coded total.
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$Name.serial.log") -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$Name.stderr.log") -Force
    $script:smokeBoots++
}

function Assert-MirrorKernelImage {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('Default', 'Trace')][string]$Flavor,
        [Parameter(Mandatory = $true)][string]$Label
    )
    # Spec 0010, acceptance test 4: a byte search of the staged kernel. Latin-1
    # maps every byte to one character, so an ordinal search is a byte search.
    $image = [System.Text.Encoding]::GetEncoding(28591).GetString([System.IO.File]::ReadAllBytes((Join-Path $repositoryRoot 'build/esp/kernel.elf')))
    if ($Flavor -eq 'Default') {
        # MIRROR_PERF_ strings (spec 0019) are allowed under 0010-D4's exception.
        $required = @()
        $forbidden = @('MIRROR_TRACE_ON', 'MIRROR_CONSOLE_COMMAND operation=', 'MIRROR_POINTER_UPDATED', 'MIRROR_POINTER_CLICK', 'MIRROR_SHELL_FRAME', 'MIRROR_DESKTOP_INPUT_TIMEOUT', 'Mirror shell: ready', 'MIRROR_USERSPACE_OK', 'Mirror init: VM checks passed', 'fixture data mismatch', 'unexpected budget exhaustion')
    }
    else {
        $required = @('MIRROR_TRACE_ON', 'MIRROR_SHELL_FRAME')
        $forbidden = @('MIRROR_USERSPACE_OK', 'fixture data mismatch')
    }
    foreach ($text in $required) {
        if ($image.IndexOf($text, [StringComparison]::Ordinal) -lt 0) {
            throw "The $Label $($Flavor.ToLowerInvariant()) kernel image lacks '$text'."
        }
    }
    foreach ($text in $forbidden) {
        if ($image.IndexOf($text, [StringComparison]::Ordinal) -ge 0) {
            throw "The $Label $($Flavor.ToLowerInvariant()) kernel image contains '$text'."
        }
    }
    Write-Host "The $Label $($Flavor.ToLowerInvariant()) kernel image passed its string scan."
}

Push-Location -LiteralPath $repositoryRoot
try {
    Assert-MirrorToolchain
    # Start from an empty log directory so every log in it comes from this run.
    # Refuse links, so the recursive removal never reaches outside the directory.
    $logDirectory = Join-Path $repositoryRoot $(if ($AgentDemo) { 'build/test-agent-logs' } else { 'build/test-logs' })
    if (Test-Path -LiteralPath $logDirectory) {
        @(Get-Item -LiteralPath $logDirectory -Force) + @(Get-ChildItem -LiteralPath $logDirectory -Recurse -Force) | ForEach-Object {
            if ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing to clear $logDirectory because $($_.FullName) is a link."
            }
        }
        Remove-Item -LiteralPath $logDirectory -Recurse -Force
    }
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    & cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Rust formatting check failed.' }
    & cargo test --locked -p mirror-desktop-protocol -p mirror-window-protocol -p mirror-vm-protocol -p mirror-thread-protocol -p mirror-clock-protocol -p mirror-sync -p mirror-acpi --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Native protocol, synchronization, and ACPI parser tests failed.' }
    & cargo clippy --locked --workspace -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Normal kernel Clippy check failed.' }
    & cargo clippy --locked -p mirror-kernel --features test-trace -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Trace kernel Clippy check failed.' }
    & cargo clippy --locked --workspace --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Smoke kernel Clippy check failed.' }

    foreach ($release in @($false, $true)) {
        $profile = if ($release) { 'release' } else { 'debug' }
        # Keep firmware and full debug ELF loading out of the kernel memory
        # regression budget. At 128 MiB Limine can exhaust its high-memory
        # allocator while reading the ELF, before any kernel code executes.
        $memoryMiB = 256
        $smpMemoryMiB = 256
        & (Join-Path $PSScriptRoot 'build.ps1') -Smoke -Release:$release -AgentDemo:$AgentDemo
        # Every case uses the V1 device profile, with the test fixture on AHCI
        # SATA, unless it names -DeviceProfile LegacyVirtio (spec 0014).
        # UserSimd remains a strict manual #XM probe: current QEMU TCG does not
        # deliver that unmasked SSE arithmetic exception (QEMU issue 215).
        foreach ($fault in @('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout', 'VmReadOnly', 'VmNx', 'VmReleased', 'VmGuard', 'UserX87', 'UserAvx', 'UserFsgsbase', 'ThreadLastExit', 'ThreadProcessExit', 'ThreadFault')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-$fault"
        }
        # The legacy VirtIO fixture holds the suite's only write, flush,
        # write-error and dirty-shutdown checks until AHCI gains a write path
        # (0014-D5, spec 0062).
        foreach ($fault in @('None', 'BlockIoError', 'BlockWrite', 'BlockWriteError', 'BlockFlushError', 'BlockNoFlush')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -DeviceProfile LegacyVirtio -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-LegacyVirtio-$fault"
        }
        # Exercise the class-standard SATA path's recovery: a completed ATA
        # error, and failed initialization after engine activation, followed by
        # safe DMA reuse and a reopen.
        foreach ($fault in @('AhciIoError', 'AhciInitFailure')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-$fault"
        }
        # Verify controller ring wrapping and bounded USB2 enumeration through
        # endpoint zero. Include addressed-slot failure cleanup and refusal of
        # multiple devices or missing reset timing; HID report input is separate.
        foreach ($case in @(
                @{ Name = 'None'; Fault = 'None'; NoHpet = $false; Device = 'None'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'InitFailure'; Fault = 'XhciInitFailure'; NoHpet = $false; Device = 'None'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'NoHpet'; Fault = 'None'; NoHpet = $true; Device = 'None'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'CpuCount4'; Fault = 'None'; NoHpet = $false; Device = 'None'; FullSpeed = $false; Cpus = 4 },
                @{ Name = 'Keyboard'; Fault = 'None'; NoHpet = $false; Device = 'Keyboard'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'Mouse'; Fault = 'None'; NoHpet = $false; Device = 'Mouse'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'Keyboard-FullSpeed'; Fault = 'None'; NoHpet = $false; Device = 'Keyboard'; FullSpeed = $true; Cpus = 1 },
                @{ Name = 'Keyboard-CpuCount4'; Fault = 'None'; NoHpet = $false; Device = 'Keyboard'; FullSpeed = $false; Cpus = 4 },
                @{ Name = 'Keyboard-NoHpet'; Fault = 'None'; NoHpet = $true; Device = 'Keyboard'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'Keyboard-DeviceFailure'; Fault = 'XhciDeviceFailure'; NoHpet = $false; Device = 'Keyboard'; FullSpeed = $false; Cpus = 1 },
                @{ Name = 'Pair'; Fault = 'None'; NoHpet = $false; Device = 'Pair'; FullSpeed = $false; Cpus = 1 })) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Xhci -XhciKeyboard:($case.Device -eq 'Keyboard') -XhciMouse:($case.Device -eq 'Mouse') -XhciPair:($case.Device -eq 'Pair') -XhciFullSpeed:$case.FullSpeed -NoHpet:$case.NoHpet -CpuCount $case.Cpus -Release:$release -AgentDemo:$AgentDemo -Fault $case.Fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-Xhci-$($case.Name)"
        }
        # Expose AVX and FSGSBASE through CPUID as well as exercising the
        # conservative CPU model. The kernel must keep both features disabled.
        foreach ($fault in @('None', 'UserAvx', 'UserFsgsbase')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -ExtendedCpu -Release:$release -AgentDemo:$AgentDemo -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-ExtendedCpu-$fault"
        }
        # Bring all configured CPUs online with private stacks/descriptors and
        # verify IPI rendezvous before and after the BSP's userspace workloads.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-CpuCount4"
        foreach ($cpuCount in @(2, 8)) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -ExtendedCpu -CpuCount $cpuCount -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-ExtendedCpu-CpuCount$cpuCount"
        }
        # A removed shared alias must fault on an AP, and a missing shootdown
        # ACK must stop unmap before returning a frame for allocator reuse.
        foreach ($fault in @('SmpUnmap', 'SmpTlbTimeout')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -CpuCount 4 -Fault $fault -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Save-MirrorSmokeLogs "$profile-$fault"
        }
        # Both PIT selection paths retain the APIC/SMP backend and must still
        # preempt userspace after explicit policy or a rejected calibration.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -PitTimer -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-PitTimer-CpuCount4"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Fault TimerCalibration -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-TimerCalibration-CpuCount4"
        # An optional elapsed-time clock must not change IRQ timer behavior.
        # Cover absent hardware with both controllers and active-device cleanup.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoHpet -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-NoHpet-CpuCount4"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoHpet -LegacyPic -CpuCount 1 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-NoHpet-LegacyPic"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Fault ClockActivation -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-ClockActivation-CpuCount4"
        # Explicit boot policy must keep the legacy PIC/PIT backend functional,
        # including userspace preemption and PS/2 input. OVMF still needs APIC
        # hardware during firmware startup, so retain the CPU's APIC feature.
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -LegacyPic -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-LegacyPic"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -LegacyPic -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $smpMemoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-LegacyPic-CpuCount4"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoDisplayDevice -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-NoDisplayDevice"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoTestDisk -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-NoTestDisk"
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoKeyboard -Release:$release -AgentDemo:$AgentDemo -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Save-MirrorSmokeLogs "$profile-NoKeyboard"

        # Captures drive a trace kernel: its console-text oracles are their
        # synchronization points (spec 0010).
        & (Join-Path $PSScriptRoot 'build.ps1') -Trace -Release:$release -AgentDemo:$AgentDemo
        & (Join-Path $PSScriptRoot 'capture.ps1') -NoBuild -CpuCount 4 -Release:$release -AgentDemo:$AgentDemo -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        foreach ($artifact in @('serial.log', 'stderr.log', 'png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/normal-boot.$artifact") -Destination (Join-Path $logDirectory "$profile-console.$artifact") -Force
        }
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/normal-console.png') -Destination (Join-Path $logDirectory "$profile-console-input.png") -Force
        foreach ($artifact in @('normal-pointer.png', 'normal-devices.png', 'normal-devices-page2.png', 'cursor-away.png', 'cursor-restored.png', 'desktop.png', 'desktop-input.png', 'desktop-cursor-away.png', 'desktop-cursor-restored.png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
        }
        $applicationArtifacts = if ($AgentDemo) { @('desktop-agent.png') } else {
            @('desktop-windows.png', 'desktop-moved.png', 'desktop-minimized.png', 'desktop-reopened.png', 'desktop-counter.png')
        }
        foreach ($artifact in $applicationArtifacts) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
        }
        $imageLabel = if ($AgentDemo) { "$profile agent-demo" } else { $profile }
        Assert-MirrorKernelImage -Flavor Trace -Label $imageLabel

        # The default flavor must hold no trace or smoke strings, and without
        # agent-demo it must pass the QEMU hygiene probe (spec 0010).
        & (Join-Path $PSScriptRoot 'build.ps1') -Release:$release -AgentDemo:$AgentDemo
        Assert-MirrorKernelImage -Flavor Default -Label $imageLabel
        if (-not $AgentDemo) {
            & (Join-Path $PSScriptRoot 'capture.ps1') -NoBuild -Hygiene -CpuCount 4 -Release:$release -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            foreach ($artifact in @('hygiene-boot.serial.log', 'hygiene-boot.stderr.log', 'hygiene-boot.png', 'hygiene-notes-before.png', 'hygiene-notes-typed.png', 'hygiene-notes-pointer.png', 'hygiene-console-before.png', 'hygiene-console-typed.png', 'hygiene-console-pointer.png')) {
                Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
            }
        }
    }

    # Leave the boot tree ready for a normal run after the test cases finish.
    & (Join-Path $PSScriptRoot 'build.ps1')
    $captureSummary = if ($AgentDemo) { 'trace captures and kernel image scans' } else { 'trace captures, kernel image scans and hygiene probes' }
    Write-Host "All $smokeBoots kernel/userspace/device smoke cases and debug/release $captureSummary passed. Logs: $logDirectory"
}
finally {
    Pop-Location
}
