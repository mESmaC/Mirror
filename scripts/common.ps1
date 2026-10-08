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

function Get-MirrorTestDisk {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)
    # A disposable fixture only. Never open a physical drive or enroll storage.
    $directory = Join-Path $RepositoryRoot 'build'
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $path = Join-Path $directory 'block-test.img'
    $expectedHash = 'D0475F48263EA2DBCDF7A40C76BEA53F39C08977888CDFD6582427268134E318'
    if ((Test-Path -LiteralPath $path -PathType Leaf) -and
        (Get-Item -LiteralPath $path).Length -eq 1048576 -and
        (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq $expectedHash) {
        return $path
    }
    $bytes = [byte[]]::new(1048576)
    $signature = [byte[]]@(77, 73, 82, 82, 79, 82, 0, 1)
    for ($sector = 0; $sector -lt 2048; $sector++) {
        $start = $sector * 512
        for ($index = 0; $index -lt 8; $index++) {
            $bytes[$start + $index] = [byte](([uint64]$sector -shr (8 * $index)) -band 255)
            $bytes[$start + 8 + $index] = $signature[$index]
        }
        for ($index = 16; $index -lt 512; $index++) {
            $bytes[$start + $index] = [byte]((($sector * 73) -bxor ($sector -shr 8) -bxor ($index * 29) -bxor 165) -band 255)
        }
    }
    [System.IO.File]::WriteAllBytes($path, $bytes)
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $expectedHash) {
        throw 'Generated block fixture does not match its expected SHA-256.'
    }
    return $path
}
