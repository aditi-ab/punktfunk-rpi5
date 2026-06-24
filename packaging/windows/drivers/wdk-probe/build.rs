//! Emits the WDK link flags for the cdylib (the same call the gamepad drivers use). If wdk-build adds
//! `/INTEGRITYCHECK`, it shows up in the produced DLL's PE DllCharacteristics — which the CI step
//! inspects to answer the M0 self-signed-load question. Also links `IddCxStub` so the `iddcx` DDIs the
//! probe calls resolve (the M1 link gate).
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()?;
    link_iddcx_stub();
    Ok(())
}

/// Link `IddCxStub.lib`. It ships only under the SDK *version* that includes IddCx (e.g. 10.0.26100.0),
/// at `Lib\<ver>\um\<arch>\iddcx\<iddcxver>\` — a newer base SDK installed alongside (e.g. 10.0.28000.0)
/// has `um\<arch>` but no `iddcx`, so we glob for the dir that actually contains the lib rather than
/// trusting the max SDK version (the same gotcha the `wdf-umdf` oracle's build script documents). x64
/// only (the Windows host is x64-only).
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
