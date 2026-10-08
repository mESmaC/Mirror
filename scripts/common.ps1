Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-QemuOptionValue {
    param([Parameter(Mandatory = $true)][string]$Value)
    # QEMU's comma-separated option syntax escapes a literal comma by doubling it.
    return $Value.Replace(',', ',,')
}

function ConvertTo-WindowsProcessArgument {
    param([AllowEmptyString()][string]$Argument)
    # Start-Process joins ArgumentList into a Windows command line. Quote each
    # argument using the Windows C-runtime rules, including trailing backslashes.
    if ($Argument.Length -eq 0) { return '""' }
    if ($Argument -notmatch '[\s"]') { return $Argument }
    $escaped = [regex]::Replace($Argument, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Get-MirrorQemu {
    param([string]$RequestedPath, [string]$RepositoryRoot)

    if ($RequestedPath) {
        $candidate = $RequestedPath
        if (Test-Path -LiteralPath $candidate -PathType Container) {
            $candidate = Join-Path $candidate 'qemu-system-x86_64.exe'
        }
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "QEMU executable does not exist: $candidate"
        }
        return (Resolve-Path -LiteralPath $candidate).Path
    }

    $localQemu = Join-Path $RepositoryRoot '.tools/qemu/qemu-system-x86_64.exe'
    if (Test-Path -LiteralPath $localQemu -PathType Leaf) {
        return (Resolve-Path -LiteralPath $localQemu).Path
    }
    $command = Get-Command 'qemu-system-x86_64.exe' -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    throw 'QEMU was not found. Set QEMU_PATH to its executable or directory, pass -QemuPath, or place it in .tools/qemu/.'
}

function Get-MirrorFirmware {
    param(
        [string]$RequestedPath,
        [string]$QemuDirectory,
        [string[]]$Names,
        [string]$ParameterName
    )

    if ($RequestedPath) {
        if (-not (Test-Path -LiteralPath $RequestedPath -PathType Leaf)) {
            throw "Firmware does not exist: $RequestedPath"
        }
        return (Resolve-Path -LiteralPath $RequestedPath).Path
    }
    $searchDirectories = @(
        $QemuDirectory,
        (Join-Path $QemuDirectory 'share'),
        (Join-Path $QemuDirectory 'share/qemu'),
        (Join-Path $QemuDirectory 'pc-bios')
    )
    foreach ($directory in $searchDirectories) {
        foreach ($name in $Names) {
            $candidate = Join-Path $directory $name
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return (Resolve-Path -LiteralPath $candidate).Path
            }
        }
    }
    throw "UEFI firmware was not found beside QEMU. Pass -$ParameterName with the appropriate OVMF file."
}
