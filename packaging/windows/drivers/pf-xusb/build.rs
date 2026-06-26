//! Build script for the `pf-xusb` UMDF driver — provides Cargo the WDK linker flags.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
