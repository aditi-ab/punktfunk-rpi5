//! The real box behind the executor (WP3.1): the extracted tree as `PayloadSource`, and the
//! seams a run is built on. `Seams::Demo` is the sandboxed fake set every preset walks;
//! `Seams::Real` probes and mutates this machine — its `root` is the extracted payload
//! (`None` only for the silent `--dry-run`, which deploys nothing).

use std::path::{Path, PathBuf};

use punktfunk_setup::platform::windows::exec::{PayloadSource, Subst};

#[derive(Clone, Debug, PartialEq)]
pub enum Seams {
    Demo {
        latency_ms: u64,
    },
    Real {
        root: Option<PathBuf>,
        version: String,
    },
}

/// `app/` → `{app}`; `staging/` stays where it is — the plan's `<staging>` points at it, and
/// the root's protected DACL is inherited, which is what the driver legs require.
pub struct DirPayload {
    pub root: PathBuf,
}

impl PayloadSource for DirPayload {
    fn deploy(&self, dest: &Path) -> Result<(), String> {
        let app = self.root.join("app");
        if !app.is_dir() {
            return Err("this payload carries no app tree — an uninstaller cannot install".into());
        }
        crate::pack::copy_tree(&app, dest).map(|_| ())
    }
}

/// The placeholders for a real run: driver staging and the ACL'd temp under the same root.
pub fn subst(root: Option<&Path>, version: &str) -> Subst {
    let (staging, temp) = match root {
        Some(root) => {
            let tmp = root.join("tmp");
            let _ = std::fs::create_dir_all(&tmp);
            (root.join("staging"), tmp)
        }
        None => (PathBuf::from("<staging>"), std::env::temp_dir()),
    };
    Subst {
        version: version.to_string(),
        staging: staging.display().to_string(),
        temp: temp.display().to_string(),
    }
}
