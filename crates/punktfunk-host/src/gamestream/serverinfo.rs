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
        local_ip = host.local_ip(),
    )
}

/// The `<ServerCodecModeSupport>` mask to advertise: the SDR baseline ([`base_codec_mode_support`])
/// plus the 10-bit (HDR) bit of each codec the host can actually deliver HDR with ([`apply_hdr`] /
/// [`crate::gamestream::host_hdr_capable`]). Without a 10-bit bit Moonlight never offers its HDR
/// toggle; with one, enabling HDR client-side negotiates that profile and the host streams
/// BT.2020 PQ.
fn codec_mode_support() -> u32 {
    use crate::encode::Codec;
    // Per codec, exactly like the SDR baseline is: `can_encode_10bit` answers for the backend this
    // host will actually open (on AMD/Intel, the union of VAAPI's and Vulkan Video's 10-bit
    // support), so a box that encodes HEVC Main10 but not 10-bit AV1 — or the reverse — advertises
    // the truth instead of one bit standing in for both.
    let hdr = crate::gamestream::host_hdr_capable();
    apply_hdr(
        base_codec_mode_support(),
        hdr && crate::encode::can_encode_10bit(Codec::H265),
        hdr && crate::encode::can_encode_10bit(Codec::Av1),
    )
}

/// Layer each codec's 10-bit (HDR) bit onto `base`, gated on the SDR baseline already advertising
/// that codec — pure so the HDR-layering is unit-testable without a GPU.
///
/// AV1 Main10 used to be omitted unconditionally, on the theory that the GameStream AV1 path was
/// unconfirmed. But the baseline already offers AV1 **Main8** to every client, so the AV1 path is
/// either live or it is not — the depth was never the uncertain part. Now that the encoders probe
/// 10-bit per codec, withholding the bit only cost AV1-preferring clients their HDR.
fn apply_hdr(base: u32, hevc_10bit: bool, av1_10bit: bool) -> u32 {
    let mut m = base;
    if hevc_10bit && base & super::SCM_HEVC != 0 {
        m |= super::SCM_HEVC_MAIN10;
    }
    if av1_10bit && base & super::SCM_AV1_MAIN8 != 0 {
        m |= super::SCM_AV1_MAIN10;
    }
    m
}

/// The **SDR baseline** mask. On the VAAPI (AMD/Intel) backend it reflects what the GPU can ACTUALLY
/// encode (probed — AV1 is narrow, and an old iGPU might lack HEVC), so a Moonlight client never
/// negotiates a codec the encoder can't open. NVENC and the GPU-less software path keep the
/// Moonlight-validated static superset. HDR (Main10) is layered on by [`codec_mode_support`].
fn base_codec_mode_support() -> u32 {
    // A GPU-less host encodes H.264 and nothing else (openh264), so advertising the superset made
    // Moonlight negotiate HEVC/AV1 and the session then died at encoder open with "the software
    // encoder emits H.264 only". `pf_encode::Codec::host_wire_caps` — the native plane's twin of
    // this function — has gated on exactly this since it was written; this one never did.
    //
    // Deliberately a local gate rather than delegating wholesale to `host_wire_caps()`: that would
    // be the drift-proof shape, but on Windows it re-runs the DXGI adapter enumeration several
    // times per `/serverinfo` GET (the probe helpers each sample it), and this endpoint is polled.
    // The software case is a plain config read, so it costs nothing here. (Follow-up worth doing:
    // the static `MaxLumaPixelsHEVC` in the XML above still advertises an HEVC limit even when the
    // mask drops HEVC — harmless, since Moonlight gates on the mask, but it is a second and now
    // inconsistent advertisement.)
    if matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    ) {
        return super::SCM_H264;
    }
    #[cfg(target_os = "linux")]
    if crate::encode::linux_zero_copy_is_vaapi() {
        if let Some(m) = probed_mask(crate::encode::vaapi_codec_support()) {
            return m;
        }
    }
    // Linux NVIDIA: the driver's own encode-GUID list (`nvenc_codec_support`, one throwaway
    // direct-SDK session, cached) — the same probe `host_wire_caps` consults, so both planes
    // stop advertising HEVC/AV1 on a chip without them (the GM107 dead-session field bug).
    // Fail-open like every arm here: an unanswerable probe → `probed_mask` = None → superset.
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    if !crate::encode::linux_zero_copy_is_vaapi() {
        if let Some(m) = probed_mask(crate::encode::nvenc_codec_support()) {
            return m;
        }
    }
    // Windows: advertise only what the GPU actually encodes (AV1 is narrow, an old iGPU might
    // lack HEVC, a 1st-gen-Maxwell NVENC is H.264-only). AMF probes natively (no build feature
    // needed); QSV needs the libavcodec or VPL build, NVENC the `nvenc` build. The GPU-less
    // software path keeps the static superset.
    #[cfg(target_os = "windows")]
    if crate::encode::windows_backend_is_probed() {
        if let Some(m) = probed_mask(crate::encode::windows_codec_support()) {
            return m;
        }
    }
    SERVER_CODEC_MODE_SUPPORT
}

/// Turn a probed [`CodecSupport`](crate::encode::CodecSupport) into a `ServerCodecModeSupport` mask,
/// or `None` if the probe found nothing — meaning the GPU wasn't usable at probe time (GPU-less CI,
/// a misconfigured/wrong-vendor host), NOT that it encodes zero codecs; the caller then advertises
/// the static superset (pre-probe behaviour) rather than claiming nothing.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn probed_mask(caps: crate::encode::CodecSupport) -> Option<u32> {
    use super::{SCM_AV1_MAIN8, SCM_H264, SCM_HEVC};
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
    (m != 0).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamestream::{SCM_AV1_MAIN10, SCM_AV1_MAIN8, SCM_H264, SCM_HEVC, SCM_HEVC_MAIN10};

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

    /// The 10-bit bits are layered PER CODEC, and each needs both halves: the host able to encode
    /// 10-bit for that codec, and the SDR baseline already advertising it. A client gates its HDR
    /// toggle on these, so an over-claim invites it into a mode the encoder cannot open.
    #[test]
    fn apply_hdr_adds_each_codecs_10bit_bit_independently() {
        let sdr = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;
        // Both codecs 10-bit-capable → both bits.
        assert_eq!(
            apply_hdr(sdr, true, true),
            sdr | SCM_HEVC_MAIN10 | SCM_AV1_MAIN10
        );
        // Neither → baseline unchanged (no HDR claim).
        assert_eq!(apply_hdr(sdr, false, false), sdr);
        // One without the other — the case a single shared flag used to get wrong in both
        // directions (AV1 Main10 was never advertised at all, and HEVC Main10 stood in for it).
        assert_eq!(apply_hdr(sdr, true, false), sdr | SCM_HEVC_MAIN10);
        assert_eq!(apply_hdr(sdr, false, true), sdr | SCM_AV1_MAIN10);
        // 10-bit-capable but the codec isn't in the SDR baseline at all → no bit for it.
        assert_eq!(apply_hdr(SCM_H264, true, true), SCM_H264);
        assert_eq!(
            apply_hdr(SCM_H264 | SCM_HEVC, true, true),
            SCM_H264 | SCM_HEVC | SCM_HEVC_MAIN10
        );
    }

    #[test]
    fn serverinfo_xml_carries_codec_mask() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
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
