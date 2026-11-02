//! The ghost-monitor reap: removes the not-present "punktfunk" monitor devnodes that
//! `IddCxMonitorDeparture` leaves behind. Each ghost pins a VidPN target against IddCx's
//! ~16-slot budget; once full, `IOCTL_ADD` answers `ERROR_NOT_FOUND`.
//!
//! Both entry points walk the monitor class over SetupDi in-process and remove each ghost
//! ([`is_ghost`]) with `DIF_REMOVE`. When SetupDi refuses to list the class,
//! [`reap_ghost_monitors`] (the exhausted ADD's) spawns the PowerShell script, which applies the
//! same rule; [`reap_ghost_monitors_in_process`] (the control-device open's) does not. Both are
//! best-effort and log what they found and removed.
//!
//! [`after_add`] is the exhausted ADD's side: when the driver's ADD retry loop reaps, retries
//! or gives up, by [`AddFailure`] class.
//!
//! The rules are pure and run in every host's tests; the SetupDi pass runs on Windows only.

#[cfg(target_os = "windows")]
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, SetupDiCallClassInstaller, SetupDiSetClassInstallParamsW,
    CM_DEVNODE_STATUS_FLAGS, CM_PROB, DIF_REMOVE, DI_REMOVEDEVICE_GLOBAL, GUID_DEVCLASS_MONITOR,
    SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_FRIENDLYNAME, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA,
    SP_REMOVEDEVICE_PARAMS,
};

#[cfg(target_os = "windows")]
use super::DevInfoList;

/// What one reap saw: the ghost nodes it found and how many of them it removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Reaped {
    found: u32,
    removed: u32,
}

/// `CM_Get_DevNode_Status`'s `CONFIGRET` for a node that is not present: its devnode is gone
/// while its registry keys stay. A present node answers `CR_SUCCESS`, problem state or not.
const CR_NO_SUCH_DEVNODE: u32 = 0x0D;

/// A ghost is a monitor node whose friendly name names punktfunk, in any case, and whose devnode
/// is gone. Any other answer, an unreadable name included, leaves the node alone: a live
/// session's monitor is present.
fn is_ghost(friendly_name: Option<&str>, cm_status: u32) -> bool {
    cm_status == CR_NO_SUCH_DEVNODE
        && friendly_name.is_some_and(|name| name.to_ascii_lowercase().contains("punktfunk"))
}

// The script's form of [`is_ghost`]: presence, not health (`Status -ne 'OK'` would remove a live
// monitor in a problem state); `Status -eq 'Unknown'` guards a null `Present`. A query that
// throws exits 3 before it reports, never `0 0`. Full-path pnputil; `$LASTEXITCODE=1` before
// launch so a miss cannot look like exit 0. Tokens are locale-invariant.
const REAP_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
    try { $all = @(Get-PnpDevice -Class Monitor -ErrorAction Stop) } catch { exit 3 }; \
    $g = @($all | Where-Object { -not $_.Present -and $_.Status -eq 'Unknown' -and $_.FriendlyName -match 'punktfunk' }); \
    $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); \
    $n = 0; foreach ($d in $g) { $LASTEXITCODE = 1; if (Test-Path $pnp) { & $pnp /remove-device $d.InstanceId *> $null }; if ($LASTEXITCODE -eq 0) { $n++ } }; \
    Write-Output ($g.Count.ToString() + ' ' + $n)";

/// Removes the ghost monitor nodes and returns how many went, the script standing in when SetupDi
/// cannot list them. Logs found and removed even when both are zero: a silent reap hides a
/// failed one.
#[cfg(target_os = "windows")]
pub(super) fn reap_ghost_monitors() -> u32 {
    reap_with(setupdi_reap, Some(script_reap)).map_or(0, report)
}

/// [`reap_ghost_monitors`] without the script. The control-device open runs under the manager's
/// `device` mutex inside the Welcome handshake, where a PowerShell cold start costs seconds; a
/// refused SetupDi leaves the ghosts to the next exhausted ADD.
#[cfg(target_os = "windows")]
pub(super) fn reap_ghost_monitors_in_process() -> u32 {
    reap_with(setupdi_reap, None).map_or(0, report)
}

/// The in-process pass, then `script` only when that pass could not list the class. `None`
/// when neither reported.
fn reap_with(
    in_process: fn() -> anyhow::Result<Reaped>,
    script: Option<fn() -> Option<Reaped>>,
) -> Option<Reaped> {
    match in_process() {
        Ok(reaped) => Some(reaped),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                powershell = script.is_some(),
                "pf-vdisplay: in-process ghost-monitor reap refused"
            );
            script.and_then(|run| run())
        }
    }
}

/// Every monitor node, absent ones included, through [`is_ghost`], then `DIF_REMOVE` on each
/// ghost. `Err` only when SetupDi refuses to list the class.
#[cfg(target_os = "windows")]
fn setupdi_reap() -> anyhow::Result<Reaped> {
    use anyhow::Context;
    let set = DevInfoList::of(&GUID_DEVCLASS_MONITOR, SETUP_DI_GET_CLASS_DEVS_FLAGS(0))
        .context("SetupDiGetClassDevsW(monitor class)")?;
    // Listed before any removal, so a removal cannot shift the walk.
    let ghosts: Vec<SP_DEVINFO_DATA> = set
        .devices()
        .filter(|did| {
            let name = set.text(did, SPDRP_FRIENDLYNAME);
            is_ghost(name.as_deref(), devnode_status(did.DevInst))
        })
        .collect();
    let mut removed = 0;
    for did in &ghosts {
        match remove(&set, did) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(error = %e, "pf-vdisplay: ghost monitor node not removed"),
        }
    }
    Ok(Reaped {
        found: ghosts.len() as u32,
        removed,
    })
}

/// `CM_Get_DevNode_Status`'s raw `CONFIGRET` for `devinst`.
#[cfg(target_os = "windows")]
fn devnode_status(devinst: u32) -> u32 {
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    // SAFETY: both out-params are live locals the call writes and only it touches; `devinst` is
    // a plain handle value, and one whose node is gone is answered with a CONFIGRET.
    unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) }.0
}

/// Uninstalls `did`'s node from every hardware profile: `DIF_REMOVE` with
/// `DI_REMOVEDEVICE_GLOBAL`.
#[cfg(target_os = "windows")]
fn remove(set: &DevInfoList, did: &SP_DEVINFO_DATA) -> windows::core::Result<()> {
    let params = SP_REMOVEDEVICE_PARAMS {
        ClassInstallHeader: SP_CLASSINSTALL_HEADER {
            cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
            InstallFunction: DIF_REMOVE,
        },
        Scope: DI_REMOVEDEVICE_GLOBAL,
        HwProfile: 0,
    };
    // SAFETY: `set` is live and `did` one of its elements. The header pointer is cast from the
    // whole `params` (`repr(C)`, header first), never taken from the header field, so it carries
    // provenance over the `size_of::<SP_REMOVEDEVICE_PARAMS>()` bytes SetupAPI reads; SetupAPI
    // copies them into the element before it returns, and `params` outlives the call.
    unsafe {
        SetupDiSetClassInstallParamsW(
            set.0,
            Some(did),
            Some((&raw const params).cast()),
            size_of::<SP_REMOVEDEVICE_PARAMS>() as u32,
        )
    }?;
    // SAFETY: `set` is live and `did` one of its elements; the class installer reads the remove
    // params SetupAPI stored on the element above.
    unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, Some(did)) }
}

/// [`REAP_PS`] under PowerShell. `None`, logged, when it did not run or died before reporting.
#[cfg(target_os = "windows")]
fn script_reap() -> Option<Reaped> {
    // Full-path powershell: LocalSystem PATH need not include System32.
    let ps = pf_paths::system32(pf_paths::POWERSHELL);
    // Bounded: the exhausted-ADD retry runs it inside a manager transition, so a wedged
    // Get-PnpDevice would stall every slot change. `output_within` kills the whole tree on the
    // deadline.
    let mut cmd = std::process::Command::new(&ps);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        REAP_PS,
    ]);
    let out = match crate::proc::output_within(&mut cmd, std::time::Duration::from_secs(30)) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: ghost-monitor reap could not spawn powershell");
            return None;
        }
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let Some((found, removed)) = parse_reap_output(&raw) else {
        tracing::warn!(
            output = %raw.trim(),
            "pf-vdisplay: ghost-monitor reap died before reporting — ghost nodes (if any) still pin IddCx monitor slots"
        );
        return None;
    };
    Some(Reaped { found, removed })
}

/// Logs what a reap found and removed; returns the removed count.
#[cfg(target_os = "windows")]
fn report(Reaped { found, removed }: Reaped) -> u32 {
    if found == 0 {
        tracing::info!("pf-vdisplay: no ghost (not-present) virtual-monitor nodes to reap");
    } else if removed < found {
        tracing::warn!(
            found,
            removed,
            "pf-vdisplay: ghost-monitor reap left ghost nodes behind — the leftovers keep pinning IddCx monitor slots toward the 0x80070490 wedge"
        );
    } else {
        tracing::warn!(
            reaped = removed,
            "pf-vdisplay: reaped ghost (not-present) virtual-monitor nodes — IddCx slot-exhaustion prevention"
        );
    }
    removed
}

/// Why an `IOCTL_ADD` failed, from the HRESULT its error carries (the classes are documented on
/// `control::AddReply`). An older driver answers every failure with `ERROR_NOT_FOUND`, so it
/// keeps the exhausted path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddFailure {
    /// `ERROR_NOT_READY`: the adapter is still starting, or a removal raced the create.
    NotReady,
    /// `ERROR_NOT_FOUND`: IddCx refused the monitor, usually ghost monitors holding every slot.
    Exhausted,
    /// Any other HRESULT: the driver or IddCx refused this ADD outright.
    Refused(u32),
}

/// ADDs after the first, one cadence step apart: VidPN slot reclaim and adapter init are async.
const ADD_RETRIES: u32 = 5;

/// What the ADD retry loop does after a failed ADD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AfterAdd {
    /// Reap the ghost monitors first.
    pub reap: bool,
    /// Send the ADD again after one cadence step; `false` returns the error.
    pub retry: bool,
}

/// The step after the `attempt`-th ADD (0 = the first) failed with `failure`; `reaped` once a
/// reap ran. A refused first ADD fails at once; a not-ready or exhausted one earns
/// [`ADD_RETRIES`] more, whatever those answer. The first exhausted answer reaps, on whichever
/// attempt it comes, and none after it: the slots a reap frees return asynchronously.
pub(super) fn after_add(attempt: u32, failure: AddFailure, reaped: bool) -> AfterAdd {
    let retry = if attempt == 0 {
        !matches!(failure, AddFailure::Refused(_))
    } else {
        attempt < ADD_RETRIES
    };
    AfterAdd {
        reap: failure == AddFailure::Exhausted && !reaped,
        retry,
    }
}

/// Parse `"<found> <removed>"` from [`REAP_PS`]. `None` = the script died before reporting
/// (callers treat that as removed-nothing, loudly).
fn parse_reap_output(out: &str) -> Option<(u32, u32)> {
    let mut it = out.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(found)), Some(Ok(removed))) => Some((found, removed)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ghost needs both: a node that is gone and a name that is ours. A present node, one in
    /// a problem state, an unknown answer or a stranger's name stays.
    #[test]
    fn only_an_absent_punktfunk_node_is_a_ghost() {
        const CR_SUCCESS: u32 = 0;
        const CR_INVALID_DEVNODE: u32 = 0x05;
        let ours = Some("generic monitor (punktfunk)");
        let rows = [
            (ours, CR_NO_SUCH_DEVNODE, true),
            (
                Some("Generic Monitor (PunktFunk)"),
                CR_NO_SUCH_DEVNODE,
                true,
            ),
            (ours, CR_SUCCESS, false),
            (ours, CR_INVALID_DEVNODE, false),
            (Some("generic pnp monitor"), CR_NO_SUCH_DEVNODE, false),
            (None, CR_NO_SUCH_DEVNODE, false),
        ];
        for (name, status, ghost) in rows {
            assert_eq!(is_ghost(name, status), ghost, "{name:?} status {status:#x}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_ghost_status_is_the_configret_for_a_missing_devnode() {
        use windows::Win32::Devices::DeviceAndDriverInstallation as setup;
        assert_eq!(CR_NO_SUCH_DEVNODE, setup::CR_NO_SUCH_DEVNODE.0);
        assert_ne!(CR_NO_SUCH_DEVNODE, setup::CR_SUCCESS.0);
    }

    fn clean() -> anyhow::Result<Reaped> {
        Ok(Reaped::default())
    }

    fn one_ghost() -> anyhow::Result<Reaped> {
        Ok(Reaped {
            found: 1,
            removed: 1,
        })
    }

    fn refused() -> anyhow::Result<Reaped> {
        anyhow::bail!("SetupDiGetClassDevsW(monitor class)")
    }

    fn no_script() -> Option<Reaped> {
        panic!("PowerShell ran although SetupDi listed the class")
    }

    fn script() -> Option<Reaped> {
        Some(Reaped {
            found: 2,
            removed: 2,
        })
    }

    /// PowerShell runs only when SetupDi refuses to list the class: a clean box and a box with
    /// a ghost are both answered in-process.
    #[test]
    fn the_script_runs_only_when_setupdi_refuses() {
        assert_eq!(reap_with(clean, Some(no_script)), Some(Reaped::default()));
        assert_eq!(reap_with(one_ghost, Some(no_script)), one_ghost().ok());
        assert_eq!(reap_with(refused, Some(script)), script());
    }

    /// The control-device open passes no script: a refused SetupDi reports nothing and spawns
    /// nothing inside the Welcome handshake.
    #[test]
    fn the_open_never_runs_the_script() {
        assert_eq!(reap_with(clean, None), Some(Reaped::default()));
        assert_eq!(reap_with(refused, None), None);
    }

    /// A monitor node is a ghost only when it is absent, PnP reads its status `Unknown` and its
    /// name is ours. Any other node may carry a live session.
    #[test]
    fn the_script_selects_only_absent_punktfunk_monitors() {
        assert!(
            REAP_PS.contains("Get-PnpDevice -Class Monitor"),
            "{REAP_PS}"
        );
        let filter = REAP_PS
            .split_once("Where-Object {")
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(filter, _)| filter)
            .expect("the ghost filter");
        for clause in [
            "-not $_.Present",
            "$_.Status -eq 'Unknown'",
            "$_.FriendlyName -match 'punktfunk'",
        ] {
            assert!(
                filter.contains(clause),
                "{clause:?} missing from {filter:?}"
            );
        }
    }

    /// A query that throws exits before it reports, so it decodes as a reap that died, never as
    /// the clean box's `0 0`.
    #[test]
    fn a_failed_query_is_not_a_clean_box() {
        let (query, rest) = REAP_PS.split_once("catch").expect("the query's catch");
        assert!(
            query.contains("try { $all = @(Get-PnpDevice -Class Monitor -ErrorAction Stop) }"),
            "{query}"
        );
        assert!(rest.trim_start().starts_with("{ exit 3 }"), "{rest}");
    }

    /// Feeds `answers` (the failed ADDs, in order) through [`after_add`] as the retry loop does.
    /// Returns the attempts that reaped, and the attempt it gave up on (`None`: the next ADD
    /// went out).
    fn play(answers: &[AddFailure]) -> (Vec<u32>, Option<u32>) {
        let mut reaps = Vec::new();
        for (attempt, &failure) in (0u32..).zip(answers) {
            let step = after_add(attempt, failure, !reaps.is_empty());
            if step.reap {
                reaps.push(attempt);
            }
            if !step.retry {
                return (reaps, Some(attempt));
            }
        }
        (reaps, None)
    }

    /// The first exhausted answer reaps, once, on whichever attempt it comes; on the last one the
    /// reap still frees the slots for the next connect. A refused answer never reaps.
    #[test]
    fn the_add_retry_reaps_by_its_table() {
        use AddFailure::{Exhausted, NotReady, Refused};
        const REFUSED: AddFailure = Refused(0x8007_001F);
        let rows: [(&[AddFailure], &[u32], Option<u32>); 7] = [
            (&[Exhausted], &[0], None),
            (&[NotReady, NotReady, Exhausted], &[2], None),
            (&[Exhausted, Exhausted, Exhausted], &[0], None),
            (&[REFUSED], &[], Some(0)),
            (&[NotReady, REFUSED, Exhausted], &[2], None),
            (&[NotReady; 6], &[], Some(5)),
            (
                &[NotReady, NotReady, NotReady, NotReady, NotReady, Exhausted],
                &[5],
                Some(5),
            ),
        ];
        for (answers, reaps, gave_up) in rows {
            assert_eq!(play(answers), (reaps.to_vec(), gave_up), "{answers:?}");
        }
    }

    /// Found and removed travel separately so a leftover ghost is loud. A single number or
    /// a script that died before reporting must not decode as a reap.
    #[test]
    fn reap_output_decodes_found_and_removed() {
        assert_eq!(parse_reap_output("3 3\r\n"), Some((3, 3)));
        assert_eq!(
            parse_reap_output("4 0"),
            Some((4, 0)),
            "pnputil unlaunchable"
        );
        assert_eq!(parse_reap_output("0 0"), Some((0, 0)), "clean box");
        for dead in ["5", "", "   ", "garbage", "OK"] {
            assert_eq!(
                parse_reap_output(dead),
                None,
                "{dead:?} is not a reap report"
            );
        }
    }
}
