[CmdletBinding(DefaultParameterSetName = 'Case')]
param(
    [Parameter(ParameterSetName = 'Case', Mandatory = $true)]
    [Parameter(ParameterSetName = 'Recover')][string]$Vmx,
    [Parameter(Mandatory = $true)][string]$LaneRoot,
    [Parameter(Mandatory = $true)][string]$LockOwner,
    [Parameter(ParameterSetName = 'Case', Mandatory = $true)][ValidateSet('boot', 'suspend-resume', 'restart')][string]$Case,
    [Parameter(ParameterSetName = 'Recover', Mandatory = $true)][string]$Recover,
    [Parameter(ParameterSetName = 'Recover')][switch]$PowerOff,
    [string]$Vmrun
)

# Host acceptance cannot enable VMware operations or establish D11 completion.
# Both entries fail before invoking a tool, acquiring a lease or changing a VM.
throw 'VMware cases and VMware recovery are disabled together pending 0015d D11 worker/service separation and terminal-completion evidence. Host package recovery uses package.ps1 -Lane -Recover.'
