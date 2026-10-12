# Loaded by capture.ps1 after its shared QMP, input and screenshot helpers.
# These checks use real guest input and post-flush shell diagnostics as fences.

function Wait-MirrorShellMessage {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Message,
        [int]$AfterOffset
    )
    $escaped = [regex]::Escape('user: Mirror shell: ' + $Message + '\n')
    Wait-MirrorDesktopMessage $Process "(?m)^$escaped\r?`$" $AfterOffset
}

function Move-MirrorShellPointer {
    param(
        [System.Diagnostics.Process]$Process,
        [hashtable]$Pointer,
        [int]$X,
        [int]$Y
    )
    if ($X -lt 0 -or $X -ge $Pointer.Width -or $Y -lt 0 -or $Y -ge $Pointer.Height) {
        throw "Shell pointer target is out of bounds: ($X,$Y)."
    }
    if ($Pointer.X -eq $X -and $Pointer.Y -eq $Y) { return }
    $offset = (Read-MirrorSerial).Length
    Move-MirrorDesktopPointer $Process $Pointer.Desktop $X $Y ($X - $Pointer.X) ($Y - $Pointer.Y)
    Wait-MirrorShellMessage $Process "pointer $X $Y $($Pointer.Buttons)" $offset
    $Pointer.X = $X
    $Pointer.Y = $Y
}

function Set-MirrorShellButton {
    param(
        [System.Diagnostics.Process]$Process,
        [hashtable]$Pointer,
        [bool]$Down
    )
    $buttons = if ($Down) { 1 } else { 0 }
    $offset = (Read-MirrorSerial).Length
    Send-MirrorPointerButton $Process 'left' $Down
    Wait-MirrorShellMessage $Process "pointer $($Pointer.X) $($Pointer.Y) $buttons" $offset
    $Pointer.Buttons = $buttons
}

function Invoke-MirrorShellClick {
    param(
        [System.Diagnostics.Process]$Process,
        [hashtable]$Pointer,
        [int]$X,
        [int]$Y
    )
    Move-MirrorShellPointer $Process $Pointer $X $Y
    Set-MirrorShellButton $Process $Pointer $true
    Set-MirrorShellButton $Process $Pointer $false
}

function Sync-MirrorShellFrame {
    param([System.Diagnostics.Process]$Process, [hashtable]$Pointer)
    # App diagnostics follow a private frame commit. Request a later shell
    # repaint so screenshots cannot race the compositor consuming that frame.
    $originalX = $Pointer.X
    $nextX = if ($originalX + 1 -lt $Pointer.Width) { $originalX + 1 } else { $originalX - 1 }
    Move-MirrorShellPointer $Process $Pointer $nextX $Pointer.Y
    Move-MirrorShellPointer $Process $Pointer $originalX $Pointer.Y
}

function Start-MirrorCapturedApp {
    param(
        [System.Diagnostics.Process]$Process,
        [hashtable]$Pointer,
        [int]$Slot,
        [int]$Kind
    )
    $launcherX = if ($Kind -eq 0) { 64 } else { 164 }
    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $Pointer $launcherX ($Pointer.Height - 24)
    $pattern = "(?m)^user: Mirror shell: launch $Slot ([1-9][0-9]*) $Kind\\n\r?`$"
    Wait-MirrorDesktopMessage $Process $pattern $offset
    $match = [regex]::Match((Read-MirrorSerial).Substring($offset), $pattern)
    if (-not $match.Success) { throw 'Shell launch marker disappeared.' }
    $applicationPid = [long]$match.Groups[1].Value
    Wait-MirrorDesktopMessage $Process "(?m)^user: Mirror app: ready $applicationPid $Kind\\n\r?`$" $offset
    Sync-MirrorShellFrame $Process $Pointer
    return $applicationPid
}

function Wait-MirrorAppText {
    param(
        [System.Diagnostics.Process]$Process,
        [long]$ApplicationPid,
        [string]$Text,
        [int]$AfterOffset
    )
    $escaped = [regex]::Escape("user: Mirror app: text $ApplicationPid $Text" + '\n')
    Wait-MirrorDesktopMessage $Process "(?m)^$escaped\r?`$" $AfterOffset
}

function Assert-MirrorAppText {
    param([long]$ApplicationPid, [string]$Expected)
    $matches = [regex]::Matches((Read-MirrorSerial), "(?m)^user: Mirror app: text $ApplicationPid (.*)\\n\r?`$")
    if ($matches.Count -eq 0 -or $matches[$matches.Count - 1].Groups[1].Value -cne $Expected) {
        throw "Application $ApplicationPid did not retain the expected document: $Expected."
    }
}

function Wait-MirrorAppReclaimed {
    param(
        [System.Diagnostics.Process]$Process,
        [long]$ApplicationPid,
        [int]$AfterOffset
    )
    Wait-MirrorDesktopMessage $Process "(?m)^MIRROR_APP_RECLAIMED pid=$ApplicationPid\b[^\r\n]*\bframes_reclaimed=1(?:\s|`$)" $AfterOffset
}

function Invoke-MirrorShellCapture {
    param([System.Diagnostics.Process]$Process)
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_SHELL_READY(?:\s|$)'
    $ready = [regex]::Match((Read-MirrorSerial), '(?m)^MIRROR_SHELL_READY .*width=(\d+) height=(\d+)(?:\s|$)')
    if (-not $ready.Success) { throw 'Shell surface geometry is missing.' }
    $width = [int]$ready.Groups[1].Value
    $height = [int]$ready.Groups[2].Value
    if ($width -ne 1280 -or $height -ne 800) {
        throw "Shell capture currently requires 1280x800; got ${width}x$height."
    }
    $pointer = @{
        X = [int][Math]::Floor($width / 2)
        Y = [int][Math]::Floor($height / 2)
        Width = $width
        Height = $height
        Buttons = 0
        Desktop = Get-MirrorDesktopRectangle $width $height
    }
    Save-MirrorScreenshot $Process $desktopScreenshot
    Move-MirrorShellPointer $Process $pointer 0 0
    Save-MirrorScreenshot $Process $desktopAwayScreenshot
    if ((Get-FileHash -LiteralPath $desktopScreenshot).Hash -eq (Get-FileHash -LiteralPath $desktopAwayScreenshot).Hash) {
        throw 'Shell cursor movement did not change the display.'
    }
    Move-MirrorShellPointer $Process $pointer ($width - 1) ($height - 1)
    Move-MirrorShellPointer $Process $pointer ($width / 2) ($height / 2)
    Save-MirrorScreenshot $Process $desktopRestoredScreenshot
    if ((Get-FileHash -LiteralPath $desktopScreenshot).Hash -ne (Get-FileHash -LiteralPath $desktopRestoredScreenshot).Hash) {
        throw 'Shell cursor left stale pixels on the empty desktop.'
    }

    $firstPid = Start-MirrorCapturedApp $Process $pointer 0 0
    $offset = (Read-MirrorSerial).Length
    Send-MirrorText $Process 'alpha'
    Wait-MirrorAppText $Process $firstPid 'alpha' $offset
    # The app owns caret hit testing. Verify a first client click and edit,
    # then restore the document used by the independent-window checks below.
    Invoke-MirrorShellClick $Process $pointer 110 203
    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'x'
    Wait-MirrorAppText $Process $firstPid 'alxpha' $offset
    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'backspace'
    Wait-MirrorAppText $Process $firstPid 'alpha' $offset
    $secondPid = Start-MirrorCapturedApp $Process $pointer 1 0
    if ($firstPid -eq $secondPid) { throw 'Two windows share an application PID.' }
    $offset = (Read-MirrorSerial).Length
    Send-MirrorText $Process 'beta'
    Wait-MirrorAppText $Process $secondPid 'beta' $offset
    Assert-MirrorAppText $firstPid 'alpha'
    Sync-MirrorShellFrame $Process $pointer
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-windows.png')

    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 100 114
    Wait-MirrorShellMessage $Process 'focus 0' $offset
    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'end'
    Send-MirrorKey $Process 'x'
    Wait-MirrorAppText $Process $firstPid 'alphax' $offset
    Assert-MirrorAppText $secondPid 'beta'
    Sync-MirrorShellFrame $Process $pointer

    # A title drag crosses the sibling's rectangle, exercising stacking and
    # composition without sending these mouse movements to either document.
    Move-MirrorShellPointer $Process $pointer 100 114
    $offset = (Read-MirrorSerial).Length
    Set-MirrorShellButton $Process $pointer $true
    Move-MirrorShellPointer $Process $pointer 320 200
    Set-MirrorShellButton $Process $pointer $false
    Wait-MirrorShellMessage $Process 'move 0 300 186' $offset
    Assert-MirrorAppText $firstPid 'alphax'
    Assert-MirrorAppText $secondPid 'beta'
    Save-MirrorScreenshot $Process $desktopInputScreenshot
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-moved.png')

    # Both applications are static Notes instances here: exact cursor restoration
    # also proves that moving over overlapping windows preserves their pixels.
    Move-MirrorShellPointer $Process $pointer 0 0
    Save-MirrorScreenshot $Process $desktopAwayScreenshot
    if ((Get-FileHash -LiteralPath $desktopInputScreenshot).Hash -eq (Get-FileHash -LiteralPath $desktopAwayScreenshot).Hash) {
        throw 'Cursor movement over application windows did not change pixels.'
    }
    Move-MirrorShellPointer $Process $pointer ($width - 1) ($height - 1)
    Move-MirrorShellPointer $Process $pointer 320 200
    Save-MirrorScreenshot $Process $desktopRestoredScreenshot
    if ((Get-FileHash -LiteralPath $desktopInputScreenshot).Hash -ne (Get-FileHash -LiteralPath $desktopRestoredScreenshot).Hash) {
        throw 'Shell cursor left stale pixels over overlapping applications.'
    }

    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 577 200
    Wait-MirrorShellMessage $Process 'minimize 0' $offset
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-minimized.png')
    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 310 ($height - 24)
    Wait-MirrorShellMessage $Process 'restore 0' $offset
    Assert-MirrorAppText $firstPid 'alphax'
    if ((Read-MirrorSerial).Substring($offset) -match '(?m)^user: Mirror shell: launch 0 ') {
        throw 'Restoring a minimized application launched a replacement process.'
    }
    Sync-MirrorShellFrame $Process $pointer

    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 734 174
    Wait-MirrorShellMessage $Process "close 1 $secondPid" $offset
    Wait-MirrorAppReclaimed $Process $secondPid $offset
    $reopenedPid = Start-MirrorCapturedApp $Process $pointer 1 0
    if ($reopenedPid -eq $firstPid -or $reopenedPid -eq $secondPid) {
        throw 'Relaunch reused an earlier application PID.'
    }
    $offset = (Read-MirrorSerial).Length
    Send-MirrorText $Process 'gamma'
    Wait-MirrorAppText $Process $reopenedPid 'gamma' $offset
    Assert-MirrorAppText $firstPid 'alphax'
    Sync-MirrorShellFrame $Process $pointer
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-reopened.png')

    # Both slots are in use, so the Counter launcher is refused (spec 0029-D8):
    # the shell shows its warning toast and starts no application. The close
    # below clears the toast.
    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 164 ($height - 24)
    Wait-MirrorShellMessage $Process 'refused launch 1' $offset
    if ((Read-MirrorSerial).Substring($offset) -match '(?m)^(?:user: Mirror shell: launch |MIRROR_APP_STARTED(?:\s|$))') {
        throw 'A refused launch started an application.'
    }
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-launch-refused.png')

    # Focus the exposed portion of the first title before using its close button:
    # the reopened sibling otherwise covers that button at the overlap.
    Invoke-MirrorShellClick $Process $pointer 320 200
    $offset = (Read-MirrorSerial).Length
    Invoke-MirrorShellClick $Process $pointer 604 200
    Wait-MirrorShellMessage $Process "close 0 $firstPid" $offset
    Wait-MirrorAppReclaimed $Process $firstPid $offset
    $counterPid = Start-MirrorCapturedApp $Process $pointer 0 1
    if ($counterPid -in @($firstPid, $secondPid, $reopenedPid)) {
        throw 'Counter launch reused an earlier application PID.'
    }
    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'ret'
    Wait-MirrorDesktopMessage $Process "(?m)^user: Mirror app: reset $counterPid\\n\r?`$" $offset
    Assert-MirrorAppText $reopenedPid 'gamma'
    Sync-MirrorShellFrame $Process $pointer
    Save-MirrorScreenshot $Process (Join-Path $buildDirectory 'desktop-counter.png')

    $offset = (Read-MirrorSerial).Length
    Send-MirrorKey $Process 'esc'
    Wait-MirrorDesktopMessage $Process '(?m)^MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1(?:\s|$)' $offset
    Wait-MirrorAppReclaimed $Process $counterPid $offset
    Wait-MirrorAppReclaimed $Process $reopenedPid $offset
    if ((Read-MirrorSerial) -match '(?m)^MIRROR_AGENT_READY(?:\s|$)') {
        throw 'Default desktop unexpectedly launched an agent.'
    }
    Write-Host 'Shell capture passed: independent apps, focus, drag, minimize/restore, close/relaunch, refused launch, Counter, and session cleanup.'
}
