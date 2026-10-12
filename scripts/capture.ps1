[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$Release,
    [switch]$Hygiene,
    [switch]$RelativePointer,
    [ValidateRange(1, 64)][int]$CpuCount = 1,
    [string]$QemuPath = $env:QEMU_PATH,
    [string]$FirmwareCode = $env:OVMF_CODE,
    [string]$FirmwareVars = $env:OVMF_VARS,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 30
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
# Captures synchronize on the test-trace console oracles, so they boot a trace
# kernel. The hygiene probe boots the default kernel, which prints neither those
# oracles nor user content (spec 0010).
if (-not $NoBuild) { & (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release -Trace:(-not $Hygiene) }
$qemu = Get-MirrorQemu -RequestedPath $QemuPath -RepositoryRoot $repositoryRoot
$qemuDirectory = Split-Path -Parent $qemu
$codePath = Get-MirrorFirmware -RequestedPath $FirmwareCode -QemuDirectory $qemuDirectory -Names @('edk2-x86_64-code.fd', 'OVMF_CODE.fd', 'OVMF_CODE.4m.fd') -ParameterName 'FirmwareCode'
$varsTemplate = Get-MirrorFirmware -RequestedPath $FirmwareVars -QemuDirectory $qemuDirectory -Names @('edk2-i386-vars.fd', 'OVMF_VARS.fd', 'OVMF_VARS.4m.fd') -ParameterName 'FirmwareVars'
$buildDirectory = Join-Path $repositoryRoot 'build'
$espDirectory = Join-Path $buildDirectory 'esp'
# Captures boot the image read-only with its own limine.conf and no SMBIOS
# config (spec 0015a). build.ps1 writes the image only after restaging
# build/esp and verifies it against that tree, so the staged limine.conf is
# the one the image holds.
$diskImage = Join-Path $buildDirectory 'mirror.img'
if (-not (Test-Path -LiteralPath $diskImage -PathType Leaf) -or (Get-Item -LiteralPath $diskImage).Length -ne 1073741824) {
    throw "The disk image is missing or incomplete: $diskImage. Run scripts/build.ps1 first."
}
if ($Hygiene -and [System.IO.File]::ReadAllText((Join-Path $espDirectory 'limine.conf')) -match 'mirror\.perf=') {
    throw 'The hygiene probe boots without a mirror.perf= token, so the timing-marker exception (spec 0010-D4) stays off.'
}
$varsPath = Join-Path $buildDirectory 'capture.vars.fd'
Copy-Item -LiteralPath $varsTemplate -Destination $varsPath -Force
$captureName = if ($Hygiene) { 'hygiene-boot' } elseif ($RelativePointer) { 'relative-boot' } else { 'normal-boot' }
$serialLog = Join-Path $buildDirectory "$captureName.serial.log"
$screenshot = Join-Path $buildDirectory 'normal-boot.png'
$consoleScreenshot = Join-Path $buildDirectory 'normal-console.png'
$pointerScreenshot = Join-Path $buildDirectory 'normal-pointer.png'
$devicesScreenshot = Join-Path $buildDirectory 'normal-devices.png'
$devicesSecondScreenshot = Join-Path $buildDirectory 'normal-devices-page2.png'
$cursorAwayScreenshot = Join-Path $buildDirectory 'cursor-away.png'
$cursorRestoredScreenshot = Join-Path $buildDirectory 'cursor-restored.png'
$desktopScreenshot = Join-Path $buildDirectory 'desktop.png'
$desktopInputScreenshot = Join-Path $buildDirectory 'desktop-input.png'
$desktopAwayScreenshot = Join-Path $buildDirectory 'desktop-cursor-away.png'
$desktopRestoredScreenshot = Join-Path $buildDirectory 'desktop-cursor-restored.png'
[System.IO.File]::WriteAllText($serialLog, '')

# Every QMP event read while waiting for a command's response, in order. The
# restart step checks its SHUTDOWN event here (spec 0058a, test 5).
$qmpEvents = [System.Collections.Generic.List[object]]::new()

function Read-MirrorSerial {
    # The guest writes continuously. Share both read and write access so Windows
    # allows inspecting this invocation's serial log without closing QEMU.
    $stream = [System.IO.File]::Open($serialLog, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    $reader = [System.IO.StreamReader]::new($stream)
    try { return $reader.ReadToEnd() }
    finally { $reader.Dispose() }
}

function Assert-MirrorCaptureSmp {
    param([string]$Serial, [int]$RequestedCpus)
    Assert-MirrorTimer -Serial $Serial -Smoke $false -UseLegacyPic $false -ForcePit $false
    Assert-MirrorClock -Serial $Serial -Smoke $false -WithoutHpet $false
    Assert-MirrorHeapGrowth -Serial $Serial -RequestedCpus $RequestedCpus -Smoke $false -UseLegacyPic $false
    Assert-MirrorDriverCatalog -Serial $Serial -Smoke $false -LegacyDisk $false
    Assert-MirrorAhciMarkers -Serial $Serial -Attached $true -Smoke $false
    Assert-MirrorXhciMarkers -Serial $Serial -Attached $false -Smoke $false
    Assert-MirrorUsbMarkers -Serial $Serial -Attached $false -Smoke $false
    # Trace and default kernels report every CPU's guarded stacks too (spec 0016a).
    $null = Assert-MirrorStacks -Serial $Serial -OnlineCpus $RequestedCpus -Smoke $false
    # And the kernel VA window, without the smoke-only self-test (spec 0050-D4).
    Assert-MirrorKernelVa -Serial $Serial -OnlineCpus $RequestedCpus -Smoke $false
    # And per-CPU data, without the smoke-only probe (spec 0050-D4).
    Assert-MirrorCpuData -Serial $Serial -Smoke $false
    # And every CPU's protections (spec 0016b). The capture CPU,
    # qemu64,+smep,+smap, reports SMEP and SMAP but not UMIP (0016-D6).
    Assert-MirrorHardening -Serial $Serial -OnlineCpus $RequestedCpus -SmepSmap $true -Umip $false
    # And the BSP's speculative-execution exposure (spec 0016b).
    Assert-MirrorCpuVulnerability -Serial $Serial
    # Every capture ends with the console's restart (spec 0058a, test 5).
    Assert-MirrorReset -Serial $Serial -Smoke $false -ConsoleRestart $true
    if ($Serial -match '(?m)^MIRROR_(?:SMP_SKIPPED|SMP_CPU_FAILED|SMP_GUARD_OK|SMP_UNMAP_OK|SMP_TLB_TIMEOUT_ARMED|SHARED_TIMEOUT_\w+|SHARED_VM_SKIPPED|SHARED_OFFLINE_OK)(?:\s|$)') {
        throw 'Normal capture skipped SMP, lost a CPU, or ran an injected guard fault.'
    }
    $readyPattern = '(?m)^MIRROR_SMP_READY discovered={0} online={0} bsp=(\d+) aps={1} userspace_cpus=1\r?$' -f $RequestedCpus, ($RequestedCpus - 1)
    $ready = [regex]::Matches($Serial, $readyPattern)
    if ($ready.Count -ne 1 -or [regex]::Matches($Serial, '(?m)^MIRROR_SMP_READY(?:\s|$)').Count -ne 1) {
        throw "Normal capture must report all $RequestedCpus CPUs online exactly once."
    }
    if ([uint64]$ready[0].Groups[1].Value -ge 255) { throw 'Normal capture reported an invalid BSP APIC ID.' }
    $cpuRecords = [regex]::Matches($Serial, '(?m)^MIRROR_SMP_CPU slot=(\d+) apic=(\d+) stack=(0x[0-9a-f]+) cr3=(0x[0-9a-f]+)\r?$')
    if ($cpuRecords.Count -ne ($RequestedCpus - 1) -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SMP_CPU(?:\s|$)').Count -ne ($RequestedCpus - 1)) {
        throw 'Normal capture must validate every secondary CPU context exactly once.'
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
            $apicIdentity -ge 255 -or -not $apicIds.Add($apicIdentity) -or
            $stackTop -eq 0 -or ($stackTop % 16) -ne 0 -or -not $stacks.Add($stackTop) -or
            $rootAddress -eq 0 -or ($rootAddress % 4096) -ne 0) {
            throw 'Normal capture requires unique AP slots, IDs and aligned private stacks with valid roots.'
        }
        if ($secondaryRoot -eq 0) { $secondaryRoot = $rootAddress }
        elseif ($rootAddress -ne $secondaryRoot) { throw 'Normal capture APs must share the kernel root.' }
    }
    $phases = @(
        @{ Name = 'boot'; Rounds = 8 },
        @{ Name = 'after_userspace'; Rounds = 4 },
        @{ Name = 'after_desktop'; Rounds = 4 }
    )
    if ([regex]::Matches($Serial, '(?m)^MIRROR_SMP_IPI_OK(?:\s|$)').Count -ne $phases.Count) {
        throw 'Normal capture must complete exactly three SMP rendezvous phases.'
    }
    $previous = $ready[0].Index
    $sharedPattern = '(?m)^MIRROR_SHARED_VM_READY pages=512 cpus={0} rounds=1 reclaimed=3 borrowed_root=1\r?$' -f $RequestedCpus
    $shared = [regex]::Matches($Serial, $sharedPattern)
    if ($shared.Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_SHARED_VM_READY(?:\s|$)').Count -ne 1) {
        throw 'Normal capture must verify runtime shared mappings and complete frame reclamation on every CPU.'
    }
    foreach ($phase in $phases) {
        $pattern = '(?m)^MIRROR_SMP_IPI_OK phase={0} cpus={1} rounds={2}\r?$' -f $phase.Name, $RequestedCpus, $phase.Rounds
        $records = [regex]::Matches($Serial, $pattern)
        if ($records.Count -ne 1 -or $records[0].Index -le $previous) {
            throw "Normal capture has missing, incorrect, duplicate, or out-of-order SMP phase $($phase.Name)."
        }
        if (($phase.Name -eq 'boot' -and $shared[0].Index -le $records[0].Index) -or
            ($phase.Name -eq 'after_userspace' -and $shared[0].Index -ge $records[0].Index)) {
            throw 'Normal shared mapping checks must follow startup and precede the userspace rendezvous.'
        }
        $previous = $records[0].Index
    }
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
    # With -ExpectError the command must fail, and its QMP error is returned.
    # With -PassThru its return value is returned. Events are recorded.
    param([System.Diagnostics.Process]$Process, [string]$Command, [hashtable]$Arguments = @{}, [switch]$ExpectError, [switch]$PassThru)
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
            if ($response.PSObject.Properties['error']) {
                if ($ExpectError) { return $response.error }
                throw "QMP $Command failed: $($response.error | ConvertTo-Json -Compress)"
            }
            if (-not $response.PSObject.Properties['return']) { throw 'Malformed QMP response.' }
            if ($ExpectError) { throw "QMP $Command unexpectedly succeeded." }
            if ($PassThru) { return $response.return }
            return
        }
        if ($response.PSObject.Properties['event']) { $qmpEvents.Add($response) }
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
        if ($key -ne 'spc' -and $key -cnotmatch '^[a-z0-9]$') { throw 'Input probe accepts lowercase letters, digits, and spaces only.' }
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

function Wait-MirrorDesktopMessage {
    param([System.Diagnostics.Process]$Process, [string]$Pattern, [int]$AfterOffset = 0)
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw 'Kernel exited during desktop capture.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|MIRROR_USER_FAULT)(?:\s|:|$)') {
            throw "Desktop or kernel faulted. See $serialLog."
        }
        if ($serial.Length -lt $AfterOffset) { throw 'Serial log unexpectedly shrank.' }
        if ($serial.Substring($AfterOffset) -match $Pattern) { return }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "Desktop did not report $Pattern. See $serialLog." }
        Start-Sleep -Milliseconds 50
    } while ($true)
}

function Send-MirrorPointerMotion {
    param([System.Diagnostics.Process]$Process, [int]$Dx, [int]$Dy)
    # Keep each delta below a standard PS/2 packet's range, while allowing the
    # guest to clamp a sequence beyond the screen boundaries.
    while ($Dx -ne 0 -or $Dy -ne 0) {
        $stepX = [Math]::Max(-100, [Math]::Min($Dx, 100))
        $stepY = [Math]::Max(-100, [Math]::Min($Dy, 100))
        $events = @()
        if ($stepX -ne 0) { $events += @{ type = 'rel'; data = @{ axis = 'x'; value = $stepX } } }
        if ($stepY -ne 0) { $events += @{ type = 'rel'; data = @{ axis = 'y'; value = $stepY } } }
        Invoke-MirrorQmp $Process 'input-send-event' @{ events = $events }
        $Dx -= $stepX
        $Dy -= $stepY
        Start-Sleep -Milliseconds 40
    }
}

function Send-MirrorPointerButton {
    param([System.Diagnostics.Process]$Process, [string]$Button, [bool]$Down)
    # Mouse state is synchronized at the end of a QMP command. Separate calls
    # preserve both button edges instead of collapsing a click into no change.
    Invoke-MirrorQmp $Process 'input-send-event' @{ events = @(@{ type = 'btn'; data = @{ button = $Button; down = $Down } }) }
}

function ConvertTo-MirrorAbsoluteAxis {
    param([int]$Pixel, [int]$Extent)
    # QMP abs values span 0..0x7FFF, which QEMU's vmmouse scales to its
    # 0..0xFFFF device range; the kernel maps that range onto the framebuffer
    # with px = (x * (W - 1) + 0x7FFF) / 0xFFFF (spec 0017-D14). The nearest
    # QMP value, round(p * 0x7FFF / (W - 1)), maps back to pixel p exactly.
    if ($Extent -lt 1 -or $Pixel -lt 0 -or $Pixel -ge $Extent) {
        throw "Absolute pointer target $Pixel is outside a $Extent-pixel framebuffer axis."
    }
    if ($Extent -eq 1) { return 0 }
    $last = [int64]($Extent - 1)
    return [int][Math]::Floor((2 * [int64]$Pixel * 0x7FFF + $last) / (2 * $last))
}

function Send-MirrorPointerPosition {
    param([System.Diagnostics.Process]$Process, [int]$X, [int]$Y)
    # One input-send-event carries both abs axes, so the vmmouse queues a
    # single position for framebuffer pixel (X,Y). Buttons stay separate events.
    $events = @(
        @{ type = 'abs'; data = @{ axis = 'x'; value = (ConvertTo-MirrorAbsoluteAxis $X $framebufferSize.Width) } },
        @{ type = 'abs'; data = @{ axis = 'y'; value = (ConvertTo-MirrorAbsoluteAxis $Y $framebufferSize.Height) } }
    )
    Invoke-MirrorQmp $Process 'input-send-event' @{ events = $events }
    Start-Sleep -Milliseconds 40
}

function Move-MirrorPointer {
    param([System.Diagnostics.Process]$Process, [int]$X, [int]$Y, [int]$Dx, [int]$Dy)
    # Captures place the pointer at framebuffer pixel (X,Y) with one absolute
    # move and no homing step (spec 0017c, acceptance test 4). -RelativePointer
    # captures send the relative motion (Dx,Dy) instead (acceptance test 5).
    if ($RelativePointer) { Send-MirrorPointerMotion $Process $Dx $Dy }
    else { Send-MirrorPointerPosition $Process $X $Y }
}

function Move-MirrorDesktopPointer {
    param([System.Diagnostics.Process]$Process, [hashtable]$Desktop, [int]$X, [int]$Y, [int]$Dx, [int]$Dy)
    # (X,Y) is surface-local: absolute moves add the desktop's centered origin.
    Move-MirrorPointer $Process ($Desktop.X + $X) ($Desktop.Y + $Y) $Dx $Dy
}

function Get-MirrorDesktopRectangle {
    param([int]$Width = 0, [int]$Height = 0)
    # The kernel centers the desktop, capped at 1280x800, in the framebuffer
    # (kernel/src/surface.rs). A shell or desktop READY size must match it.
    $rectangle = @{ Width = [Math]::Min($framebufferSize.Width, 1280); Height = [Math]::Min($framebufferSize.Height, 800) }
    if ($Width -ne 0 -and ($Width -ne $rectangle.Width -or $Height -ne $rectangle.Height)) {
        throw "The ${Width}x$Height surface is not the centered desktop in the $($framebufferSize.Width)x$($framebufferSize.Height) framebuffer."
    }
    $rectangle.X = [int][Math]::Floor(($framebufferSize.Width - $rectangle.Width) / 2)
    $rectangle.Y = [int][Math]::Floor(($framebufferSize.Height - $rectangle.Height) / 2)
    return $rectangle
}

function Assert-MirrorAbsoluteRejected {
    param([System.Diagnostics.Process]$Process)
    # Spec 0017c, acceptance test 4: QEMU routes abs events only to a handler
    # that takes them, and the vmmouse registers one only after the guest
    # requests absolute mode. Before the mouse result, QMP must reject one.
    if ((Read-MirrorSerial) -match '(?m)^MIRROR_MOUSE_(?:READY|SKIPPED)(?:\s|$)') {
        throw "The guest reported its mouse before the early abs probe. See $serialLog."
    }
    $events = @(
        @{ type = 'abs'; data = @{ axis = 'x'; value = 0 } },
        @{ type = 'abs'; data = @{ axis = 'y'; value = 0 } }
    )
    $rejection = Invoke-MirrorQmp $Process 'input-send-event' @{ events = $events } -ExpectError
    if ($rejection.class -cne 'GenericError' -or $rejection.desc -cne 'Input handler not found for event type abs') {
        throw "QMP rejected the early abs event for an unexpected reason: $($rejection | ConvertTo-Json -Compress)"
    }
}

function Assert-MirrorPointerMode {
    param([string]$Serial)
    # Spec 0017-D18: q35's default vmport gives the vmmouse, and -RelativePointer
    # turns vmport off, so the mouse stays relative. Exactly one result, after
    # the PS/2 mouse's, and no fallback.
    $expected = if ($RelativePointer) { 'MIRROR_VMMOUSE_SKIPPED reason=no_backdoor' } else { 'MIRROR_VMMOUSE_READY mode=absolute' }
    $results = [regex]::Matches($Serial, '(?m)^MIRROR_VMMOUSE_(?:READY|SKIPPED|FALLBACK)(?:[ \t][^\r\n]*)?\r?$')
    $mouse = [regex]::Match($Serial, '(?m)^MIRROR_MOUSE_READY(?:\s|$)')
    if ($results.Count -ne 1 -or $results[0].Value.TrimEnd("`r") -cne $expected -or
        -not $mouse.Success -or $results[0].Index -le $mouse.Index) {
        throw "Captures require '$expected' once, after MIRROR_MOUSE_READY, and no vmmouse fallback. See $serialLog."
    }
}

function Wait-MirrorPointerMode {
    param([System.Diagnostics.Process]$Process)
    # The vmmouse result prints at boot after the framebuffer and the PS/2
    # mouse, before any pointer input. Return the framebuffer size that
    # absolute positions span.
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_VMMOUSE_(?:READY|SKIPPED)[^\r\n]*\r?\n'
    $serial = Read-MirrorSerial
    Assert-MirrorPointerMode $serial
    $framebuffer = [regex]::Matches($serial, '(?m)^MIRROR_FRAMEBUFFER_READY width=([1-9][0-9]*) height=([1-9][0-9]*) ')
    if ($framebuffer.Count -ne 1) { throw "Captures require exactly one MIRROR_FRAMEBUFFER_READY with the framebuffer size. See $serialLog." }
    return @{ Width = [int]$framebuffer[0].Groups[1].Value; Height = [int]$framebuffer[0].Groups[2].Value }
}

function Wait-MirrorPointer {
    param([System.Diagnostics.Process]$Process, [int]$X, [int]$Y, [int]$Buttons, [int]$AfterOffset)
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw 'Kernel exited during mouse input.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|MIRROR_CONSOLE_INPUT_LOST)(?:\s|:|$)') {
            throw "Kernel failed or lost PS/2 input. See $serialLog."
        }
        if ($serial.Length -lt $AfterOffset) { throw 'Serial log unexpectedly shrank.' }
        $fresh = $serial.Substring($AfterOffset)
        if ($fresh -match "(?m)^MIRROR_POINTER_UPDATED x=$X y=$Y buttons=$Buttons irq=[1-9][0-9]*\r?`$") { return }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "Pointer did not reach ($X,$Y) with buttons=$Buttons. See $serialLog." }
        Start-Sleep -Milliseconds 50
    } while ($true)
}

. (Join-Path $PSScriptRoot 'capture-shell.ps1')

function Wait-MirrorKernelFlavor {
    param([System.Diagnostics.Process]$Process)
    # test-trace kernels print MIRROR_TRACE_ON before PCI discovery (spec 0010).
    # Decide the flavor at PCI discovery, before any input, so a wrong staged
    # kernel fails fast instead of timing out on an oracle it never prints.
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw 'Kernel exited before PCI discovery.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION)(?:\s|:|$)') {
            throw "Kernel failed before PCI discovery. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_PCI_READY(?:\s|$)') {
            $traced = $serial -match '(?m)^MIRROR_TRACE_ON(?:\s|$)'
            if ($Hygiene -and $traced) {
                throw 'The hygiene probe needs the default kernel, but this kernel printed MIRROR_TRACE_ON. Run scripts/build.ps1 without -Trace or -Smoke.'
            }
            if (-not $Hygiene -and -not $traced) {
                throw 'Captures need a trace kernel, but this kernel printed no MIRROR_TRACE_ON before PCI discovery. Run scripts/build.ps1 -Trace.'
            }
            return
        }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "Kernel did not reach PCI discovery. See $serialLog." }
        Start-Sleep -Milliseconds 50
    } while ($true)
}

function Get-MirrorSerialLength {
    # Bytes, not decoded characters: the hygiene windows must stay byte-for-byte empty.
    $stream = [System.IO.File]::Open($serialLog, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try { return $stream.Length }
    finally { $stream.Dispose() }
}

function Wait-MirrorStableScreen {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Path,
        [string]$DifferentFrom
    )
    # Stable means two screendumps 1 s apart are identical, within 10 s (spec
    # 0010, acceptance test 3). With -DifferentFrom, a stable screen that still
    # equals that earlier one keeps waiting: the input under test must change it.
    $reference = if ($DifferentFrom) { (Get-FileHash -LiteralPath $DifferentFrom).Hash } else { $null }
    $previous = Join-Path $buildDirectory 'hygiene-previous.png'
    $unchanged = $false
    $elapsed = [System.Diagnostics.Stopwatch]::StartNew()
    Save-MirrorScreenshot $Process $previous
    do {
        Start-Sleep -Seconds 1
        if ($Process.HasExited) { throw 'Kernel exited during the hygiene probe.' }
        Save-MirrorScreenshot $Process $Path
        $hash = (Get-FileHash -LiteralPath $Path).Hash
        $stable = $hash -eq (Get-FileHash -LiteralPath $previous).Hash
        if ($stable -and $hash -ne $reference) { return }
        $unchanged = $stable
        Copy-Item -LiteralPath $Path -Destination $previous -Force
    } while ($elapsed.Elapsed.TotalSeconds -lt 10)
    if ($unchanged) { throw "The screen stayed identical to $DifferentFrom for 10 s, so the input never reached the guest." }
    throw "The screen did not settle within 10 s ($Path)."
}

function Invoke-MirrorHygieneInput {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Target,
        [string]$Text,
        [int]$PointerX,
        [int]$PointerY,
        [switch]$Submit
    )
    # Acceptance test 3, step 3 (and its repeat in step 4). A change between
    # stable screens proves the keys and the pointer motion reached the guest;
    # the serial log must not grow by a single byte meanwhile. The pointer,
    # at framebuffer pixel (PointerX,PointerY), moves 100 px right and 100 px
    # up: by relative motion, or by one absolute move (spec 0017c).
    $before = Join-Path $buildDirectory "hygiene-$Target-before.png"
    $typed = Join-Path $buildDirectory "hygiene-$Target-typed.png"
    $moved = Join-Path $buildDirectory "hygiene-$Target-pointer.png"
    Wait-MirrorStableScreen $Process $before
    $length = Get-MirrorSerialLength
    Send-MirrorText $Process $Text
    if ($Submit) { Send-MirrorKey $Process 'ret' }
    Wait-MirrorStableScreen $Process $typed -DifferentFrom $before
    Move-MirrorPointer $Process ($PointerX + 100) ($PointerY - 100) 100 (-100)
    Wait-MirrorStableScreen $Process $moved -DifferentFrom $typed
    $grown = (Get-MirrorSerialLength) - $length
    if ($grown -ne 0) {
        throw "The default kernel wrote $grown serial byte(s) while $Target received typed text and pointer motion. See $serialLog."
    }
}

function Invoke-MirrorHygieneProbe {
    param([System.Diagnostics.Process]$Process)
    # Spec 0010, acceptance test 3, on a default kernel with the decoy AHCI fixture.
    # Step 1: boot completes without a panic, trace output or the init battery.
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw 'Kernel exited before MIRROR_BOOT_OK.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|PANIC|EXCEPTION)(?:\s|:|$)') {
            throw "The default kernel panicked or faulted while booting with the decoy fixture. See $serialLog."
        }
        if ($serial -match '(?m)^MIRROR_BOOT_OK(?:\s|$)') { break }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "The default kernel did not report MIRROR_BOOT_OK. See $serialLog." }
        Start-Sleep -Milliseconds 100
    } while ($true)
    Assert-MirrorTraceMarker -Serial $serial -Expected $false
    if ([regex]::Matches($serial, '(?m)^MIRROR_AHCI_PROBE_OK fixture=0 reads=1\r?$').Count -ne 1) {
        throw "The default kernel must probe the decoy with one sector-0 read (MIRROR_AHCI_PROBE_OK fixture=0 reads=1). See $serialLog."
    }
    if ($serial -match '(?m)^MIRROR_USERSPACE_OK(?:\s|$)') {
        throw "The default kernel ran the init battery. See $serialLog."
    }

    # Step 2: BOOT_OK precedes input-session creation, which drains old input.
    # Wait for the actual shell launcher before sending one click. A stable boot
    # screen alone cannot prove readiness, and retrying a slow, unacknowledged
    # click can start a second Notes. Use the confirmed 0029 dark launcher pixels
    # (shell.rs launcher/paint, ui-tokens DEFAULT), without a default serial oracle.
    $bootScreen = Join-Path $buildDirectory 'hygiene-boot.png'
    Add-Type -AssemblyName System.Drawing
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw 'Kernel exited before the shell launcher appeared.' }
        $serial = Read-MirrorSerial
        if ($serial -match '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|MIRROR_USER_FAULT|MIRROR_SHELL_CLEANUP_OK|MIRROR_CONSOLE_READY)(?:\s|:|$)') {
            throw "Kernel or shell failed before the launcher appeared. See $serialLog."
        }
        Save-MirrorScreenshot $Process $bootScreen
        $image = [System.Drawing.Bitmap]::new($bootScreen)
        try {
            if ($image.Width -ne 1280 -or $image.Height -ne 800) {
                throw "The hygiene probe requires a 1280x800 display; got $($image.Width)x$($image.Height)."
            }
            $launcherReady = $true
            foreach ($point in @(
                    @(20, 760, 0x3B4A6B), @(21, 761, 0x232E44), @(107, 791, 0x3B4A6B),
                    @(116, 760, 0x3B4A6B), @(117, 761, 0x232E44), @(215, 791, 0x3B4A6B))) {
                if (($image.GetPixel($point[0], $point[1]).ToArgb() -band 0xFFFFFF) -ne $point[2]) {
                    $launcherReady = $false
                    break
                }
            }
        } finally { $image.Dispose() }
        if ($launcherReady) { break }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "The shell launcher did not appear. See $bootScreen and $serialLog." }
        Start-Sleep -Milliseconds 100
    } while ($true)
    # The Notes launcher sits at surface pixel (64,776). Default kernels print
    # no shell READY marker, so the desktop origin comes from the framebuffer
    # size: one absolute move gets there (spec 0017c), and -RelativePointer
    # clamps to the corner and moves by relative PS/2 motion instead.
    $desktop = Get-MirrorDesktopRectangle
    if ($RelativePointer) {
        Send-MirrorPointerMotion $Process (-2560) (-1600)
        Send-MirrorPointerMotion $Process 64 776
    }
    else {
        Send-MirrorPointerPosition $Process ($desktop.X + 64) ($desktop.Y + 776)
    }
    Send-MirrorPointerButton $Process 'left' $true
    Start-Sleep -Milliseconds 40
    Send-MirrorPointerButton $Process 'left' $false
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_APP_STARTED(?:\s|$)'
    $serial = Read-MirrorSerial
    if ([regex]::Matches($serial, '(?m)^MIRROR_APP_STARTED(?:\s|$)').Count -ne 1 -or
        [regex]::Matches($serial, '(?m)^MIRROR_APP_STARTED pid=[1-9][0-9]* kind=0\r?$').Count -ne 1) {
        throw "The launcher must start exactly one Notes application (MIRROR_APP_STARTED pid=N kind=0). See $serialLog."
    }

    # Step 3: Notes receives the sentinel and pointer motion with empty serial.
    # Step 2 left the pointer on the Notes launcher.
    Invoke-MirrorHygieneInput $Process 'notes' 'zqxjvw' ($desktop.X + 64) ($desktop.Y + 776)

    # Step 4: Esc ends the shell session; repeat on the kernel console, whose
    # pointer starts where MIRROR_POINTER_READY reports.
    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'esc'
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_SHELL_CLEANUP_OK(?:\s|$)' $offset
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_CONSOLE_READY(?:\s|$)' $offset
    $consolePointer = [regex]::Matches((Read-MirrorSerial).Substring($offset), '(?m)^MIRROR_POINTER_READY x=(\d+) y=(\d+) ')
    if ($consolePointer.Count -ne 1) { throw "The console must report its pointer once. See $serialLog." }
    Invoke-MirrorHygieneInput $Process 'console' 'echo zqxjvw' ([int]$consolePointer[0].Groups[1].Value) ([int]$consolePointer[0].Groups[2].Value) -Submit
}

function Assert-MirrorHygieneLog {
    param([string]$Serial)
    # Step 5: the whole log holds no typed text, no user output, no per-input
    # markers and no shell or desktop oracles, plus the step 1 and 2 invariants.
    if ($Serial.IndexOf('zqxjvw', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "The default kernel leaked the typed sentinel to serial. See $serialLog."
    }
    foreach ($pattern in @(
            '(?m)^user: ',
            '(?m)^MIRROR_CONSOLE_(?:COMMAND|UPDATED|INPUT irq)',
            '(?m)^MIRROR_POINTER_(?:UPDATED|CLICK)',
            '(?m)^MIRROR_(?:SHELL|DESKTOP)_(?:READY|FRAME)',
            'Mirror (?:init|app|shell|desktop|agent):',
            '(?m)^(?:MIRROR_PANIC|MIRROR_EXCEPTION|PANIC|EXCEPTION)(?:\s|:|$)',
            '(?m)^MIRROR_USERSPACE_OK(?:\s|$)')) {
        $count = [regex]::Matches($Serial, $pattern).Count
        if ($count -ne 0) { throw "The default kernel printed $count line(s) matching '$pattern'. See $serialLog." }
    }
    Assert-MirrorTraceMarker -Serial $Serial -Expected $false
    if ([regex]::Matches($Serial, '(?m)^MIRROR_APP_STARTED(?:\s|$)').Count -ne 1 -or
        [regex]::Matches($Serial, '(?m)^MIRROR_APP_STARTED pid=[1-9][0-9]* kind=0\r?$').Count -ne 1) {
        throw "The hygiene probe must start exactly one Notes application. See $serialLog."
    }
    foreach ($marker in @('MIRROR_BOOT_OK', 'MIRROR_SHELL_CLEANUP_OK', 'MIRROR_CONSOLE_READY')) {
        if ([regex]::Matches($Serial, "(?m)^$marker(?:\s|$)").Count -ne 1) {
            throw "The hygiene probe requires exactly one $marker. See $serialLog."
        }
    }
}

function Invoke-MirrorRestart {
    # Spec 0058a, test 5: every capture ends by typing restart at the kernel
    # console. Under -no-reboot -no-shutdown the guest reset stops the VM with
    # one SHUTDOWN event (guest-reset), and quit then exits QEMU with 0.
    param([System.Diagnostics.Process]$Process)
    $length = Get-MirrorSerialLength
    Send-MirrorText $Process 'restart'
    Send-MirrorKey $Process 'ret'
    # Trace kernels log the command and redraw before restarting.
    if (-not $Hygiene) { Wait-MirrorConsoleCommand $Process 'operation=restart text=' }
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if ($Process.HasExited) { throw "QEMU exited on the guest reset instead of stopping. See $serialLog." }
        $status = Invoke-MirrorQmp $Process 'query-status' -PassThru
        if ($status.status -eq 'shutdown') { break }
        if ($deadline.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "The restart command did not reset the guest (status $($status.status)). See $serialLog." }
        Start-Sleep -Milliseconds 100
    } while ($true)
    if ($status.running) { throw 'The VM kept running after the guest reset.' }
    $shutdowns = @($qmpEvents | Where-Object { $_.event -eq 'SHUTDOWN' })
    if ($shutdowns.Count -ne 1 -or $shutdowns[0].data.guest -ne $true -or $shutdowns[0].data.reason -ne 'guest-reset') {
        throw "QMP must report exactly one SHUTDOWN event, from the guest, with reason guest-reset; got $($shutdowns | ConvertTo-Json -Compress -Depth 4)."
    }
    if ($Hygiene) {
        # The default kernel prints nothing for the typing; the only bytes
        # after it are the restart's two lifecycle lines.
        $stream = [System.IO.File]::Open($serialLog, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $tail = [byte[]]::new($stream.Length - $length)
            $null = $stream.Seek($length, [System.IO.SeekOrigin]::Begin)
            if ($stream.Read($tail, 0, $tail.Length) -ne $tail.Length) { throw 'Short serial log read.' }
        }
        finally { $stream.Dispose() }
        $expected = "MIRROR_RESET_BEGIN cause=console`r`nMIRROR_RESET_ATTEMPT method=fadt`r`n"
        if ([System.Text.Encoding]::ASCII.GetString($tail) -cne $expected) {
            throw "After the typed restart the default kernel must print only its two reset lines. See $serialLog."
        }
    }
}

function Complete-MirrorCapture {
    param([System.Diagnostics.Process]$Process)
    # QMP permits quit to terminate before returning a response. Require clean
    # process exit below rather than interpreting that allowed EOF as failure.
    $Process.StandardInput.WriteLine('{"execute":"quit"}')
    $Process.StandardInput.Flush()
    if (-not $Process.WaitForExit(5000)) { throw 'QEMU did not exit after the capture.' }
    $Process.WaitForExit()
    if ($Process.ExitCode -ne 0) { throw "QEMU capture exited with code $($Process.ExitCode)." }
    # Read the complete log after exit, so later input cannot hide an AP failure.
    # These are trace or default builds; smoke-only self-tests and injected faults are absent.
    $serial = Read-MirrorSerial
    if ($Hygiene) { Assert-MirrorHygieneLog $serial }
    else { Assert-MirrorTraceMarker -Serial $serial -Expected $true }
    Assert-MirrorPointerMode $serial
    Assert-MirrorSchedReady -Serial $serial
    if ($serial -match '(?m)^MIRROR_CONSOLE_INPUT_LOST(?:\s|$)') { throw "The capture lost console input. See $serialLog." }
    Assert-MirrorCaptureSmp -Serial $serial -RequestedCpus $CpuCount
    if ((Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash -ne $testDiskHash) {
        throw 'The read-only block fixture was modified during the capture.'
    }
    if ((Get-Item -LiteralPath $fixtureImage).Length -ne 1048576 -or
        (Get-FileHash -LiteralPath $fixtureImage -Algorithm SHA256).Hash -ne $fixtureHash) {
        throw 'The AHCI read-only probe modified its disposable disk image during the capture.'
    }
}

# Captures use run.ps1's default smoke CPU, so the shell, apps and console run
# with SMEP and SMAP on (0016-D6). q35 keeps its default vmport, so captures
# point through the vmmouse; -RelativePointer turns it off (0017-D18).
$machine = if ($RelativePointer) { 'q35,accel=tcg,vmport=off' } else { 'q35,accel=tcg' }
$qemuArguments = @(
    '-machine', $machine, '-cpu', 'qemu64,+smep,+smap', '-m', '256', '-smp', "$CpuCount",
    '-display', 'none', '-vga', 'std', '-monitor', 'none', '-qmp', 'stdio',
    '-nic', 'none', '-no-reboot', '-no-shutdown',
    '-drive', "if=pflash,unit=0,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $codePath)",
    '-drive', "if=pflash,unit=1,format=raw,file=$(ConvertTo-QemuOptionValue $varsPath)",
    '-drive', "if=none,id=mirror-disk,format=raw,readonly=on,file=$(ConvertTo-QemuOptionValue $diskImage)",
    '-device', 'virtio-blk-pci,drive=mirror-disk,disable-legacy=on,bootindex=0', '-serial', "file:$serialLog"
)
# Captures use the V1 device profile: the e1000 NIC and the AHCI SATA fixture.
# The QEMU ATA model needs a writable backend, so the fixture is the fresh
# disposable copy build/ahci-test.img, verified in full after QEMU exits.
$deviceArguments = Get-MirrorDeviceArguments -Profile V1 -RepositoryRoot $repositoryRoot
$testDisk = Get-MirrorTestDisk -RepositoryRoot $repositoryRoot
$testDiskHash = (Get-FileHash -LiteralPath $testDisk -Algorithm SHA256).Hash
$fixtureImage = $deviceArguments.FixtureImage
$fixtureHash = $testDiskHash
if ($Hygiene) {
    # Acceptance test 3 replaces build/ahci-test.img with a decoy: the fixture with
    # file offset 612 (sector 1, byte 100) inverted. Sector 0 keeps the fixture
    # signature, so a kernel that still recognized fixtures would accept the decoy
    # and then panic in its data check.
    $decoy = [System.IO.File]::ReadAllBytes($testDisk)
    if ($decoy.Length -ne 1048576 -or ($decoy[8..15] -join ',') -ne '77,73,82,82,79,82,0,1') {
        throw 'The block fixture lacks its sector-0 signature.'
    }
    $decoy[612] = [byte]($decoy[612] -bxor 0xff)
    $fixtureImage = Join-Path $buildDirectory 'ahci-decoy.img'
    [System.IO.File]::WriteAllBytes($fixtureImage, $decoy)
    $decoyHash = [System.Security.Cryptography.SHA256]::Create()
    try { $fixtureHash = [BitConverter]::ToString($decoyHash.ComputeHash($decoy)).Replace('-', '') }
    finally { $decoyHash.Dispose() }
}
$qemuArguments += $deviceArguments.NicArguments
$qemuArguments += @(
    '-drive', "if=none,id=mirror-test,format=raw,readonly=off,file=$(ConvertTo-QemuOptionValue $fixtureImage)",
    '-device', $deviceArguments.FixtureDevice
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
    Assert-MirrorAbsoluteRejected $process
    Wait-MirrorKernelFlavor $process
    $framebufferSize = Wait-MirrorPointerMode $process
    $pointerMode = if ($RelativePointer) { 'relative' } else { 'absolute' }
    if ($Hygiene) {
        Invoke-MirrorHygieneProbe $process
        Invoke-MirrorRestart $process
        Complete-MirrorCapture $process
        Write-Host "Default kernel hygiene probe passed: decoy fixture probed, no user content or per-input markers on serial (pointer=$pointerMode, cpus=$CpuCount). Images: $buildDirectory"
        return
    }
    Invoke-MirrorShellCapture $process
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
    foreach ($marker in @('MIRROR_BOOT_OK', 'MIRROR_PCI_READY', 'MIRROR_FRAMEBUFFER_READY', 'MIRROR_FRAMEBUFFER_PRESENTED', 'MIRROR_AHCI_READY', 'MIRROR_AHCI_PROBE_OK fixture=0 reads=1', 'MIRROR_AHCI_CLEANUP_OK', 'MIRROR_BLOCK_SKIPPED', 'MIRROR_KEYBOARD_READY', 'MIRROR_MOUSE_READY', 'MIRROR_POINTER_READY')) {
        if ($serial -notmatch "(?m)^$marker(?:\s|$)") { throw "Normal boot missing $marker. See $serialLog." }
    }
    # Trace kernels run no init battery and recognize no fixture (spec 0010).
    foreach ($marker in @('MIRROR_USERSPACE_OK', 'MIRROR_USER_CLEANUP_OK', 'MIRROR_SCHEDULER_OK', 'MIRROR_IPC_OK', 'MIRROR_PREEMPT_OK', 'MIRROR_SCHED_BOUNDS_OK', 'MIRROR_KVA_TESTS_OK', 'MIRROR_USER_SURFACE_OK', 'MIRROR_USER_SURFACE_CLEANUP_OK', 'MIRROR_USER_CLOCK_OK', 'MIRROR_AHCI_READ_OK', 'MIRROR_HEAP_CHARGE_OK', 'MIRROR_CPU_DATA_TESTS_OK', 'MIRROR_KSTACK_OK', 'MIRROR_KSTACK_GUARD', 'MIRROR_KVA_DEGRADED')) {
        if ($serial -match "(?m)^$marker(?:\s|$)") { throw "Trace boot unexpectedly reported $marker. See $serialLog." }
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

    $pointerReady = [regex]::Match($serial, '(?m)^MIRROR_POINTER_READY x=(\d+) y=(\d+) width=(\d+) height=(\d+)\r?$')
    if (-not $pointerReady.Success) { throw 'Pointer geometry is missing.' }
    $startX = [int]$pointerReady.Groups[1].Value
    $startY = [int]$pointerReady.Groups[2].Value
    $width = [int]$pointerReady.Groups[3].Value
    $height = [int]$pointerReady.Groups[4].Value
    # The console pointer spans the framebuffer, as absolute positions do.
    if ($width -ne $framebufferSize.Width -or $height -ne $framebufferSize.Height) {
        throw "The console pointer spans ${width}x$height, not the $($framebufferSize.Width)x$($framebufferSize.Height) framebuffer."
    }
    $offset = (Read-MirrorSerial).Length
    Move-MirrorPointer $process 0 0 (-2 * $width) (-2 * $height)
    Wait-MirrorPointer $process 0 0 0 $offset
    Save-MirrorScreenshot $process $cursorAwayScreenshot
    if ((Get-FileHash -LiteralPath $consoleScreenshot).Hash -eq (Get-FileHash -LiteralPath $cursorAwayScreenshot).Hash) {
        throw 'Cursor motion did not change the framebuffer.'
    }
    $offset = (Read-MirrorSerial).Length
    Move-MirrorPointer $process ($width - 1) ($height - 1) (2 * $width) (2 * $height)
    Wait-MirrorPointer $process ($width - 1) ($height - 1) 0 $offset
    $offset = (Read-MirrorSerial).Length
    Move-MirrorPointer $process $startX $startY ($startX - $width + 1) ($startY - $height + 1)
    Wait-MirrorPointer $process $startX $startY 0 $offset
    Save-MirrorScreenshot $process $cursorRestoredScreenshot
    if ((Get-FileHash -LiteralPath $consoleScreenshot).Hash -ne (Get-FileHash -LiteralPath $cursorRestoredScreenshot).Hash) {
        throw 'Moving the cursor left stale pixels after returning to its starting position.'
    }

    # Place the caret before c in "echo abcd", then insert x through the
    # keyboard. Exact output verifies that pointer and keyboard input cooperate.
    Send-MirrorText $process 'echo abcd'
    $large = $width -ge 640 -and $height -ge 400
    $scale = if ($large) { 2 } else { 1 }
    $margin = if ($large) { 24 } else { 0 }
    $inset = if ($large) { 24 } else { 4 }
    $footer = if ($large) { 80 } else { 28 }
    $clickX = $margin + $inset + (2 + 7) * 6 * $scale + 1
    $clickY = $margin + [Math]::Max(0, $height - 2 * $margin - $footer) + 3 * $scale
    if ($clickX -ge $width -or $clickY -ge $height) { throw 'Display is too small for the pointer editing probe.' }
    $offset = (Read-MirrorSerial).Length
    Move-MirrorPointer $process $clickX $clickY ($clickX - $startX) ($clickY - $startY)
    Wait-MirrorPointer $process $clickX $clickY 0 $offset
    $offset = (Read-MirrorSerial).Length
    Send-MirrorPointerButton $process 'left' $true
    Wait-MirrorPointer $process $clickX $clickY 1 $offset
    if ((Read-MirrorSerial).Substring($offset) -notmatch "(?m)^MIRROR_POINTER_CLICK x=$clickX y=$clickY caret=7\r?`$") {
        throw 'Left click did not place the editor caret at the intended character.'
    }
    $offset = (Read-MirrorSerial).Length
    Send-MirrorPointerButton $process 'left' $false
    Wait-MirrorPointer $process $clickX $clickY 0 $offset
    foreach ($key in @('x', 'end', 'ret')) { Send-MirrorKey $process $key }
    Wait-MirrorConsoleCommand $process 'operation=echo text=abxcd'
    Save-MirrorScreenshot $process $pointerScreenshot
    # The installation fallback must expose the hardware catalog through real
    # keyboard input as well as serial diagnostics, without a desktop service.
    Send-MirrorText $process 'devices'
    Send-MirrorKey $process 'ret'
    Wait-MirrorConsoleCommand $process 'operation=devices text=page=1'
    Save-MirrorScreenshot $process $devicesScreenshot
    if ((Get-FileHash -LiteralPath $pointerScreenshot).Hash -eq (Get-FileHash -LiteralPath $devicesScreenshot).Hash) {
        throw 'The devices command did not change the visible fallback console.'
    }
    Send-MirrorText $process 'devices 2'
    Send-MirrorKey $process 'ret'
    Wait-MirrorConsoleCommand $process 'operation=devices text=page=2'
    Save-MirrorScreenshot $process $devicesSecondScreenshot
    if ((Get-FileHash -LiteralPath $devicesScreenshot).Hash -eq (Get-FileHash -LiteralPath $devicesSecondScreenshot).Hash) {
        throw 'Device catalog pagination did not change the visible fallback console.'
    }
    Invoke-MirrorRestart $process
    Complete-MirrorCapture $process
    Write-Host "Native desktop and fallback console captures passed (pointer=$pointerMode, cpus=$CpuCount). Images: $buildDirectory"
}
finally {
    if ($started) {
        if (-not $process.HasExited) {
            $process.Kill()
            $process.WaitForExit()
        }
        if ($null -ne $stderr) {
            [System.IO.File]::WriteAllText((Join-Path $buildDirectory "$captureName.stderr.log"), $stderr.GetAwaiter().GetResult())
        }
    }
    $process.Dispose()
}
