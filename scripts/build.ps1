[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Smoke,
    [switch]$Trace,
    [switch]$AgentDemo
)

. (Join-Path $PSScriptRoot 'common.ps1')
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$profile = if ($Release) { 'release' } else { 'debug' }

Push-Location -LiteralPath $repositoryRoot
try {
    Assert-MirrorToolchain
    # Package-specific linker flags without a host build-script/linker dependency.
    # Encoded flags override the kernel defaults and preserve paths with spaces.
    $previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
    $previousReleaseLto = $env:CARGO_PROFILE_RELEASE_LTO
    try {
        # Native programs live in the low half, so use the small code model. Rust's
        # prebuilt core uses the kernel model; do not merge their LLVM bitcode.
        # The kernel build below retains the workspace's release LTO setting.
        $env:CARGO_PROFILE_RELEASE_LTO = 'false'
        foreach ($nativePackage in @('init', 'desktop', 'agent', 'app')) {
            $nativeLinker = Join-Path $repositoryRoot "userspace/$nativePackage/linker.ld"
            $env:CARGO_ENCODED_RUSTFLAGS = @('-C', 'relocation-model=static', '-C', 'code-model=small', '-C', "link-arg=-T$nativeLinker", '-C', 'link-arg=-zmax-page-size=0x1000') -join [char]31
            $nativeArguments = @('build', '--locked', '-p', "mirror-$nativePackage", '--target', 'x86_64-unknown-none')
            if ($Release) { $nativeArguments += '--release' }
            & cargo @nativeArguments
            if ($LASTEXITCODE -ne 0) { throw "$nativePackage build failed with exit code $LASTEXITCODE." }
        }
    }
    finally {
        $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
        $env:CARGO_PROFILE_RELEASE_LTO = $previousReleaseLto
    }
    $cargoArguments = @('build', '--locked', '-p', 'mirror-kernel', '--target', 'x86_64-unknown-none')
    if ($Release) { $cargoArguments += '--release' }
    # The three kernel flavors of ledger S7a (spec 0010): default (no features),
    # trace (test-trace) and smoke (boot-smoke, which implies test-trace).
    if ($Smoke) { $cargoArguments += @('--features', 'boot-smoke') }
    if ($Trace) { $cargoArguments += @('--features', 'test-trace') }
    if ($AgentDemo) { $cargoArguments += @('--features', 'agent-demo') }
    & cargo @cargoArguments
    if ($LASTEXITCODE -ne 0) { throw "Kernel build failed with exit code $LASTEXITCODE." }

    $kernelPath = Join-Path $repositoryRoot "target/x86_64-unknown-none/$profile/mirror-kernel"
    if (-not (Test-Path -LiteralPath $kernelPath -PathType Leaf)) {
        throw "Cargo did not produce the expected kernel: $kernelPath"
    }

    $limineVersion = 'v12.9.3'
    $limineHash = '1bdbbcda650c5dba9b1eca5833d598137bf11bd6289bc89b6bcc4f8f112fa676'
    $limineDirectory = Join-Path $repositoryRoot ".tools/limine/$limineVersion"
    $archivePath = Join-Path $limineDirectory 'limine-binary.zip'
    New-Item -ItemType Directory -Path $limineDirectory -Force | Out-Null
    if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
        $downloadPath = "$archivePath.download"
        $downloadUrl = "https://github.com/Limine-Bootloader/Limine/releases/download/$limineVersion/limine-binary.zip"
        Write-Host "Downloading Limine $limineVersion..."
        Invoke-WebRequest -Uri $downloadUrl -OutFile $downloadPath -UseBasicParsing
        $downloadHash = (Get-FileHash -LiteralPath $downloadPath -Algorithm SHA256).Hash
        if ($downloadHash -ne $limineHash) {
            throw "Limine download checksum mismatch. Expected $limineHash; got $downloadHash."
        }
        Move-Item -LiteralPath $downloadPath -Destination $archivePath -Force
    }

    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if ($actualHash -ne $limineHash) {
        throw "Cached Limine archive checksum mismatch: $archivePath"
    }
    # Re-extract the verified archive so a modified cached EFI file is not trusted.
    # Extract into this checkout's build folder: .tools is shared across
    # worktrees, and two builds at once must not write the same files.
    $extractedDirectory = Join-Path $repositoryRoot 'build/limine-extracted'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractedDirectory -Force
    $bootFiles = @(Get-ChildItem -LiteralPath $extractedDirectory -Recurse -File -Filter 'BOOTX64.EFI')
    if ($bootFiles.Count -ne 1) {
        throw "Expected exactly one BOOTX64.EFI in the verified Limine archive; found $($bootFiles.Count)."
    }

    $espDirectory = Join-Path $repositoryRoot 'build/esp'
    $imagePath = Join-Path $repositoryRoot 'build/mirror.img'
    $vmdkPath = Join-Path $repositoryRoot 'build/mirror.vmdk'
    # The disk image holds exactly the staged tree (spec 0015a), so restage it
    # from empty. Drop the previous image first: if this build fails, no image
    # of an older tree is left beside a partly restaged one.
    foreach ($artifact in @($imagePath, $vmdkPath)) {
        if (Test-Path -LiteralPath $artifact) { Remove-Item -LiteralPath $artifact -Force }
    }
    if (Test-Path -LiteralPath $espDirectory) {
        # Refuse links, so the recursive removal never reaches outside the tree.
        @(Get-Item -LiteralPath $espDirectory -Force) + @(Get-ChildItem -LiteralPath $espDirectory -Recurse -Force) | ForEach-Object {
            if ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing to clear $espDirectory because $($_.FullName) is a link."
            }
        }
        Remove-Item -LiteralPath $espDirectory -Recurse -Force
    }
    $efiDirectory = Join-Path $espDirectory 'EFI/BOOT'
    New-Item -ItemType Directory -Path $efiDirectory -Force | Out-Null
    Copy-Item -LiteralPath $bootFiles[0].FullName -Destination (Join-Path $efiDirectory 'BOOTX64.EFI') -Force
    Copy-Item -LiteralPath $kernelPath -Destination (Join-Path $espDirectory 'kernel.elf') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "target/x86_64-unknown-none/$profile/mirror-init") -Destination (Join-Path $espDirectory 'init.elf') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "target/x86_64-unknown-none/$profile/mirror-desktop") -Destination (Join-Path $espDirectory 'desktop.elf') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "target/x86_64-unknown-none/$profile/mirror-agent") -Destination (Join-Path $espDirectory 'agent.elf') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "target/x86_64-unknown-none/$profile/mirror-app") -Destination (Join-Path $espDirectory 'app.elf') -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'boot/limine.conf') -Destination (Join-Path $espDirectory 'limine.conf') -Force
    # The image redistributes Limine's binaries (BSD-2-Clause), so it carries
    # the license files that sit beside BOOTX64.EFI in the verified archive.
    $licenseDirectory = Join-Path $espDirectory 'LICENSES/limine'
    New-Item -ItemType Directory -Path $licenseDirectory -Force | Out-Null
    foreach ($licenseFile in @('LICENSE', '3RDPARTY.md')) {
        $licenseSource = Join-Path $bootFiles[0].DirectoryName $licenseFile
        if (-not (Test-Path -LiteralPath $licenseSource -PathType Leaf)) {
            throw "The verified Limine archive lacks $licenseFile beside BOOTX64.EFI."
        }
        Copy-Item -LiteralPath $licenseSource -Destination (Join-Path $licenseDirectory $licenseFile) -Force
    }
    $flavor = if ($Smoke) { 'smoke' } elseif ($Trace) { 'trace' } else { 'default' }
    Write-Host "Mirror $profile $flavor kernel and native programs staged in $espDirectory (agent-demo=$AgentDemo)"

    # tools/mirror-image writes the GPT disk image and its VMDK descriptor from
    # the staged tree, then reads the image back against the tree (spec 0015a).
    # It is a host program in its own workspace, so this step needs the MSVC
    # linker and Windows SDK (0015-D4).
    & cargo build --locked --release --target x86_64-pc-windows-msvc --manifest-path (Join-Path $repositoryRoot 'tools/mirror-image/Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "mirror-image build failed with exit code $LASTEXITCODE." }
    $imageTool = Join-Path $repositoryRoot 'tools/mirror-image/target/x86_64-pc-windows-msvc/release/mirror-image.exe'
    if (-not (Test-Path -LiteralPath $imageTool -PathType Leaf)) {
        throw "Cargo did not produce the expected image tool: $imageTool"
    }
    & $imageTool disk --esp $espDirectory --out $imagePath --vmdk $vmdkPath
    if ($LASTEXITCODE -ne 0) { throw "mirror-image disk failed with exit code $LASTEXITCODE." }
    & $imageTool verify --esp $espDirectory $imagePath
    if ($LASTEXITCODE -ne 0) {
        $verifyExitCode = $LASTEXITCODE
        Remove-Item -LiteralPath $imagePath, $vmdkPath -Force
        throw "mirror-image verify failed with exit code $verifyExitCode; the unverified image was removed."
    }
    Write-Host "Mirror $profile $flavor disk image written and verified: $imagePath, $vmdkPath"
}
finally {
    Pop-Location
}
