//! Emits the WDK link flags for the cdylib (wdk-build). STEP 0 needs only the WDF stub link + the
//! `/INTEGRITYCHECK` that the CI step clears; `IddCxStub` (+ `IddMinimumVersionRequired`) is added in
//! STEP 2 when the driver actually calls IddCx — see wdk-probe/build.rs for the glob recipe.
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
