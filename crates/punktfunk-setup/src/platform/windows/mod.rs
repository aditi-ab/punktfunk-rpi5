//! Windows facts: the read-only probe of a Windows box (WP1.1).
//!
//! Same charter as `facts.rs`, different machine: everything later stages may know about a
//! Windows host, as one serde'd struct so `--facts` and `--demo` stay honest. Registry reads
//! go through `reg.exe` via `CommandRunner` — value names and `REG_*` type tokens are
//! locale-invariant, unlike anything PowerShell prints — and the two questions no process can
//! answer (NLA network categories, a port already bound) sit behind `NetProbe`, whose system
//! impl is the only `cfg(windows)` code here. Everything else compiles and tests on any OS,
//! which is what puts the Windows plans in the Linux and macOS lanes.
//!
//! Design of record: `design/installer-v2-windows.md` §3 (facts), D11 (coexistence), D12
//! (network step) in the planning repo.

pub mod args;
pub mod choices;
pub mod demo;
pub mod exec;
pub mod plan;
pub mod report;
pub mod screen;
pub mod silent;
pub mod sys;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::seam::{BasePaths, CommandRunner, Env};

/// The host installer's ARP key. The `_is1` suffix is Inno's, kept forever: winget's
/// `ProductCode` and every fielded install track this exact key name.
pub const HOST_ARP_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{7C9E6A52-1F4B-4E8D-A3C7-2B5D8F1E0A93}_is1";

/// GameStream hosts that own the ports punktfunk would want. Service names, verbatim from the
/// shipped installer's probe list.
pub const COMPETING_SERVICES: [&str; 5] = [
    "SunshineService",
    "ApolloService",
    "VibeshineService",
    "VibepolloService",
    "LuminalShineService",
];

/// The management API's default port — and Sunshine's web UI port, which is why D11 exists.
pub const MGMT_PORT: u16 = 47990;

/// The uninstaller's file name: docs-site promises `{app}\unins000.exe` (D6), so the pack
/// step emits the wizard, payload-less, under it — and running under it means teardown only.
pub const UNINSTALLER_EXE: &str = "unins000.exe";

/// The real box's paths: `config` is `%ProgramData%`, so `host_env()` is the file the
/// service reads. The Linux-shaped fields point into the same tree and nothing reads them.
pub fn base_paths(env: &Env) -> BasePaths {
    let data = PathBuf::from(env.get("ProgramData").unwrap_or(r"C:\ProgramData"));
    BasePaths {
        os_release: data.join("os-release"),
        etc_root: data.clone(),
        sys: data.clone(),
        run: data.clone(),
        config: data,
        home: PathBuf::from(env.get("USERPROFILE").unwrap_or(r"C:\Users\Default")),
    }
}

/// The D6 mode switch: the same wizard exe, launched under the uninstaller's name.
pub fn launched_as_uninstaller(exe: &std::path::Path) -> bool {
    // Both separators by hand: on the unix golden lanes a backslash path is ONE component.
    exe.to_str()
        .and_then(|s| s.rsplit(['\\', '/']).next())
        .is_some_and(|n| n.eq_ignore_ascii_case(UNINSTALLER_EXE))
}

/// Where D11 moves the management API when a competitor owns [`MGMT_PORT`].
pub const MGMT_PORT_MOVED: u16 = 47991;

/// An NLA network category. `Domain` is immutable through the API; the D12 step never offers
/// to change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetCategory {
    Public,
    Private,
    Domain,
}

/// One connected network, as NLA sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetProfile {
    pub name: String,
    pub category: NetCategory,
}

/// The two probes that have no process to shell: NLA categories and a bound port.
///
/// The system impl answers from COM and a bind attempt; tests and `--demo` inject the box
/// they want, the same bargain as `CommandRunner`.
pub trait NetProbe {
    fn networks(&self) -> Vec<NetProfile>;
    fn port_in_use(&self, port: u16) -> bool;
    /// D12's consented fix. Per-network; `false` = the API refused (e.g. a domain network).
    fn make_private(&self, network: &str) -> bool;
}

/// A `NetProbe` that answers from data and touches nothing.
#[derive(Debug, Default)]
pub struct FakeNet {
    pub networks: Vec<NetProfile>,
    pub ports_in_use: Vec<u16>,
    /// Every network a test run asked to flip — what the D12 tests assert against.
    pub made_private: std::cell::RefCell<Vec<String>>,
}

impl NetProbe for FakeNet {
    fn networks(&self) -> Vec<NetProfile> {
        self.networks.clone()
    }

    fn port_in_use(&self, port: u16) -> bool {
        self.ports_in_use.contains(&port)
    }

    fn make_private(&self, network: &str) -> bool {
        self.made_private.borrow_mut().push(network.to_string());
        true
    }
}

/// A scheduled task's pre-install state — captured so upgrade stop/restore can put back
/// exactly what was there, including on an aborted run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Absent,
    Enabled,
    Disabled,
}

/// What the ARP key says about an existing install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinInstall {
    pub version: Option<String>,
    /// `InstallLocation` — an upgrade follows it instead of the default dir.
    pub location: Option<String>,
}

/// Everything the Windows plan is allowed to know about this machine.
///
/// On Windows, `BasePaths.config` is `%ProgramData%` — `host_env()` then lands on
/// `%ProgramData%\punktfunk\host.env`, the file the service reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinFacts {
    pub os_build: u32,
    /// `x64` / `arm64`, lowercased raw value for anything else.
    pub arch: String,
    /// `None` = no ARP key = a fresh install.
    pub installed: Option<WinInstall>,
    pub host_env_present: bool,
    /// Skips the password page and the password step — the web install already happened.
    pub web_password_present: bool,
    /// An operator set `PUNKTFUNK_MGMT_BIND` themselves; D11 must never rewrite it.
    pub mgmt_bind_set: bool,
    /// Competing GameStream host services with `Start ≤ 2`. Disabled/manual don't count.
    pub competing_hosts: Vec<String>,
    /// Something already answers on [`MGMT_PORT`] — catches a competitor running without a
    /// service entry.
    pub mgmt_port_in_use: bool,
    pub networks: Vec<NetProfile>,
    pub steam_audio_drivers: bool,
    pub tray_autostart: bool,
    pub vulkan_layer_registered: bool,
    pub web_task: TaskState,
    pub scripting_task: TaskState,
    /// Inno's `unins000.dat` sits in the install dir: the box was installed by the `.iss`, and
    /// the first upgrade over it retires that data (D6 — never run Inno's uninstaller).
    pub inno_uninstaller: bool,
    /// The client artifact's own ARP key (per user, HKCU). `installed` above is the host's.
    pub client_installed: Option<WinInstall>,
}

impl WinFacts {
    pub fn probe(
        paths: &BasePaths,
        run: &dyn CommandRunner,
        env: &Env,
        net: &dyn NetProbe,
    ) -> WinFacts {
        let host_env_text = paths.read(&paths.host_env());
        let installed = arp_install(run);
        let inno_uninstaller = installed
            .as_ref()
            .and_then(|i| i.location.as_deref())
            .is_some_and(|dir| std::path::Path::new(dir).join("unins000.dat").is_file());
        WinFacts {
            os_build: reg_value(
                run,
                r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
                "CurrentBuildNumber",
            )
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
            arch: arch(env),
            installed,
            host_env_present: host_env_text.is_some(),
            web_password_present: paths.config.join("punktfunk/web-password").is_file(),
            mgmt_bind_set: mgmt_bind_set(host_env_text.as_deref().unwrap_or_default()),
            competing_hosts: competing_hosts(run),
            mgmt_port_in_use: net.port_in_use(MGMT_PORT),
            networks: net.networks(),
            steam_audio_drivers: steam_audio_drivers(env),
            tray_autostart: reg_value(
                run,
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
                "PunktfunkTray",
            )
            .is_some(),
            vulkan_layer_registered: reg_query(run, r"HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers")
                .is_some_and(|t| t.contains("pf_vkhdr_layer.json")),
            web_task: task_state(run, "PunktfunkWeb"),
            scripting_task: task_state(run, "PunktfunkScripting"),
            inno_uninstaller,
            client_installed: arp_install_at(run, plan::CLIENT_ARP_KEY),
        }
    }

    /// The install this artifact upgrades or removes: the host's under HKLM, the client's
    /// under HKCU — two products, two keys (D1).
    pub fn installed_for(&self, artifact: plan::Artifact) -> Option<&WinInstall> {
        match artifact {
            plan::Artifact::Host => self.installed.as_ref(),
            plan::Artifact::Client => self.client_installed.as_ref(),
        }
    }

    /// D11: a competitor is live — by service or by the port answering — and the operator has
    /// not already moved the management API themselves.
    pub fn needs_coexistence(&self) -> bool {
        (!self.competing_hosts.is_empty() || self.mgmt_port_in_use) && !self.mgmt_bind_set
    }

    /// D12: the networks whose Public category makes the default firewall rules inert.
    pub fn public_networks(&self) -> Vec<&NetProfile> {
        self.networks
            .iter()
            .filter(|n| n.category == NetCategory::Public)
            .collect()
    }
}

/// `PROCESSOR_ARCHITECTURE` in the artifact vocabulary the packers already use.
fn arch(env: &Env) -> String {
    match env.get("PROCESSOR_ARCHITECTURE") {
        Some("AMD64") => "x64".into(),
        Some("ARM64") => "arm64".into(),
        Some(other) => other.to_ascii_lowercase(),
        None => String::new(),
    }
}

fn arp_install(run: &dyn CommandRunner) -> Option<WinInstall> {
    arp_install_at(run, HOST_ARP_KEY)
}

fn arp_install_at(run: &dyn CommandRunner, key: &str) -> Option<WinInstall> {
    let out = run.probe("reg", &["query", key]).filter(|o| o.ok())?;
    Some(WinInstall {
        version: parse_reg_value(&out.stdout, "DisplayVersion"),
        location: parse_reg_value(&out.stdout, "InstallLocation"),
    })
}

/// `Start ≤ 2` (boot/system/auto) only: a *disabled* Sunshine aborting a winget install was a
/// fielded bug, and the narrowing that fixed it ports verbatim.
fn competing_hosts(run: &dyn CommandRunner) -> Vec<String> {
    COMPETING_SERVICES
        .iter()
        .filter(|svc| {
            reg_value(
                run,
                &format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{svc}"),
                "Start",
            )
            .and_then(|v| hex_u32(&v))
            .is_some_and(|start| start <= 2)
        })
        .map(|svc| (*svc).to_string())
        .collect()
}

fn mgmt_bind_set(host_env: &str) -> bool {
    host_env.lines().any(|line| {
        line.trim()
            .strip_prefix("PUNKTFUNK_MGMT_BIND=")
            .is_some_and(|v| !v.trim().is_empty())
    })
}

/// The Steam streaming-audio drivers the host's mic capture rides on — their absence is a
/// warn-and-point, exactly as the shipped installer words it.
fn steam_audio_drivers(env: &Env) -> bool {
    let base = env
        .get("CommonProgramFiles(x86)")
        .unwrap_or(r"C:\Program Files (x86)\Common Files");
    // Joined component-wise so the probe (and its test tree) also works on the unix lanes,
    // where a literal backslash is not a separator.
    ["x64", "arm64", "x86"].iter().any(|a| {
        [
            "Steam",
            "drivers",
            "Windows10",
            a,
            "SteamStreamingMicrophone.inf",
        ]
        .iter()
        .fold(std::path::PathBuf::from(base), |p, part| p.join(part))
        .is_file()
    })
}

/// `schtasks /XML` is the locale-invariant surface — the human table localizes its headers,
/// the task XML never does. A missing `<Enabled>` element means enabled (the schema default).
fn task_state(run: &dyn CommandRunner, name: &str) -> TaskState {
    match run.probe("schtasks", &["/Query", "/TN", name, "/XML"]) {
        Some(out) if out.ok() => {
            if out.stdout.contains("<Enabled>false</Enabled>") {
                TaskState::Disabled
            } else {
                TaskState::Enabled
            }
        }
        _ => TaskState::Absent,
    }
}

fn reg_query(run: &dyn CommandRunner, key: &str) -> Option<String> {
    run.probe("reg", &["query", key])
        .filter(|o| o.ok())
        .map(|o| o.stdout)
}

fn reg_value(run: &dyn CommandRunner, key: &str, value: &str) -> Option<String> {
    let out = run
        .probe("reg", &["query", key, "/v", value])
        .filter(|o| o.ok())?;
    parse_reg_value(&out.stdout, value)
}

/// One value line of `reg query` output: `    <name>    REG_<type>    <data>`. The data is
/// everything after the type token, so a `REG_SZ` path with spaces survives.
pub(crate) fn parse_reg_value(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|w| w.eq_ignore_ascii_case(name)) {
            continue;
        }
        let Some(ty) = words.next().filter(|w| w.starts_with("REG_")) else {
            continue;
        };
        let after = line.find(ty)? + ty.len();
        let data = line[after..].trim();
        if !data.is_empty() {
            return Some(data.to_string());
        }
    }
    None
}

fn hex_u32(data: &str) -> Option<u32> {
    let hex = data
        .strip_prefix("0x")
        .or_else(|| data.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

/// The real box: NLA over COM, and a plain bind attempt for the port question.
#[cfg(windows)]
pub struct SystemNet;

#[cfg(windows)]
impl NetProbe for SystemNet {
    fn networks(&self) -> Vec<NetProfile> {
        nlm_networks().unwrap_or_default()
    }

    /// A listener that cannot bind means someone answers there — the same 0.0.0.0 bind both
    /// hosts take, so a false positive needs another server on the box, which IS the conflict.
    fn port_in_use(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("0.0.0.0", port)).is_err()
    }

    fn make_private(&self, network: &str) -> bool {
        nlm_make_private(network).unwrap_or(false)
    }
}

/// One COM pass over the connected networks; `visit` returns `true` to stop early.
#[cfg(windows)]
fn nlm_visit(
    mut visit: impl FnMut(&::windows::Win32::Networking::NetworkListManager::INetwork, &str) -> bool,
) -> Option<()> {
    use ::windows::Win32::Networking::NetworkListManager::{
        INetwork, INetworkListManager, NetworkListManager, NLM_ENUM_NETWORK_CONNECTED,
    };
    use ::windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // SAFETY: plain COM lifecycle — init tolerating an already-initialized thread, walk the
    // enumerator through `windows`' smart pointers, uninit only when this call did the init.
    unsafe {
        let inited = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = (|| -> ::windows::core::Result<()> {
            let nlm: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)?;
            let networks = nlm.GetNetworks(NLM_ENUM_NETWORK_CONNECTED)?;
            loop {
                let mut slot: [Option<INetwork>; 1] = [None];
                let mut fetched = 0u32;
                if networks.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(network) = slot[0].take() else { break };
                let name = network.GetName().map(|b| b.to_string()).unwrap_or_default();
                if visit(&network, &name) {
                    break;
                }
            }
            Ok(())
        })();
        if inited {
            CoUninitialize();
        }
        result.ok()
    }
}

#[cfg(windows)]
fn nlm_networks() -> Option<Vec<NetProfile>> {
    use ::windows::Win32::Networking::NetworkListManager::{
        NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED, NLM_NETWORK_CATEGORY_PRIVATE,
    };
    let mut out = Vec::new();
    // SAFETY: `GetCategory` is a plain out-value call on an interface `nlm_visit` owns.
    nlm_visit(|network, name| {
        // SAFETY: a plain out-value call on an interface `nlm_visit` owns for this frame.
        let category = match unsafe { network.GetCategory() } {
            Ok(NLM_NETWORK_CATEGORY_PRIVATE) => NetCategory::Private,
            Ok(NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED) => NetCategory::Domain,
            _ => NetCategory::Public,
        };
        out.push(NetProfile {
            name: name.to_string(),
            category,
        });
        false
    })?;
    Some(out)
}

/// D12's consented fix: find the network by name and set its NLA category to Private.
#[cfg(windows)]
fn nlm_make_private(wanted: &str) -> Option<bool> {
    use ::windows::Win32::Networking::NetworkListManager::NLM_NETWORK_CATEGORY_PRIVATE;
    let mut done = false;
    // SAFETY: `SetCategory` is a plain in-value call on an interface `nlm_visit` owns.
    nlm_visit(|network, name| {
        if name == wanted {
            // SAFETY: a plain in-value call on an interface `nlm_visit` owns for this frame.
            done = unsafe { network.SetCategory(NLM_NETWORK_CATEGORY_PRIVATE) }.is_ok();
            return true;
        }
        false
    })?;
    Some(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::FakeRunner;

    /// A realistic `reg query` transcript for one value.
    fn reg_out(key: &str, name: &str, ty: &str, data: &str) -> String {
        format!("\r\n{key}\r\n    {name}    {ty}    {data}\r\n\r\n")
    }

    fn probe(run: &FakeRunner, env: &Env, net: &FakeNet, root: &std::path::Path) -> WinFacts {
        WinFacts::probe(&BasePaths::rooted(root), run, env, net)
    }

    fn fresh_box() -> (FakeRunner, Env, FakeNet, tempfile::TempDir) {
        let run = FakeRunner::new()
            .answer(
                r"reg query HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion /v CurrentBuildNumber",
                0,
                &reg_out("HKEY...", "CurrentBuildNumber", "REG_SZ", "26200"),
            )
            .with_path("reg")
            .with_path("schtasks");
        let env = Env::of(&[("PROCESSOR_ARCHITECTURE", "AMD64")]);
        (run, env, FakeNet::default(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn the_client_key_is_probed_apart_from_the_hosts() {
        let (mut run, env, net, tmp) = fresh_box();
        run = run.answer(
            &format!("reg query {}", plan::CLIENT_ARP_KEY),
            0,
            &reg_out(plan::CLIENT_ARP_KEY, "DisplayVersion", "REG_SZ", "0.33.1"),
        );
        let facts = probe(&run, &env, &net, tmp.path());
        assert!(facts.installed.is_none());
        assert_eq!(
            facts
                .installed_for(plan::Artifact::Client)
                .unwrap()
                .version
                .as_deref(),
            Some("0.33.1")
        );
        assert!(facts.installed_for(plan::Artifact::Host).is_none());
    }

    #[test]
    fn the_real_box_reads_host_env_under_program_data() {
        let paths = base_paths(&Env::of(&[("ProgramData", r"D:\PD")]));
        assert!(paths.host_env().starts_with(r"D:\PD"));
        assert!(paths.host_env().ends_with("punktfunk/host.env"));
        assert!(base_paths(&Env::default())
            .config
            .starts_with(r"C:\ProgramData"));
    }

    #[test]
    fn the_uninstaller_name_switches_mode_whatever_its_case() {
        let p = std::path::Path::new;
        assert!(launched_as_uninstaller(p(
            r"C:\Program Files\punktfunk\UNINS000.EXE"
        )));
        assert!(launched_as_uninstaller(p("unins000.exe")));
        assert!(!launched_as_uninstaller(p(
            r"C:\x\punktfunk-host-setup.exe"
        )));
        assert!(!launched_as_uninstaller(p(r"C:\unins000.exe\setup.exe")));
    }

    #[test]
    fn reg_value_survives_spaces_in_reg_sz_data() {
        let text = reg_out(
            HOST_ARP_KEY,
            "InstallLocation",
            "REG_SZ",
            r"C:\Program Files\punktfunk\",
        );
        assert_eq!(
            parse_reg_value(&text, "InstallLocation").unwrap(),
            r"C:\Program Files\punktfunk\"
        );
        assert_eq!(parse_reg_value(&text, "DisplayVersion"), None);
    }

    #[test]
    fn a_fresh_box_probes_clean() {
        let (run, env, net, tmp) = fresh_box();
        let facts = probe(&run, &env, &net, tmp.path());
        assert_eq!(facts.os_build, 26200);
        assert_eq!(facts.arch, "x64");
        assert!(facts.installed.is_none());
        assert!(!facts.host_env_present);
        assert!(!facts.needs_coexistence());
        assert_eq!(facts.web_task, TaskState::Absent);
    }

    #[test]
    fn an_arp_key_reports_the_install_with_version_and_location() {
        let (mut run, env, net, tmp) = fresh_box();
        let body = format!(
            "\r\n{HOST_ARP_KEY}\r\n    DisplayVersion    REG_SZ    0.35.16661\r\n    InstallLocation    REG_SZ    C:\\Program Files\\punktfunk\\\r\n\r\n"
        );
        run = run.answer(&format!("reg query {HOST_ARP_KEY}"), 0, &body);
        let facts = probe(&run, &env, &net, tmp.path());
        let installed = facts.installed.unwrap();
        assert_eq!(installed.version.unwrap(), "0.35.16661");
        assert_eq!(installed.location.unwrap(), r"C:\Program Files\punktfunk\");
    }

    // The winget narrowing, verbatim: Start=4 (disabled) and Start=3 (manual) are not a
    // conflict; only boot/system/auto are.
    #[test]
    fn a_disabled_competitor_is_not_a_conflict() {
        let (mut run, env, net, tmp) = fresh_box();
        run = run
            .answer(
                r"reg query HKLM\SYSTEM\CurrentControlSet\Services\SunshineService /v Start",
                0,
                &reg_out("HKEY...", "Start", "REG_DWORD", "0x2"),
            )
            .answer(
                r"reg query HKLM\SYSTEM\CurrentControlSet\Services\ApolloService /v Start",
                0,
                &reg_out("HKEY...", "Start", "REG_DWORD", "0x4"),
            );
        let facts = probe(&run, &env, &net, tmp.path());
        assert_eq!(facts.competing_hosts, ["SunshineService"]);
        assert!(facts.needs_coexistence());
    }

    #[test]
    fn a_bound_mgmt_port_is_a_conflict_even_without_a_service() {
        let (run, env, _, tmp) = fresh_box();
        let net = FakeNet {
            ports_in_use: vec![MGMT_PORT],
            ..FakeNet::default()
        };
        let facts = probe(&run, &env, &net, tmp.path());
        assert!(facts.competing_hosts.is_empty());
        assert!(facts.needs_coexistence());
    }

    // D11's guard: an operator who moved the port themselves already coexists — rewriting
    // their value would be the bug.
    #[test]
    fn an_operator_mgmt_bind_disarms_coexistence() {
        let (mut run, env, _, tmp) = fresh_box();
        run = run.answer(
            r"reg query HKLM\SYSTEM\CurrentControlSet\Services\SunshineService /v Start",
            0,
            &reg_out("HKEY...", "Start", "REG_DWORD", "0x2"),
        );
        let net = FakeNet::default();
        let dir = tmp.path().join("config/punktfunk");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("host.env"), "PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n").unwrap();
        let facts = probe(&run, &env, &net, tmp.path());
        assert!(facts.mgmt_bind_set);
        assert!(!facts.needs_coexistence());
    }

    #[test]
    fn a_commented_or_empty_mgmt_bind_does_not_count() {
        assert!(!mgmt_bind_set("# PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n"));
        assert!(!mgmt_bind_set("PUNKTFUNK_MGMT_BIND=\n"));
        assert!(mgmt_bind_set("  PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\n"));
    }

    #[test]
    fn task_state_reads_the_xml_not_the_localized_table() {
        let enabled = FakeRunner::new().answer(
            "schtasks /Query /TN PunktfunkScripting /XML",
            0,
            "<Task><Settings><Enabled>true</Enabled></Settings></Task>",
        );
        assert_eq!(
            task_state(&enabled, "PunktfunkScripting"),
            TaskState::Enabled
        );
        let disabled = FakeRunner::new().answer(
            "schtasks /Query /TN PunktfunkScripting /XML",
            0,
            "<Task><Settings><Enabled>false</Enabled></Settings></Task>",
        );
        assert_eq!(
            task_state(&disabled, "PunktfunkScripting"),
            TaskState::Disabled
        );
        let absent = FakeRunner::new().with_path("schtasks");
        assert_eq!(task_state(&absent, "PunktfunkScripting"), TaskState::Absent);
    }

    #[test]
    fn public_networks_filters_and_domain_is_not_public() {
        let net = FakeNet {
            networks: vec![
                NetProfile {
                    name: "Home".into(),
                    category: NetCategory::Private,
                },
                NetProfile {
                    name: "Cafe".into(),
                    category: NetCategory::Public,
                },
                NetProfile {
                    name: "Corp".into(),
                    category: NetCategory::Domain,
                },
            ],
            ..FakeNet::default()
        };
        let (run, env, _, tmp) = fresh_box();
        let facts = probe(&run, &env, &net, tmp.path());
        let public: Vec<_> = facts
            .public_networks()
            .iter()
            .map(|n| n.name.clone())
            .collect();
        assert_eq!(public, ["Cafe"]);
    }

    #[test]
    fn steam_audio_drivers_found_under_any_arch_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let inf = tmp.path().join(r"Steam/drivers/Windows10/arm64");
        std::fs::create_dir_all(&inf).unwrap();
        std::fs::write(inf.join("SteamStreamingMicrophone.inf"), "").unwrap();
        let env = Env::of(&[("CommonProgramFiles(x86)", tmp.path().to_str().unwrap())]);
        assert!(steam_audio_drivers(&env));
        assert!(!steam_audio_drivers(&Env::of(&[(
            "CommonProgramFiles(x86)",
            "/nonexistent"
        )])));
    }

    // --facts and --demo hand WinFacts through JSON; a field that doesn't round-trip is a
    // field the demo lies about.
    #[test]
    fn win_facts_round_trip_through_serde() {
        let (mut run, env, _, tmp) = fresh_box();
        run = run.answer(
            r"reg query HKLM\SYSTEM\CurrentControlSet\Services\SunshineService /v Start",
            0,
            &reg_out("HKEY...", "Start", "REG_DWORD", "0x2"),
        );
        let net = FakeNet {
            networks: vec![NetProfile {
                name: "Cafe".into(),
                category: NetCategory::Public,
            }],
            ports_in_use: vec![MGMT_PORT],
            ..FakeNet::default()
        };
        let facts = probe(&run, &env, &net, tmp.path());
        let json = serde_json::to_string(&facts).unwrap();
        let back: WinFacts = serde_json::from_str(&json).unwrap();
        assert_eq!(facts, back);
    }
}
