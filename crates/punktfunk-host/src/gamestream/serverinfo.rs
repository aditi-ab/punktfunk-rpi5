//! The `/serverinfo` capability/status XML Moonlight GETs before pairing and each launch.

use super::{Host, APP_VERSION, GFE_VERSION, SERVER_CODEC_MODE_SUPPORT};

/// Build the `<root status_code="200">…</root>` serverinfo document. `https` selects the
/// paired-HTTPS variant (real MAC); `paired` is whether the HTTPS peer presented a client cert
/// that is in the paired allow-list (drives `PairStatus`). Element names are case-sensitive and
/// match what moonlight-common-c parses.
pub fn serverinfo_xml(host: &Host, https: bool, paired: bool) -> String {
    // MAC is hidden over plain HTTP (no per-client identity there).
    let mac = if https {
        "01:02:03:04:05:06"
    } else {
        "00:00:00:00:00:00"
    };
    // PairStatus reflects the real allow-list: 1 only when the HTTPS peer's client-cert
    // fingerprint is pinned (the nvhttp handler computes `paired`); 0 otherwise (incl. plain HTTP).
    let pair_status = u8::from(paired);
    let codec_mode_support = codec_mode_support();
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
<ServerCodecModeSupport>{codec_mode_support}</ServerCodecModeSupport>
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

/// The `<ServerCodecModeSupport>` mask to advertise. On the VAAPI (AMD/Intel) backend it reflects
/// what the GPU can ACTUALLY encode (probed — AV1 is narrow, and an old iGPU might lack HEVC), so a
/// Moonlight client never negotiates a codec the encoder can't open. NVENC and Windows keep the
/// Moonlight-validated static superset.
fn codec_mode_support() -> u32 {
    #[cfg(target_os = "linux")]
    if crate::encode::linux_zero_copy_is_vaapi() {
        use super::{SCM_AV1_MAIN8, SCM_H264, SCM_HEVC};
        let caps = crate::encode::vaapi_codec_support();
        let mut m = 0;
        if caps.h264 {
            m |= SCM_H264;
        }
        if caps.h265 {
            m |= SCM_HEVC;
        }
        if caps.av1 {
            m |= SCM_AV1_MAIN8;
        }
        // Only trust a probe that actually found an encoder. An empty result means VAAPI wasn't
        // usable at probe time (no VA display — a GPU-less CI box, or a misconfigured host), NOT
        // that the GPU encodes nothing; advertise the static superset (pre-probe behaviour) rather
        // than claiming zero codecs.
        if m != 0 {
            return m;
        }
    }
    SERVER_CODEC_MODE_SUPPORT
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
        let xml = serverinfo_xml(&host, false, false);
        // The mask is the GPU-aware value (NVENC/no-GPU → the static 65793; a VAAPI host →
        // whatever it probes). Assert the XML embeds exactly what `codec_mode_support()` returns,
        // so the test is deterministic regardless of the build host's GPU.
        let mask = codec_mode_support();
        assert!(mask != 0, "must advertise at least one codec");
        assert!(xml.contains(&format!(
            "<ServerCodecModeSupport>{mask}</ServerCodecModeSupport>"
        )));
    }
}
