[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Load only the actual assertion function, never the runner's boot operations.
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'run.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'The smoke runner has parse errors.' }
$functions = @($ast.FindAll({ param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Assert-MirrorFpuMarkers'
}, $false))
if ($functions.Count -ne 1) { throw 'Expected one FPU assertion function.' }
. ([scriptblock]::Create($functions[0].Extent.Text))

$checks = 0
function Assert-Rejected {
    param([string]$Serial, [bool]$Extended, [int]$InitRuns = 2)
    $rejected = $false
    try { Assert-MirrorFpuMarkers -Serial $Serial -Extended $Extended -InitRuns $InitRuns }
    catch { $rejected = $true }
    if (-not $rejected) { throw 'A corrupt FPU fixture was accepted.' }
    $script:checks++
}
$fallback = 'MIRROR_FPU_READY format=fxsave64 bytes=512 xmm=16 eager=1 avx=0 avx2=0 cpuid_avx=0 osxsave=0 mask=0'
$enabled = 'MIRROR_FPU_READY format=xsave64 bytes=832 xmm=16 eager=1 avx=1 avx2=1 cpuid_avx=1 osxsave=1 mask=7'
$avx = 'user: Mirror init: AVX checks passed\n'
$avx2 = 'user: Mirror init: AVX2 checks passed\n'
$serial = @($enabled, $avx, $avx2, $avx, $avx2) -join "`n"
Assert-MirrorFpuMarkers -Serial $fallback -Extended $false -InitRuns 2
Assert-MirrorFpuMarkers -Serial $serial -Extended $true -InitRuns 2
Assert-MirrorFpuMarkers -Serial ($serial.Replace("`n", "`r`n")) -Extended $true -InitRuns 2
$checks += 3
foreach ($corrupt in @(
    $serial.Replace('mask=7', 'mask=3'),
    $serial.Replace('bytes=832', 'bytes=512'),
    $serial.Replace('osxsave=1', 'osxsave=0'),
    $serial.Replace('avx2=1', 'avx2=0'),
    $serial.Replace('format=xsave64', 'format=fxsave64'),
    $serial.Replace('eager=1', 'eager=1 extra=1'),
    $serial.Replace($avx2, ''),
    $serial.Replace($avx, ''),
    $serial.Replace($avx, ($avx + ' extra=1')),
    ($serial + "`n" + $enabled),
    ($serial + "`n" + $avx),
    ($serial + "`n" + $avx2),
    ($serial.Replace($enabled, 'missing'))
)) { Assert-Rejected -Serial $corrupt -Extended $true }
Assert-Rejected -Serial $serial -Extended $false
Assert-Rejected -Serial $fallback -Extended $true
Assert-Rejected -Serial ($fallback + "`n" + $avx) -Extended $false
Assert-Rejected -Serial ($fallback + "`n" + $avx2) -Extended $false
Assert-Rejected -Serial ($fallback + "`n" + $fallback) -Extended $false
$smpFunctions = @($ast.FindAll({ param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Assert-MirrorSmpUser'
}, $false))
if ($smpFunctions.Count -ne 1) { throw 'Expected one SMP assertion function.' }
. ([scriptblock]::Create($smpFunctions[0].Extent.Text))
function Assert-SmpRejected {
    param([string]$Serial, [int]$Cpus = 2, [bool]$Extended = $true)
    $rejected = $false
    try { Assert-MirrorSmpUser -Serial $Serial -UserspaceCpus $Cpus -Extended $Extended }
    catch { $rejected = $true }
    if (-not $rejected) { throw 'A corrupt migration fixture was accepted.' }
    $script:checks++
}
$smp = @('MIRROR_SCHED_BOUNDS_OK',
    'MIRROR_SMP_USER_OK cpus=2 threads=16 ran_on=2 cross_wakes=1 flushes=1 stale=0',
    'MIRROR_SMP_XSTATE_OK cpus=2 migrations=2 avx=1',
    'MIRROR_KVA_DEGRADED') -join "`n"
$single = $smp.Replace('cpus=2', 'cpus=1').Replace('ran_on=2', 'ran_on=1').Replace('cross_wakes=1', 'cross_wakes=0').Replace('flushes=1', 'flushes=0').Replace('migrations=2', 'migrations=0').Replace('avx=1', 'avx=0')
Assert-MirrorSmpUser -Serial $smp -UserspaceCpus 2 -Extended $true
Assert-MirrorSmpUser -Serial $single -UserspaceCpus 1 -Extended $false
Assert-MirrorSmpUser -Serial ($smp.Replace("`n", "`r`n")) -UserspaceCpus 2 -Extended $true
$checks += 3
foreach ($corrupt in @(
    $smp.Replace('migrations=2', 'migrations=0'),
    $smp.Replace('migrations=2', 'migrations=1'),
    $smp.Replace('avx=1', 'avx=0'),
    $smp.Replace('avx=1', 'avx=1 extra=1'),
    $smp.Replace('MIRROR_SMP_XSTATE_OK cpus=2 migrations=2 avx=1', ''),
    ($smp + "`nMIRROR_SMP_XSTATE_OK cpus=2 migrations=2 avx=1"),
    $smp.Replace('MIRROR_SCHED_BOUNDS_OK', 'MIRROR_SMP_XSTATE_OK cpus=2 migrations=2 avx=1'),
    $smp.Replace('MIRROR_KVA_DEGRADED', 'missing')
)) { Assert-SmpRejected -Serial $corrupt }
Assert-SmpRejected -Serial ($single.Replace('migrations=0', 'migrations=2')) -Cpus 1 -Extended $false
Write-Host "All $checks FPU host marker fixtures passed."
