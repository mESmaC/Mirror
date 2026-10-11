[CmdletBinding()]
param(
    [switch]$Release
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$profile = if ($Release) { 'release' } else { 'debug' }

# VMs boot copies in build/dist/, so a later build or test run that rewrites
# build/mirror.img never changes a VM's disk (spec 0015a). Packages use the
# default kernel flavor, the one for daily use (ledger S7a).
& (Join-Path $PSScriptRoot 'build.ps1') -Release:$Release
$distDirectory = Join-Path $repositoryRoot 'build/dist'
New-Item -ItemType Directory -Path $distDirectory -Force | Out-Null
# Copy the image before the descriptor that names it. Each copy goes to a
# temporary name first, so a failed copy never leaves a partial artifact.
foreach ($name in @('mirror.img', 'mirror.vmdk')) {
    $source = Join-Path $repositoryRoot "build/$name"
    $destination = Join-Path $distDirectory $name
    $temporary = "$destination.tmp"
    try {
        Copy-Item -LiteralPath $source -Destination $temporary -Force
        Move-Item -LiteralPath $temporary -Destination $destination -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}
Write-Host "Mirror $profile default disk image packaged in ${distDirectory}: mirror.img, mirror.vmdk"
