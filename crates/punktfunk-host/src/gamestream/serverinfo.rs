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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamestream::{SCM_AV1_MAIN8, SCM_H264, SCM_HEVC, SCM_HEVC_MAIN10};

    /// The advertised codec mask: H.264 + HEVC + AV1 Main8 (= 65793), and explicitly *no*
    /// 10-bit bits — Moonlight gates its HDR mode on those, which we can't deliver (8-bit
    /// SDR capture). Flag values are moonlight-common-c `Limelight.h`.
    #[test]
    fn codec_mode_support_mask() {
        assert_eq!(SERVER_CODEC_MODE_SUPPORT, 0x1 | 0x100 | 0x10000);
        assert_eq!(SERVER_CODEC_MODE_SUPPORT, 65793);
        assert_eq!(
            SERVER_CODEC_MODE_SUPPORT & SCM_HEVC_MAIN10,
            0,
            "no 10-bit/HDR claim"
        );
        assert_eq!(
            SERVER_CODEC_MODE_SUPPORT,
            SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8
        );
    }

    #[test]
    fn serverinfo_xml_carries_codec_mask() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            local_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            http_port: 47989,
            https_port: 47984,
        };
        let xml = serverinfo_xml(&host, false);
        assert!(xml.contains("<ServerCodecModeSupport>65793</ServerCodecModeSupport>"));
    }
}
