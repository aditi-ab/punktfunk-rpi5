package io.unom.punktfunk.kit

/**
 * IMU liveness gate for the raw SC2 state-report feed — the client-side half of the policy
 * proven against real hardware on the bench (2026-06-08).
 *
 * The controller streams gyro/accel only after the host writes `SETTING_IMU_MODE` (reg 0x30);
 * until then the IMU block — including its leading u32 timestamp — is FROZEN at a stale non-zero
 * resting sample (byte-frozen across 600 frames in the 2026-06-08 capture). Forwarded verbatim,
 * that constant non-zero gyro reads to Steam's desktop config as a *constant* rotation and flies
 * the cursor. So: pass the IMU through only while its timestamp is advancing, and zero the whole
 * block (timestamp included) while frozen. Self-correcting, no hardcoded gyro-enable: on the
 * desktop the IMU stays off → frozen → zeros → calm cursor; a gyro game makes Steam send the
 * enable (feature `01 87 03 30 18 00`, replayed to the pad by the existing [Sc2Capture.onHidRaw]
 * raw-return path) → the timestamp starts ticking → live data flows.
 *
 * Gated shapes: `0x42` (USB state, 54 B wire) and `0x45` (BLE state, 46 B wire) — both are
 * `[report id][pack(1) TritonMTUNoQuat_t]`, so the IMU block (u32 timestamp + 3× i16 accel +
 * 3× i16 gyro) sits at wire offset 30 (struct offset 29 + the id byte) in both. `0x47` is
 * deliberately NOT gated: its layout diverges from byte 18 (inserted trackpad timestamp), no
 * capture pins its IMU offset down, and the Windows host driver drops that id anyway (it is not
 * in the wired descriptor).
 *
 * Single-threaded by contract: [apply] runs on the link thread; [reset] runs from the same
 * teardown paths that already touch the slot state (the [Sc2Capture] threading contract).
 */
class Sc2ImuGate {
    private var lastTs = 0
    private var haveTs = false
    private var stale = 0

    /** Re-arm (forget the timestamp history) — called on link drop / capture stop, so whatever
     *  connects next must re-prove its IMU live before the block passes through. */
    fun reset() {
        lastTs = 0
        haveTs = false
        stale = 0
    }

    /**
     * Gate [report] (its first [len] bytes) in place, before it is forwarded. Non-state ids and
     * reports too short to carry a full IMU block pass through untouched; a state report whose
     * IMU timestamp has not advanced for [STALE_LIMIT] consecutive frames — or that has no
     * history yet (unknown until it moves, so treated as frozen) — gets its IMU block zeroed.
     * A live stream tolerates short repeats (report rate can exceed the IMU sample rate).
     */
    fun apply(report: ByteArray, len: Int) {
        if (len < IMU_OFFSET + IMU_LEN) return // short/truncated: no full IMU block on board
        when (report[0].toInt() and 0xFF) {
            Sc2Device.ID_STATE, Sc2Device.ID_STATE_BLE -> {}
            else -> return
        }
        val ts = (report[IMU_OFFSET].toInt() and 0xFF) or
            ((report[IMU_OFFSET + 1].toInt() and 0xFF) shl 8) or
            ((report[IMU_OFFSET + 2].toInt() and 0xFF) shl 16) or
            ((report[IMU_OFFSET + 3].toInt() and 0xFF) shl 24)
        val live: Boolean
        if (!haveTs) {
            haveTs = true
            stale = STALE_LIMIT // unknown until it moves → treat as frozen
            live = false
        } else if (ts != lastTs) {
            stale = 0
            live = true
        } else {
            if (stale < STALE_LIMIT) stale++
            live = stale < STALE_LIMIT
        }
        lastTs = ts
        if (!live) report.fill(0, IMU_OFFSET, IMU_OFFSET + IMU_LEN)
    }

    companion object {
        /** Wire offset of `TritonMTUNoQuat_t.imu` — struct offset 29 + 1 report-id byte;
         *  identical in the 0x42 and 0x45 shapes (both carry the same pack(1) struct). */
        const val IMU_OFFSET = 30

        /** u32 timestamp + 3× i16 accel + 3× i16 gyro. */
        const val IMU_LEN = 16

        /** Unchanged-timestamp frames before declaring the IMU frozen (bench-tuned 2026-06-08:
         * three repeats still pass, the fourth freezes). */
        const val STALE_LIMIT = 4
    }
}
