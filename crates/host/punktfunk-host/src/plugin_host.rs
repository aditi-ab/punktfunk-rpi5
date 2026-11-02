//! Plugins on this host: the runner and each plugin's access, and the store that installs
//! them from signed catalogs.

// A plugin's page without a listener: connections it parks, requests sent down them.
pub(crate) mod channel;
pub(crate) mod plugins;
// Running plugins and their UI credentials; the `/plugins` routes sit on it.
pub(crate) mod registry;
// Signed catalogs and install jobs via the `plugins` runner — design/plugin-store.md.
pub(crate) mod store;

/// Each installed plugin's management-API bearer, by plugin id. A store job swaps the whole set.
pub(crate) type PluginTokens =
    std::sync::Arc<std::sync::RwLock<std::collections::BTreeMap<String, String>>>;

/// Deletes a provider's library titles and returns how many went. The library sits above
/// `plugin_host`, so the `plugins remove` CLI (`main`) and the store route (`mgmt`) pass it in.
pub(crate) type DeleteTitles = fn(&str) -> anyhow::Result<usize>;

/// A store job rewrote the token file: the pipe set reconciles now, not at its next tick.
#[cfg(windows)]
pub(crate) static TOKENS_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();
