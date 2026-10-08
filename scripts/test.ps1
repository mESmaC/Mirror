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
        foreach ($fault in @('None', 'PageFault', 'InvalidOpcode', 'UserKernelRead', 'UserCodeWrite', 'UserNx', 'UserGuard', 'UserPrivileged', 'UserBadStack', 'UserBudget', 'IpcPeerExit', 'IpcTimeout')) {
            & (Join-Path $PSScriptRoot 'run.ps1') -Smoke -NoBuild -Release:$release -Fault $fault -MemoryMiB $memoryMiB -QemuPath $QemuPath -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/serial.log') -Destination (Join-Path $logDirectory "$profile-$fault.serial.log") -Force
            Copy-Item -LiteralPath (Join-Path $repositoryRoot 'build/qemu.stderr.log') -Destination (Join-Path $logDirectory "$profile-$fault.stderr.log") -Force
        }
    }

    # Leave the boot tree ready for a normal run after the test cases finish.
    & (Join-Path $PSScriptRoot 'build.ps1')
    Write-Host "All 24 kernel/userspace boot cases passed. Logs: $logDirectory"
}
finally {
    Pop-Location
}
