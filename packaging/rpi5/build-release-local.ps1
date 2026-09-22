param(
    [Parameter(Mandatory = $true)]
    [string]$Tag,

    [string]$OutputDirectory = "dist-local",
    [switch]$WorkingTree
)

$ErrorActionPreference = "Stop"
$repository = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$image = "punktfunk-rpi5-release:bookworm"
$sourceOption = if ($WorkingTree) { '--working-tree' } else { '' }

docker build `
    --platform linux/arm64 `
    --file (Join-Path $PSScriptRoot "Dockerfile.release") `
    --tag $image `
    $PSScriptRoot
if ($LASTEXITCODE -ne 0) { throw 'ARM64 build environment failed' }

docker run --rm `
    --platform linux/arm64 `
    --volume "${repository}:/workspace" `
    --volume punktfunk-rpi5-cargo-git:/usr/local/cargo/git `
    --volume punktfunk-rpi5-cargo-registry:/usr/local/cargo/registry `
    --volume punktfunk-rpi5-target:/tmp/punktfunk-target `
    --workdir /workspace `
    --env CARGO_TARGET_DIR=/tmp/punktfunk-target `
    $image `
    bash -c "git config --global --add safe.directory /workspace && packaging/rpi5/build-release.sh '$Tag' '$OutputDirectory' $sourceOption"
if ($LASTEXITCODE -ne 0) { throw 'ARM64 release build failed' }
