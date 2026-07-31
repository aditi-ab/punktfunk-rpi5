package io.unom.punktfunk.kit

import android.media.MediaCodecInfo.CodecCapabilities
import android.media.MediaCodecList
import android.os.Build

/** The decoder Kotlin ranked for a MIME, handed to [NativeBridge.nativeStartVideo]. */
data class DecoderChoice(val name: String, val lowLatencyFeature: Boolean)

/**
 * Rank the platform's `MediaCodecList` decoders for a video MIME and pick the best one for
 * low-latency streaming, the way Moonlight-Android does. There is no NDK `MediaCodecList`, so this
 * enumeration must live on the Kotlin (framework) side; Rust then creates the chosen decoder by
 * name (`AMediaCodec_createCodecByName`) and derives the per-SoC vendor low-latency keys from it.
 *
 * Ranking (best first): hardware over software; a real SoC-vendor decoder (Qualcomm/Amlogic/…) over
 * the generic AOSP software fallback; a decoder advertising `FEATURE_LowLatency` over one that
 * doesn't. Known-bad software decoders (`omx.google.*`, `c2.android.*`, Qualcomm/Samsung SW HEVC)
 * are dropped outright — matching Moonlight's blacklist.
 */
object VideoDecoders {
    /** Decoder-name prefixes/names we never want, mirroring Moonlight's blacklist. */
    private val BLOCKED_PREFIXES = listOf("omx.google.", "c2.android.", "avcdecoder", "omx.ffmpeg.")
    private val BLOCKED_EXACT = listOf("omx.qcom.video.decoder.hevcswvdec", "omx.sec.hevc.sw.dec")

    /**
     * Real SoC-vendor decoder prefixes we prefer over the generic AOSP fallback, covering the common
     * targets: Qualcomm Snapdragon and MediaTek (most phones + many TV boxes), Samsung Exynos (+
     * Google Tensor, whose decoder is `c2.exynos.*`), NVIDIA Tegra (Shield TV), Amlogic / Rockchip /
     * Realtek (TV boxes & smart TVs), and HiSilicon Kirin (older Huawei).
     */
    private val VENDOR_PREFIXES = listOf(
        "omx.qcom", "c2.qti",
        "omx.mtk", "c2.mtk",
        "omx.exynos", "c2.exynos",
        "omx.nvidia", "c2.nvidia",
        "omx.amlogic", "c2.amlogic",
        "omx.rk", "c2.rk",
        "omx.realtek", "c2.realtek",
        "omx.hisi", "c2.hisi",
    )

    /**
     * Pick the best decoder for [mime] (`"video/hevc"` / `"video/avc"` / `"video/av01"`), or `null`
     * to let the platform resolve its default. Enumerates once — call at stream start.
     */
    /**
     * The `quic::CODEC_*` bitfield of codecs this device can decode, advertised in the Hello so the
     * host never emits a codec the decode loop can't open: H.264 (1) and HEVC (2) always (universal
     * on Android hardware), plus AV1 (4) only when [pickDecoder] finds a real (hardware, non-blocked)
     * `video/av01` decoder. Enumerates `MediaCodecList` — call at connect time, not per frame.
     */
    fun decodableCodecBits(): Int =
        1 or 2 or (if (pickDecoder("video/av01") != null) 4 else 0)

    /**
     * Whether EVERY decoder this device would use tolerates multi-slice access units — the
     * `VIDEO_CAP_MULTI_SLICE` advertisement (decoder truth, per the 0.17.0 field regression:
     * Amlogic HEVC decoders wedge the whole DEVICE on multi-slice frames — Chromecast with
     * Google TV, onn 4K — which is why Moonlight requests single-slice for every hardware
     * decoder). We advertise per-family instead: tolerant unless the pick for ANY advertised
     * codec is an Amlogic decoder or can't be inspected (a null pick = the platform default —
     * uninspectable, so conservatively single-slice). Probed once at connect time; the host
     * defaults to >1 slice only toward clients that set the bit.
     */
    fun multiSliceTolerant(): Boolean {
        val mimes = buildList {
            add("video/avc")
            add("video/hevc")
            if (decodableCodecBits() and 4 != 0) add("video/av01")
        }
        return mimes.all { mime ->
            val name = pickDecoder(mime)?.name?.lowercase() ?: return@all false
            !name.startsWith("omx.amlogic") && !name.startsWith("c2.amlogic")
        }
    }

    /**
     * True when EVERY decoder this device would stream with supports partial-frame input
     * (MediaCodec `FEATURE_PartialFrame`): the client may then opt into slice-progressive
     * delivery and feed slices with `BUFFER_FLAG_PARTIAL_FRAME` ahead of the AU's tail.
     * Codec selection happens after connect, so all advertised mimes must qualify — the same
     * conservative shape as [multiSliceTolerant] (an unprobeable pick disqualifies).
     */
    fun partialFrameCapable(): Boolean {
        val mimes = buildList {
            add("video/avc")
            add("video/hevc")
            if (decodableCodecBits() and 4 != 0) add("video/av01")
        }
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return false
        return mimes.all { mime ->
            val pick = pickDecoder(mime)?.name ?: return@all false
            val info = infos.firstOrNull { it.name == pick } ?: return@all false
            runCatching {
                info.getCapabilitiesForType(mime)
                    .isFeatureSupported(CodecCapabilities.FEATURE_PartialFrame)
            }.getOrDefault(false)
        }
    }

    /**
     * One-line per-mime probe readout for the connect log (`adb logcat -s pf.caps`): which
     * decoder each advertised mime resolves to and whether it declares `FEATURE_PartialFrame` —
     * the P2 slice-pipeline gate that is otherwise invisible until a stream behaves differently.
     */
    fun capsReport(): String {
        val mimes = buildList {
            add("video/avc")
            add("video/hevc")
            if (decodableCodecBits() and 4 != 0) add("video/av01")
        }
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return "codec list unavailable"
        return mimes.joinToString("  ") { mime ->
            val pick = pickDecoder(mime)?.name
            val partial = pick?.let { p ->
                infos.firstOrNull { it.name == p }?.let { info ->
                    runCatching {
                        info.getCapabilitiesForType(mime)
                            .isFeatureSupported(CodecCapabilities.FEATURE_PartialFrame)
                    }.getOrNull()
                }
            }
            "${mime.removePrefix("video/")}=${pick ?: "platform-default"}" +
                " partialFrame=${partial ?: "?"}"
        }
    }

    fun pickDecoder(mime: String): DecoderChoice? {
        if (mime.isEmpty()) return null
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return null

        var bestName: String? = null
        var bestLowLatency = false
        var bestScore = Int.MIN_VALUE
        for (info in infos) {
            if (info.isEncoder) continue
            val name = info.name
            val lower = name.lowercase()
            if (BLOCKED_PREFIXES.any { lower.startsWith(it) } || lower in BLOCKED_EXACT) continue
            // Never a secure decoder: `.secure` names are the DRM-pipeline twins of the real
            // decoder and require a secure surface — configuring one for a clear stream fails (or
            // renders black). The plain twin is also in the list, so drop rather than rank
            // (a `.secure` twin can otherwise OUT-score its plain sibling when only it advertises
            // FEATURE_LowLatency). Moonlight filters the same way.
            if (lower.endsWith(".secure")) continue
            val caps = runCatching { info.getCapabilitiesForType(mime) }.getOrNull() ?: continue
            val secureRequired = runCatching {
                caps.isFeatureRequired(CodecCapabilities.FEATURE_SecurePlayback)
            }.getOrDefault(false)
            if (secureRequired) continue

            val hardware = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                info.isHardwareAccelerated
            } else {
                // Pre-Q heuristic: the software decoders are the ones we can name (already blocked
                // above), so anything surviving the blacklist is treated as hardware.
                true
            }
            val lowLatency = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                runCatching { caps.isFeatureSupported(CodecCapabilities.FEATURE_LowLatency) }
                    .getOrDefault(false)
            val vendor = VENDOR_PREFIXES.any { lower.startsWith(it) }

            val score = (if (hardware) 100 else 0) +
                (if (vendor) 40 else 0) +
                (if (lowLatency) 20 else 0)
            if (score > bestScore) {
                bestScore = score
                bestName = name
                bestLowLatency = lowLatency
            }
        }
        return bestName?.let { DecoderChoice(it, bestLowLatency) }
    }
}
