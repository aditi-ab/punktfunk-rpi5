//! Store tests that need the config and cache dirs pointed at a scratch dir. Those dirs come
//! from the environment, so the test runs its own binary again as a child with them set, and the
//! child runs the body. The parent's environment is never written.

use std::path::Path;
use std::process::Command;

/// Run `body(home)` in a child of the calling test binary, with the config dir at `home/config`
/// and the cache dirs under `home`. `test` is the calling test's own name and `child_var` an
/// environment variable only the child has set.
pub fn in_scratch_home(test: &str, child_var: &str, body: fn(&Path)) {
    if let Some(home) = std::env::var_os(child_var) {
        let home = Path::new(&home);
        std::fs::create_dir_all(home.join("config")).expect("scratch config dir");
        body(home);
        // The body's last act. A filter that matched no test would exit 0 without it.
        std::fs::write(home.join("ran"), b"").expect("mark the run");
        return;
    }
    let home = std::env::temp_dir().join(format!("pf-core-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("scratch dir");
    let child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", test, "--nocapture"])
        .env(child_var, &home)
        .env("PUNKTFUNK_CONFIG_DIR", home.join("config"))
        .env("XDG_CACHE_HOME", &home)
        .env("LOCALAPPDATA", &home)
        .output()
        .expect("run the child");
    let ran = home.join("ran").exists();
    std::fs::remove_dir_all(&home).ok();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(ran, "the child ran no test");
}
