//! WDK link flags for the cdylib (wdk-build) + `IddCxStub` (the driver calls IddCx DDIs via wdk-iddcx,
//! and exports `IddMinimumVersionRequired`). `/INTEGRITYCHECK` (set by wdk-build) is cleared by the CI
//! packaging step. Glob recipe matches wdk-probe/build.rs.
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()?;
    link_iddcx_stub();
    Ok(())
}

/// Link `IddCxStub.lib`. It ships only under the SDK *version* that includes IddCx, at
/// `Lib\<ver>\um\<arch>\iddcx\<iddcxver>\` — a newer base SDK alongside it lacks the `iddcx` subdir, so
/// glob for the dir that actually contains the lib rather than trusting the max SDK version. x64 only.
fn link_iddcx_stub() {
    const ARCH: &str = "x64";
    const ROOTS: [&str; 2] = [
        r"C:\Program Files (x86)\Windows Kits\10\Lib",
        r"C:\Program Files\Windows Kits\10\Lib",
    ];
    for root in ROOTS {
        let Ok(versions) = std::fs::read_dir(root) else {
            continue;
        };
        for ver in versions.flatten() {
            let iddcx = ver.path().join("um").join(ARCH).join("iddcx");
            let Ok(subdirs) = std::fs::read_dir(&iddcx) else {
                continue;
            };
            for sub in subdirs.flatten() {
                if sub.path().join("IddCxStub.lib").is_file() {
                    println!("cargo:rustc-link-search={}", sub.path().display());
                    println!("cargo:rustc-link-lib=static=IddCxStub");
                    return;
                }
            }
        }
    }
    panic!("IddCxStub.lib not found under any Windows Kits Lib\\<ver>\\um\\{ARCH}\\iddcx\\<iddcxver>\\");
}
