//! The `/serverinfo` capability/status XML Moonlight GETs before pairing and each launch.

use super::{Host, APP_VERSION, GFE_VERSION, SERVER_CODEC_MODE_SUPPORT};

/// Build the `<root status_code="200">…</root>` serverinfo document. `https` selects the
/// paired-HTTPS variant (real MAC). Element names are case-sensitive and match what
/// moonlight-common-c parses.
pub fn serverinfo_xml(host: &Host, https: bool) -> String {
    // MAC is hidden over plain HTTP; PairStatus reflects the pairing store once the HTTPS
    // path carries per-client identity (a hardening follow-up — 0 for now).
    let mac = if https {
        "01:02:03:04:05:06"
    } else {
        "00:00:00:00:00:00"
    };
    // Over the mutual-TLS HTTPS port the peer is an authenticated (paired) client.
    let pair_status = u8::from(https);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
<hostname>{hostname}</hostname>
<appversion>{APP_VERSION}</appversion>
<GfeVersion>{GFE_VERSION}</GfeVersion>
<uniqueid>{uniqueid}</uniqueid>
<HttpsPort>{https_port}</HttpsPort>
<ExternalPort>{http_port}</ExternalPort>
<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
<mac>{mac}</mac>
<LocalIP>{local_ip}</LocalIP>
<ServerCodecModeSupport>{SERVER_CODEC_MODE_SUPPORT}</ServerCodecModeSupport>
<PairStatus>{pair_status}</PairStatus>
<currentgame>0</currentgame>
<state>SUNSHINE_SERVER_FREE</state>
</root>
"#,
        hostname = host.hostname,
        uniqueid = host.uniqueid,
        https_port = host.https_port,
        http_port = host.http_port,
        local_ip = host.local_ip,
    )
}
