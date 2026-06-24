//! Emits the WDK link flags for the cdylib (the same call the gamepad drivers use). If wdk-build adds
//! `/INTEGRITYCHECK`, it shows up in the produced DLL's PE DllCharacteristics — which the CI step
//! inspects to answer the M0 self-signed-load question.
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
