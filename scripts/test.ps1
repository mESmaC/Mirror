[CmdletBinding()]
param(
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
    & cargo clippy --locked --workspace -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Normal kernel Clippy check failed.' }
    & cargo clippy --locked --workspace --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Smoke kernel Clippy check failed.' }

    $logDirectory = Join-Path $repositoryRoot 'build/test-logs'
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    foreach ($release in @($false, $true)) {
        $profile = if ($release) { 'release' } else { 'debug' }
        $memoryMiB = if ($release) { 256 } else { 128 }
        & (Join-Path $PSScriptRoot 'build.ps1') -Smoke -Release:$release
        foreach ($fault in @('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout', 'BlockIoError')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Release:$release -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-$fault.serial.log") -Force
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-$fault.stderr.log") -Force
        }
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoDisplayDevice -Release:$release -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoDisplayDevice.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoDisplayDevice.stderr.log") -Force
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoTestDisk -Release:$release -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoTestDisk.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoTestDisk.stderr.log") -Force
        & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -NoKeyboard -Release:$release -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-NoKeyboard.serial.log") -Force
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-NoKeyboard.stderr.log") -Force

        & (Join-Path $PSScriptRoot 'build.ps1') -Release:$release
        & (Join-Path $PSScriptRoot 'capture.ps1') -NoBuild -Release:$release -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
        foreach ($artifact in @('serial.log', 'stderr.log', 'png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/normal-boot.$artifact") -Destination (Join-Path $logDirectory "$profile-console.$artifact") -Force
        }
        Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/normal-console.png') -Destination (Join-Path $logDirectory "$profile-console-input.png") -Force
        foreach ($artifact in @('normal-pointer.png', 'cursor-away.png', 'cursor-restored.png', 'desktop.png', 'desktop-input.png', 'desktop-cursor-away.png', 'desktop-cursor-restored.png')) {
            Copy-Item -LiteralPath (Join-Path $repositoryRoot "build/$artifact") -Destination (Join-Path $logDirectory "$profile-$artifact") -Force
        }
    }

    # Leave the boot tree ready for a normal run after the test cases finish.
    & (Join-Path $PSScriptRoot 'build.ps1')
    Write-Host "All 32 kernel/userspace/device smoke cases and debug/release desktop/console captures passed. Logs: $logDirectory"
}
finally {
    Pop-Location
}
