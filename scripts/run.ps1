[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Smoke,
    [switch]$NoBuild,
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 30,
    [ValidateRange(128, 32768)][int]$MemoryMiB = 256
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$qemu = Get-MirrorQemu -RequestedPath $QemuPath -RepositoryRoot $repositoryRoot
$qemuDirectory = Split-Path -Parent $qemu
$codePath = Get-MirrorFirmware -RequestedPath $FirmwareCode -QemuDirectory $qemuDirectory -Names @('edk2-x86_64-code.fd', 'OVMF_CODE.fd', 'OVMF_CODE.4m.fd') -ParameterName 'FirmwareCode'
$varsTemplate = Get-MirrorFirmware -RequestedPath $FirmwareVars -QemuDirectory $qemuDirectory -Names @('edk2-i386-vars.fd', 'OVMF_VARS.fd', 'OVMF_VARS.4m.fd') -ParameterName 'FirmwareVars'

if (-not $NoBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release -Smoke:$Smoke
}

$buildDirectory = Join-Path $repositoryRoot 'build'
$espDirectory = Join-Path $buildDirectory 'esp'
foreach ($relativePath in @('EFI/BOOT/BOOTX64.EFI', 'kernel.elf', 'limine.conf')) {
    if (-not (Test-Path -LiteralPath (Join-Path $espDirectory $relativePath) -PathType Leaf)) {
        throw "The boot tree is incomplete: $relativePath. Run scripts/build.ps1 first."
    }
}

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
    '-display', 'none',
    '-monitor', 'none',
    '-nic', 'none',
    '-no-reboot',
    '-drive', "if=pflash,unit=0,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $codePath)",
    '-drive', "if=pflash,unit=1,format=raw,file=$(ConvertTo-QemuOptionValue $varsPath)",
    '-drive', "if=none,id=mirror-esp,format=raw,readonly=on,file=fat:$(ConvertTo-QemuOptionValue $espDirectory)",
    '-device', 'virtio-blk-pci,drive=mirror-esp,bootindex=0'
)
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
    if ($serial -match '(?im)^(?:MIRROR_PANIC|PANIC|\[panic\]|EXCEPTION|MIRROR_EXCEPTION)') {
        throw "Mirror reported a panic or exception. See $serialLog."
    }
    if ($qemuExitCode -ne 33) {
        $qemuError = [System.IO.File]::ReadAllText($stderrLog)
        throw "Mirror smoke test expected QEMU exit 33; got $qemuExitCode. $qemuError See $serialLog."
    }
    foreach ($marker in @('MIRROR_FRAMES_OK', 'MIRROR_ALLOCATOR_TESTS_OK', 'MIRROR_BOOT_OK')) {
        if ($serial -notmatch "(?m)^$marker(?:\s|$)") {
            throw "Mirror smoke test did not report $marker. See $serialLog."
        }
    }
    Write-Host "Mirror boot smoke test passed (QEMU exit 33, frame and boot checks). Serial log: $serialLog"
    Write-Output $serial
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
