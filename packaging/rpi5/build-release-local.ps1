param(
    [Parameter(Mandatory = $true)]
    [string]$Tag,

    [string]$OutputDirectory = "dist-local"
)

$ErrorActionPreference = "Stop"
$repository = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$image = "punktfunk-rpi5-release:bookworm"

docker build `
    --platform linux/arm64 `
    --file (Join-Path $PSScriptRoot "Dockerfile.release") `
    --tag $image `
    $PSScriptRoot

docker run --rm `
    --platform linux/arm64 `
    --volume "${repository}:/workspace" `
    --workdir /workspace `
    --env CARGO_TARGET_DIR=/tmp/punktfunk-target `
    $image `
    bash -c "git config --global --add safe.directory /workspace && packaging/rpi5/build-release.sh '$Tag' '$OutputDirectory'"
