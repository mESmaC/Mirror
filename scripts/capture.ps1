[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$Release,
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 30
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
if (-not $NoBuild) { & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release }
$qemu = Get-MirrorQemu -RequestedPath $QemuPath -RepositoryRoot $repositoryRoot
$qemuDirectory = Split-Path -Parent $qemu
$codePath = Get-MirrorFirmware -RequestedPath $FirmwareCode -QemuDirectory $qemuDirectory -Names @('edk2-x86_64-code.fd', 'OVMF_CODE.fd', 'OVMF_CODE.4m.fd') -ParameterName 'FirmwareCode'
$varsTemplate = Get-MirrorFirmware -RequestedPath $FirmwareVars -QemuDirectory $qemuDirectory -Names @('edk2-i386-vars.fd', 'OVMF_VARS.fd', 'OVMF_VARS.4m.fd') -ParameterName 'FirmwareVars'
$buildDirectory = Join-Path $repositoryRoot 'build'
$espDirectory = Join-Path $buildDirectory 'esp'
foreach ($relativePath in @('EFI/BOOT/BOOTX64.EFI', 'kernel.elf', 'init.elf', 'limine.conf')) {
    if (-not (Test-Path -LiteralPath (Join-Path $espDirectory $relativePath) -PathType Leaf)) {
        throw "The boot tree is incomplete: $relativePath. Run scripts/build.ps1 first."
    }
}
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'boot/limine.conf') -Destination (Join-Path $espDirectory 'limine.conf') -Force
$varsPath = Join-Path $buildDirectory 'capture.vars.fd'
Copy-Item -LiteralPath $varsTemplate -Destination $varsPath -Force
$serialLog = Join-Path $buildDirectory 'normal-boot.serial.log'
$screenshot = Join-Path $buildDirectory 'normal-boot.png'
$consoleScreenshot = Join-Path $buildDirectory 'normal-console.png'
[System.IO.File]::WriteAllText($serialLog, '')

function Read-MirrorSerial {
    # The guest writes continuously. Share both read and write access so Windows
    # allows inspecting this invocation's serial log without closing QEMU.
    $stream = [System.IO.File]::Open($serialLog, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    $reader = [System.IO.StreamReader]::new($stream)
    try { return $reader.ReadToEnd() }
    finally { $reader.Dispose() }
}

function Read-MirrorQmp {
    param([System.Diagnostics.Process]$Process, [int]$TimeoutMs = 5000)
    $line = $Process.StandardOutput.ReadLineAsync()
    if (-not $line.Wait($TimeoutMs)) { throw 'QMP response timed out.' }
    $value = $line.GetAwaiter().GetResult()
    if ($null -eq $value) { throw 'QMP closed unexpectedly.' }
    return ($value | ConvertFrom-Json)
}

function Invoke-MirrorQmp {
    param([System.Diagnostics.Process]$Process, [string]$Command, [hashtable]$Arguments = @{})
    $id = [guid]::NewGuid().ToString('N')
    $request = @{ execute = $Command; arguments = $Arguments; id = $id } | ConvertTo-Json -Compress -Depth 8
    $Process.StandardInput.WriteLine($request)
    $Process.StandardInput.Flush()
    # Events may arrive between command responses. Bound their handling too.
    $responseTime = [System.Diagnostics.Stopwatch]::StartNew()
    for ($messageCount = 0; $messageCount -lt 32; $messageCount++) {
        $remaining = 5000 - [int]$responseTime.ElapsedMilliseconds
        if ($remaining -le 0) { throw 'QMP response timed out.' }
        $response = Read-MirrorQmp $Process $remaining
        if ($response.PSObject.Properties['id'] -and $response.id -eq $id) {
            if ($response.PSObject.Properties['error']) { throw "QMP $Command failed: $($response.error | ConvertTo-Json -Compress)" }
            if (-not $response.PSObject.Properties['return']) { throw 'Malformed QMP response.' }
            return
        }
    }
    throw 'Too many unrelated QMP messages.'
}

function Send-MirrorKeyEdges {
    param([System.Diagnostics.Process]$Process, [object[]]$Edges)
    $events = @($Edges | ForEach-Object {
        @{ type = 'key'; data = @{ down = [bool]$_.Down; key = @{ type = 'qcode'; data = $_.Key } } }
    })
    # QMP injects actual guest keyboard events through QEMU's PS/2 device.
    # Explicit edges also cover modifier overlap and release ordering.
    Invoke-MirrorQmp $Process 'input-send-event' @{ events = $events }
    Start-Sleep -Milliseconds 40
}

function Send-MirrorKey {
    param([System.Diagnostics.Process]$Process, [string]$Key)
    Send-MirrorKeyEdges $Process @(@{ Key = $Key; Down = $true }, @{ Key = $Key; Down = $false })
}

function Send-MirrorText {
    param([System.Diagnostics.Process]$Process, [string]$Text)
    foreach ($character in $Text.ToCharArray()) {
        $key = if ($character -eq ' ') { 'spc' } else { [string]$character }
        if ($key -ne 'spc' -and $key -cnotmatch '^[a-z]$') { throw 'Input probe accepts lowercase letters and spaces only.' }
        Send-MirrorKey $Process $key
    }
}

function Wait-MirrorConsoleCommand {
    param([System.Diagnostics.Process]$Process, [string]$Command)
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    $escaped = [regex]::Escape($Command)
    do {
        if ($Process.HasExited) { throw 'Kernel exited during keyboard input.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|MIRROR_CONSOLE_INPUT_LOST)(?:\s|:|$)') {
            throw "Kernel failed or lost keyboard input. See $serialLog."
        }
        # Require a completed redraw after this exact command, with real IRQ1
        # delivery. A QMP acknowledgement alone cannot satisfy the input test.
        if ($serial -match "(?m)^MIRROR_CONSOLE_COMMAND $escaped\r?`$(?s:.*?)^MIRROR_CONSOLE_UPDATED revision=[0-9]+ display=1 irq=[1-9][0-9]*\r?`$") { return }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "Console command did not complete: $Command. See $serialLog." }
        Start-Sleep -Milliseconds 50
    } while ($true)
}

function Save-MirrorScreenshot {
    param([System.Diagnostics.Process]$Process, [string]$Path)
    Invoke-MirrorQmp $Process 'screendump' @{ filename = $Path; format = 'png' }
    $image = [System.IO.File]::ReadAllBytes($Path)
    if ($image.Length -lt 24 -or [Convert]::ToBase64String($image[0..7]) -ne 'iVBORw0KGgo=') {
        throw 'QEMU did not produce a PNG screenshot.'
    }
}

$qemuArguments = @(
    '-machine', 'q35,accel=tcg', '-cpu', 'qemu64', '-m', '256', '-smp', '1',
    '-display', 'none', '-vga', 'std', '-monitor', 'none', '-qmp', 'stdio',
    '-nic', 'none', '-no-reboot', '-no-shutdown',
    '-drive', "if=pflash,unit=0,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $codePath)",
    '-drive', "if=pflash,unit=1,format=raw,file=$(ConvertTo-QemuOptionValue $varsPath)",
    '-drive', "if=none,id=mirror-esp,format=raw,readonly=on,file=fat:$(ConvertTo-QemuOptionValue $espDirectory)",
    '-device', 'virtio-blk-pci,drive=mirror-esp,disable-legacy=on,bootindex=0', '-serial', "file:$serialLog"
)
$testDisk = Get-MirrorTestDisk -RepositoryRoot $repositoryRoot
$testDiskHash = (Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash
$qemuArguments += @(
    '-drive', "if=none,id=mirror-test,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $testDisk)",
    '-device', 'virtio-blk-pci,drive=mirror-test,disable-modern=on,disable-legacy=off'
)
if (Test-Path -LiteralPath (Join-Path $qemuDirectory 'share') -PathType Container) {
    $qemuArguments += @('-L', (Join-Path $qemuDirectory 'share'))
}
$start = [System.Diagnostics.ProcessStartInfo]::new()
$start.FileName = $qemu
$start.WorkingDirectory = $repositoryRoot
$start.Arguments = (($qemuArguments | ForEach-Object { ConvertTo-WindowsProcessArgument $_ }) -join ' ')
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardInput = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $start
$stderr = $null
$started = $false
try {
    if (-not $process.Start()) { throw 'Could not start QEMU.' }
    $started = $true
    $stderr = $process.StandardError.ReadToEndAsync()
    $greeting = Read-MirrorQmp $process
    if (-not $greeting.PSObject.Properties['QMP']) { throw 'Missing QMP greeting.' }
    Invoke-MirrorQmp $process 'qmp_capabilities'
    $elapsed = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($process.HasExited) { throw 'Normal kernel exited before reaching idle.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|PANIC|EXCEPTION)(?:\s|:)') {
            throw "Normal kernel reported a fatal diagnostic. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_CONSOLE_READY input=ps2 display=1(?:\s|$)') { break }
        if ($elapsed.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "Normal boot timed out. See $serialLog." }
        Start-Sleep -Milliseconds 100
    } while ($true)
    foreach ($marker in @('MIRROR_BOOT_OK', 'MIRROR_USERSPACE_OK scenario=0', 'MIRROR_USER_CLEANUP_OK', 'MIRROR_SCHEDULER_OK processes=2', 'MIRROR_IPC_OK messages=3', 'MIRROR_PCI_READY', 'MIRROR_FRAMEBUFFER_READY', 'MIRROR_FRAMEBUFFER_PRESENTED', 'MIRROR_BLOCK_READY', 'MIRROR_BLOCK_READ_OK', 'MIRROR_BLOCK_CLEANUP_OK', 'MIRROR_KEYBOARD_READY')) {
        if ($serial -notmatch "(?m)^$marker(?:\s|$)") { throw "Normal boot missing $marker. See $serialLog." }
    }
    if ($serial -match '(?m)^MIRROR_FRAMEBUFFER_SKIPPED(?:\s|$)') { throw 'Normal boot skipped framebuffer output.' }
    Save-MirrorScreenshot $process $screenshot
    Send-MirrorText $process 'echo '
    Send-MirrorKeyEdges $process @(@{ Key = 'shift'; Down = $true }, @{ Key = 'shift_r'; Down = $true }, @{ Key = 'shift'; Down = $false })
    Send-MirrorKey $process 'm'
    Send-MirrorKeyEdges $process @(@{ Key = 'shift_r'; Down = $false })
    Send-MirrorText $process 'irrpr'
    foreach ($key in @('left', 'left', 'delete', 'o', 'end', 'backspace', 'r', 'ret')) { Send-MirrorKey $process $key }
    Wait-MirrorConsoleCommand $process 'operation=echo text=Mirror'

    Send-MirrorText $process 'echo '
    Send-MirrorKeyEdges $process @(@{ Key = 'caps_lock'; Down = $true }, @{ Key = 'caps_lock'; Down = $true }, @{ Key = 'caps_lock'; Down = $false })
    Send-MirrorKey $process 'c'
    Send-MirrorKey $process 'caps_lock'
    Send-MirrorText $process 'aps'
    Send-MirrorKeyEdges $process @(@{ Key = 'ctrl'; Down = $true }, @{ Key = 'x'; Down = $true }, @{ Key = 'x'; Down = $false }, @{ Key = 'ctrl'; Down = $false })
    Send-MirrorKeyEdges $process @(@{ Key = 'alt'; Down = $true }, @{ Key = 'z'; Down = $true }, @{ Key = 'z'; Down = $false }, @{ Key = 'alt'; Down = $false })
    Send-MirrorKey $process 'ret'
    Wait-MirrorConsoleCommand $process 'operation=echo text=Caps'

    Send-MirrorText $process 'discard'
    Send-MirrorKey $process 'esc'
    Send-MirrorText $process 'sttus'
    foreach ($key in @('home', 'right', 'right', 'a', 'end', 'ret')) { Send-MirrorKey $process $key }
    Wait-MirrorConsoleCommand $process 'operation=status text='
    Send-MirrorText $process 'help'
    Send-MirrorKey $process 'ret'
    Wait-MirrorConsoleCommand $process 'operation=help text='
    Save-MirrorScreenshot $process $consoleScreenshot
    if ((Get-FileHash -LiteralPath $screenshot).Hash -eq (Get-FileHash -LiteralPath $consoleScreenshot).Hash) {
        throw 'Framebuffer did not change after keyboard commands.'
    }
    # QMP permits quit to terminate before returning a response. Require clean
    # process exit below rather than interpreting that allowed EOF as failure.
    $process.StandardInput.WriteLine('{"execute":"quit"}')
    $process.StandardInput.Flush()
    if (-not $process.WaitForExit(5000)) { throw 'QEMU did not exit after the capture.' }
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) { throw "QEMU capture exited with code $($process.ExitCode)." }
    if ((Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash -ne $testDiskHash) {
        throw 'The read-only block fixture was modified during the capture.'
    }
    Write-Host "Normal boot, PS/2 input, console editing, and framebuffer captures passed. Image: $consoleScreenshot"
}
finally {
    if ($started) {
        if (-not $process.HasExited) {
            $process.Kill()
            $process.WaitForExit()
        }
        if ($null -ne $stderr) {
            [System.IO.File]::WriteAllText((Join-Path $buildDirectory 'normal-boot.stderr.log'), $stderr.GetAwaiter().GetResult())
        }
    }
    $process.Dispose()
}
