//! One pass over the PipeWire registry for the apps playing audio right now.
//!
//! Feeds the console's voice-chat app picker, so it names apps the way
//! `pf_host_config::voice_app_matches` matches them: process binary first, then
//! `application.name`, lowercased.

use anyhow::{Context, Result};

/// Every audio output stream's app, deduped and sorted. The host's own streams are left out.
pub(crate) fn playing_apps() -> Result<Vec<String>> {
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context.connect_rc(None).context("pw connect")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let apps: Rc<RefCell<Vec<String>>> = Rc::default();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let apps = apps.clone();
            move |g| {
                let Some(props) = g.props else { return };
                if !matches!(g.type_, pw::types::ObjectType::Node)
                    || props.get("media.class") != Some("Stream/Output/Audio")
                {
                    return;
                }
                let name = app_name(
                    props.get("application.process.binary"),
                    props.get("application.name"),
                );
                let mut apps = apps.borrow_mut();
                if let Some(name) = name.filter(|n| !apps.contains(n)) {
                    apps.push(name);
                }
            }
        })
        .register();

    // One sync round: the registry replays every global before the `done` for this seq.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    awaited.set(Some(core.sync(0).context("pw sync")?));
    mainloop.run();

    let mut out = apps.take();
    out.sort();
    Ok(out)
}

fn app_name(binary: Option<&str>, name: Option<&str>) -> Option<String> {
    [binary, name]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_ascii_lowercase())
        .find(|s| !s.is_empty() && !s.contains(','))
        .filter(|s| s != "punktfunk-host")
}

#[cfg(test)]
mod tests {
    use super::app_name;

    #[test]
    fn names_prefer_the_binary_and_skip_the_host() {
        assert_eq!(
            app_name(Some("Discord"), Some("WEBRTC VoiceEngine")).as_deref(),
            Some("discord")
        );
        assert_eq!(
            app_name(None, Some(" Firefox ")).as_deref(),
            Some("firefox")
        );
        assert_eq!(app_name(Some(""), None), None);
        // A comma would split into two entries in the env form of the list.
        assert_eq!(app_name(Some("a,b"), None), None);
        assert_eq!(app_name(Some("punktfunk-host"), None), None);
    }
}
