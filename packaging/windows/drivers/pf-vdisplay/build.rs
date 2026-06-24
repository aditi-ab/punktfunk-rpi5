//! WDK link flags for the cdylib (wdk-build) + `IddCxStub` (the driver calls IddCx DDIs via wdk-iddcx,
//! and exports `IddMinimumVersionRequired`). `/INTEGRITYCHECK` (set by wdk-build) is cleared by the CI
//! packaging step. Glob recipe matches wdk-probe/build.rs.
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()?;
    link_iddcx_stub();
    // wdk-build emits `/OPT:REF,ICF`. ICF (Identical COMDAT Folding) merges functions with identical
    // bodies into ONE address — our many identical stub IddCx callbacks (`return STATUS_SUCCESS`) collapse
    // to the same pointer (and even CRT EH handlers fold, hence the dumpbin `__CxxFrameHandler4 = DllMain`
    // alias). The working virtual-display-rs links with NO `/OPT`, and IddCxAdapterInitAsync rejects a
    // config whose distinct callbacks alias each other. Disable ICF (REF stays on); last `/OPT` wins.
    println!("cargo::rustc-cdylib-link-arg=/OPT:NOICF");
    Ok(())
}

/// Link `IddCxStub.lib`. It ships under `Lib\<sdkver>\um\<arch>\iddcx\<iddcxver>\`, and the iddcx
/// versions are NOT equivalent: the `1.0`/`1.2` stubs lack the versioned-struct-size table symbols
/// (`IddStructures`/`IddStructureCount`/`IddClientVersionHigherThanFramework`) that `size.rs` needs —
/// `1.3`+ and `1.10` have them. So pick the HIGHEST `iddcx\<X.Y>` dir that has the lib (version-aware,
/// since "1.10" < "1.2" lexically). x64 only.
fn link_iddcx_stub() {
    const ARCH: &str = "x64";
    const ROOTS: [&str; 2] = [
        r"C:\Program Files (x86)\Windows Kits\10\Lib",
        r"C:\Program Files\Windows Kits\10\Lib",
    ];
    let mut best: Option<((u32, u32), std::path::PathBuf)> = None;
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
                if !sub.path().join("IddCxStub.lib").is_file() {
                    continue;
                }
                let name = sub.file_name().to_string_lossy().into_owned();
                let mut parts = name.split('.');
                let v = (
                    parts.next().and_then(|x| x.parse().ok()).unwrap_or(0u32),
                    parts.next().and_then(|x| x.parse().ok()).unwrap_or(0u32),
                );
                if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
                    best = Some((v, sub.path()));
                }
            }
        }
    }
    let Some((_, dir)) = best else {
        panic!("IddCxStub.lib not found under any Windows Kits Lib\\<ver>\\um\\{ARCH}\\iddcx\\<iddcxver>\\");
    };
    println!("cargo:rustc-link-search={}", dir.display());
    println!("cargo:rustc-link-lib=static=IddCxStub");
}
