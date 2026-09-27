//! A running gamescope's command line: which processes are gamescope, and the flag values they
//! carry. gamescope parses with `getopt_long`, so a long flag's value is the next token or follows
//! `=`; every reader here takes both.

/// Compositor argv from `/proc/<pid>/cmdline`. Basename `ends_with("gamescope")` — `/proc/…/exe`
/// is often unreadable, and `==` would miss `punktfunk-gamescope` while still excluding helpers.
pub(super) fn gamescope_argvs() -> Vec<Vec<String>> {
    crate::proc::pids()
        .filter_map(|(_, path)| {
            let raw = std::fs::read(path.join("cmdline")).ok()?;
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            let a0 = args.first()?;
            a0.rsplit('/')
                .next()
                .unwrap_or(a0)
                .ends_with("gamescope")
                .then_some(args)
        })
        .collect()
}

/// Value of the first matching flag, in `--flag value` and `--flag=value` form.
pub(super) fn flag_value<'a>(argv: &'a [String], names: &[&str]) -> Option<&'a str> {
    argv.iter().enumerate().find_map(|(i, a)| {
        if let Some((k, v)) = a.split_once('=') {
            if names.contains(&k) {
                return Some(v);
            }
        }
        if names.contains(&a.as_str()) {
            return argv.get(i + 1).map(|s| s.as_str());
        }
        None
    })
}

pub(super) fn argv_u32(argv: &[String], names: &[&str]) -> Option<u32> {
    flag_value(argv, names)?.parse().ok()
}

/// `-W`/`-H` of one argv. `None` if either is missing — also the compositor vs helper filter.
pub(super) fn gamescope_output_size(argv: &[String]) -> Option<(u32, u32)> {
    match (
        argv_u32(argv, &["-W", "--output-width"]),
        argv_u32(argv, &["-H", "--output-height"]),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}
