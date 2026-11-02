//! The desktop's own colours, so the console can look like it belongs to the box it manages
//! (`design/web-console-overhaul.md` §7).
//!
//! One route, read per request. There is no cache: the console already polls `ui-config` every
//! two seconds to catch `omarchy-theme-set`, and the same tick then catches a Windows accent
//! change for free. A registry read or one D-Bus round trip is cheaper than the staleness a
//! cache would buy.
//!
//! Linux reads the XDG portal, which is the only cross-desktop answer — GNOME, Plasma and
//! COSMIC all serve it, and `xdg-desktop-portal-gtk` answers for Hyprland and sway. A box with
//! no portal reports nothing, and the console says so; reading `gtk-4.0/settings.ini` behind
//! its back would be a second, quietly wrong code path.
//!
//! Windows reads the CONSOLE SESSION's hive. The host is SYSTEM in session 0 and has no HKCU
//! of its own, so `HKEY_CURRENT_USER` here would be the service account's empty profile. Its Win32
//! calls live in `crate::windows::theme`, because this module forbids `unsafe`.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// The desktop's appearance, as far as this host can see it.
#[derive(Serialize, ToSchema, Default)]
pub(crate) struct HostTheme {
    /// Where the answer came from, for the console's "Follow host — Windows" line.
    /// `null` when nothing answered.
    #[schema(example = "gnome")]
    source: Option<String>,
    /// `light` | `dark`, or `null` when the desktop expresses no preference.
    mode: Option<String>,
    /// `#rrggbb`, or `null` when the desktop has no accent to report.
    accent: Option<String>,
}

/// Host desktop theme
///
/// The mode and accent colour of the desktop this host runs on, for a console that follows it.
/// Every field is nullable: "no answer" is a real state, and the console renders it as
/// "none detected" rather than guessing.
#[utoipa::path(
    get,
    path = "/host/theme",
    tag = "host",
    operation_id = "getHostTheme",
    responses(
        (status = OK, description = "The desktop's mode and accent, as far as this host can see", body = HostTheme),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = crate::mgmt::shared::ApiError),
    )
)]
pub(crate) async fn get_host_theme() -> Json<HostTheme> {
    Json(read().await)
}

/// Awaited on the host's own runtime, on a bus connection of its own that closes with the
/// read. The await holds no worker while the portal thinks. Plain method calls, not ashpd's
/// `Settings`: its proxy caches properties, and on a portal without the Settings interface
/// zbus warns about the failed `GetAll` on every poll.
#[cfg(target_os = "linux")]
async fn read() -> HostTheme {
    // A desktop running no portal must not stall the request. Anything slower than this is
    // "no answer" as far as the console is concerned, and the budget covers the whole
    // exchange: a portal that accepts the connection and then never answers is the case
    // worth bounding.
    const BUDGET: std::time::Duration = std::time::Duration::from_millis(400);
    match tokio::time::timeout(BUDGET, async {
        let Ok(conn) = pf_portal::session_bus().await else {
            return HostTheme::default();
        };
        // 0 = no preference, 1 = dark, 2 = light.
        let mode = match appearance::<u32>(&conn, "color-scheme").await {
            Some(1) => Some("dark".into()),
            Some(2) => Some("light".into()),
            _ => None,
        };
        let accent = appearance::<(f64, f64, f64)>(&conn, "accent-color")
            .await
            .map(|(r, g, b)| rgb_hex(r, g, b));
        // Only claim a source when something actually answered, or a box with no portal
        // would read as "following" a desktop that said nothing.
        let source = (mode.is_some() || accent.is_some()).then(|| "portal".to_string());
        HostTheme {
            source,
            mode,
            accent,
        }
    })
    .await
    {
        Ok(theme) => theme,
        Err(_) => {
            tracing::debug!("host theme: the desktop portal did not answer in time");
            HostTheme::default()
        }
    }
}

/// One `org.freedesktop.appearance` key. `None` covers every miss: no portal, no Settings
/// interface, a key this desktop does not publish, or a value of another type.
#[cfg(target_os = "linux")]
async fn appearance<T>(conn: &zbus::Connection, key: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "Read",
            &("org.freedesktop.appearance", key),
        )
        .await
        .ok()?;
    let body = reply.body();
    // `Read`, because Settings v1 portals have no `ReadOne`. It nests the value in one more
    // variant; peel it.
    let value = match body.deserialize::<zbus::zvariant::Value>().ok()? {
        zbus::zvariant::Value::Value(inner) => *inner,
        v => v,
    };
    T::try_from(value.try_into_owned().ok()?).ok()
}

/// The portal reports each channel as 0.0..=1.0.
#[cfg(target_os = "linux")]
fn rgb_hex(r: f64, g: f64, b: f64) -> String {
    let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", ch(r), ch(g), ch(b))
}

/// The registry read blocks, and the console polls this every two seconds per open tab.
#[cfg(target_os = "windows")]
async fn read() -> HostTheme {
    tokio::task::spawn_blocking(read_registry)
        .await
        .unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn read_registry() -> HostTheme {
    use crate::windows::theme::{read_dword, session_sid};
    let Some(sid) = session_sid() else {
        // Nobody signed in to the host's session: there is no user hive to read, and saying
        // nothing is correct.
        return HostTheme::default();
    };
    let mode = read_dword(
        &format!(r"{sid}\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        "AppsUseLightTheme",
    )
    .map(|v| {
        if v == 0 {
            "dark".to_string()
        } else {
            "light".to_string()
        }
    });
    let accent = read_dword(
        &format!(r"{sid}\Software\Microsoft\Windows\DWM"),
        "AccentColor",
    )
    .map(abgr_hex);
    let source = (mode.is_some() || accent.is_some()).then(|| "windows".to_string());
    HostTheme {
        source,
        mode,
        accent,
    }
}

/// DWM stores the accent as ABGR: the alpha rides the top byte and the channels are reversed
/// from the `#rrggbb` a stylesheet wants.
#[cfg(target_os = "windows")]
fn abgr_hex(v: u32) -> String {
    let [b, g, r] = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn read() -> HostTheme {
    // No host runs here; the console falls back to its own palette.
    HostTheme::default()
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// DWM's DWORD is ABGR with alpha on top; a naive read renders every accent's red and
    /// blue swapped, which looks plausible and is wrong.
    #[test]
    fn accent_is_read_as_abgr_not_rgb() {
        // Windows' own default blue, #0078d4, is stored as 0xFFD47800.
        assert_eq!(abgr_hex(0xFFD4_7800), "#0078d4");
    }
}
