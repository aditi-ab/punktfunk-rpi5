package io.unom.punktfunk.kit

import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.InputDevice
import java.util.concurrent.ConcurrentHashMap

/**
 * Motion from a controller the Android input stack owns — the Bluetooth pads.
 *
 * Before this, the only motion sources on Android were the capture links: [DsCapture] (a Sony pad
 * claimed over USB, raw HID) and [Sc2Capture] (Steam Controller 2 passthrough). A DualSense, a
 * DualShock 4, a Switch Pro or an 8BitDo paired over BLUETOOTH is neither — it arrives as an
 * ordinary [InputDevice], its buttons and sticks work, and its gyro was silently dead. That is a
 * whole class of controller with no motion at all.
 *
 * Android 12 (API 31) exposes those sensors: [InputDevice.getSensorManager] hands back a
 * [android.hardware.SensorManager] scoped to that one controller, carrying the usual
 * TYPE_GYROSCOPE / TYPE_ACCELEROMETER. This class registers a listener per forwarded controller
 * that has a gyroscope, converts each sample to wire units, and sends it on that pad's wire index
 * through [GamepadRouter.deviceMotion]. Below API 31 nothing is registered and the class is inert —
 * those pads keep working, minus motion, exactly as they did.
 *
 * It follows [DeviceGyro] (the phone-gyro mirror) wherever the two solve the same problem:
 *  - samples ride ONE dedicated [HandlerThread] with batching disabled (`maxReportLatencyUs = 0`)
 *    — sensor batching would trade away the exact latency gyro aim exists to avoid, and the main
 *    thread is where Compose recomposition lives;
 *  - a feed torn down while its wire pad is still alive parks the rotation at zero first, because
 *    the host holds motion as STATE and re-emits it in every virtual-pad report: an angular
 *    velocity left behind reads as a pad rotating forever (the gyro sweep's "stale rate re-sent
 *    forever" finding).
 *
 * One writer per pad, three ways:
 *  1. A USB capture claims the physical device away from the input stack; [DsCapture.startUsb]
 *     calls [GamepadRouter.releaseDevice] at claim time, which closes the slot, which fires
 *     `onSlotClosed`, which lands on [onSlotClosed] here and unregisters. The claim also makes the
 *     controller's [InputDevice] vanish outright, so even a reopened slot would find nothing to
 *     register — but the explicit teardown is what makes the ordering deterministic instead of a
 *     race against the platform's own removal callback.
 *  2. The phone-gyro mirror stands down: registering flips
 *     [GamepadRouter.setDeviceHasSensorMotion], [GamepadRouter.padHasOwnMotion] reports it, and
 *     [DeviceGyro] re-reads that gate on every sample (sending its own zero park on the edge).
 *  3. Exactly one feed exists per deviceId — [onSlotOpened] is idempotent, and it is the only
 *     thing that ever constructs one.
 *
 * Frame: see [gyroToWire] — the mapping is straight through, and NOT yet verified on hardware.
 */
class PadSensors(private val router: GamepadRouter) {

    /** One controller's live sensor feed: its listener state and the accel it pairs with each
     *  rotation. Its arrays belong to the sensor thread; [stop] reads them only after the join. */
    private inner class Feed(private val deviceId: Int) : SensorEventListener {
        /** Latest converted accel, paired with each gyro send (the wire fuses both per sample).
         *  Starts at the host's neutral — 1 g on the up axis, NOT [0,0,0], which is free fall. */
        private val accel = intArrayOf(0, Gamepad.MOTION_ACCEL_LSB_PER_G, 0)
        private val gyro = IntArray(3)

        /** Whether any rotation has gone out on this pad — gates the park on teardown, so a pad
         *  that never sent motion is not handed a sample it did not earn. */
        @Volatile
        var wroteMotion = false
            private set

        override fun onSensorChanged(event: SensorEvent) {
            when (event.sensor.type) {
                Sensor.TYPE_ACCELEROMETER -> accelToWire(event.values, accel)
                Sensor.TYPE_GYROSCOPE -> {
                    gyroToWire(event.values, gyro)
                    // One line per controller per session, on the first sample that carries both
                    // planes: it is the cheapest possible version of the frame measurement
                    // [gyroToWire] asks for. Hold the pad flat and still while a stream starts and
                    // the accel triple says which slot gravity lands on — the one thing that
                    // settles whether the straight-through mapping is right.
                    if (!wroteMotion) {
                        Log.i(
                            TAG,
                            "controller $deviceId first motion sample: " +
                                "gyro ${gyro.joinToString()} accel ${accel.joinToString()}",
                        )
                    }
                    wroteMotion = true
                    router.deviceMotion(deviceId, gyro, accel)
                }
            }
        }

        override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}

        /** Zero rotation, last-known accel — "at rest", not free fall. */
        fun park() {
            gyro.fill(0)
            router.deviceMotion(deviceId, gyro, accel)
        }
    }

    /** deviceId → live feed. Concurrent: the main thread mutates it while the sensor thread is
     *  running (hot-plug, a capture link's claim). */
    private val feeds = ConcurrentHashMap<Int, Feed>()

    private val thread = HandlerThread("pf-pad-sensors")
    private var handler: Handler? = null

    /**
     * Start the sensor thread and attach to every controller the router already forwards — the
     * pads connected before the session opened, which will never fire a hot-plug callback.
     * Everything after that arrives through [onSlotOpened]. Main thread.
     */
    fun start() {
        if (!supported()) return
        thread.start()
        handler = Handler(thread.looper)
        for (deviceId in router.forwardedDevices()) onSlotOpened(deviceId)
    }

    /**
     * A slot opened for real controller [deviceId] — attach if it has a gyroscope of its own.
     * Idempotent, and a no-op before [start] or on a platform without the API. Main thread, from
     * [GamepadRouter.onSlotOpened].
     */
    fun onSlotOpened(deviceId: Int) {
        val h = handler ?: return
        if (feeds.containsKey(deviceId)) return
        // API 31+ only — getSensorManager does not exist below it. Re-checked here rather than
        // relying on start()'s gate, so the entry point is safe on its own terms.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val dev = InputDevice.getDevice(deviceId) ?: return
        // Declared non-null: a controller with no sensors gets an empty manager, not a null one.
        val sm = dev.sensorManager
        // A gyroscope is the entry price; the accelerometer alone does not buy a feed. The rotation
        // is what gyro aim is for, and an accel-only feed would send gravity while pinning rotation
        // at zero on a pad the phone-gyro mirror is otherwise entitled to speak for — precisely the
        // two-writers-on-one-pad fight this program has spent its day unpicking. Such a pad stays
        // on the mirror's terms instead, where at least the accel agrees with the gyro beside it.
        // Nothing found here is not proof the pad has no IMU. A DualSense's motion arrives on its
        // own evdev node, and whether InputReader merges that node onto the gamepad InputDevice
        // (shared descriptor) or leaves it standing alone is the platform's business, not ours —
        // and a standalone one is exactly what GamepadRouter.isForwardable filters out, so this
        // would never see it. Android 12's own controller-sensor documentation cites the DualShock
        // 4 and DualSense, which says the merge happens; it is not something this code can assert.
        // If a Bluetooth Sony pad ever turns up here with no gyroscope, THAT is the thing to check.
        val gyroSensor = sm.getDefaultSensor(Sensor.TYPE_GYROSCOPE) ?: return
        val feed = Feed(deviceId)
        feeds[deviceId] = feed
        // ~200 Hz requested, zero report latency: batching is poison for gyro aim, and 200 Hz is
        // what the framework grants an app without HIGH_SAMPLING_RATE_SENSORS anyway.
        sm.registerListener(feed, gyroSensor, DeviceGyro.SAMPLING_PERIOD_US, 0, h)
        sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)?.let {
            sm.registerListener(feed, it, DeviceGyro.SAMPLING_PERIOD_US, 0, h)
        }
        // The pad sources its own rotation from here on → the phone-gyro mirror stands down for it.
        router.setDeviceHasSensorMotion(deviceId, true)
        Log.i(TAG, "controller $deviceId (${dev.name}) has a gyro — forwarding its motion")
    }

    /**
     * The slot for [deviceId] closed — unplug, session teardown, or a capture link claiming the
     * device. Unregister and hand the pad back to the phone-gyro mirror. Main thread, from
     * [GamepadRouter.onSlotClosed].
     *
     * No park-at-zero here, on purpose: the router removed the slot BEFORE invoking the callback
     * and has already sent that pad's Remove, so the host tore the virtual pad down and there is no
     * latched rotation left to clear — while writing to a wire index that is free again would be
     * addressing whoever claims it next. [stop] is the case where the pad outlives the feed.
     */
    fun onSlotClosed(deviceId: Int) {
        unregister(deviceId)
        router.setDeviceHasSensorMotion(deviceId, false)
    }

    /**
     * Unregister every listener, join the sensor thread, then park at zero each pad that was
     * rotating. Call BEFORE the router is released and the session handle freed — the same
     * teardown ordering rule as the feedback poll threads and [DeviceGyro.stop]. The parks come
     * AFTER the join for two reasons: a sample still in flight would re-latch the rotation just
     * cleared, and the join is what publishes the sensor thread's writes to this one.
     */
    fun stop() {
        val parked = feeds.keys.toList().mapNotNull { id -> unregister(id)?.let { id to it } }
        for ((deviceId, _) in parked) router.setDeviceHasSensorMotion(deviceId, false)
        thread.quitSafely()
        runCatching { thread.join() }
        handler = null
        for ((_, feed) in parked) if (feed.wroteMotion) feed.park()
    }

    /**
     * Drop [deviceId]'s listeners, returning the feed that held them (null if there was none).
     * Safe for a controller that is already gone: the sensor manager is reached through the
     * [InputDevice], and a vanished device simply leaves nothing to unregister — the platform has
     * stopped calling the listener either way.
     */
    private fun unregister(deviceId: Int): Feed? {
        val feed = feeds.remove(deviceId) ?: return null
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            InputDevice.getDevice(deviceId)?.sensorManager?.unregisterListener(feed)
        }
        return feed
    }

    companion object {
        private const val TAG = "PadSensors"

        /** Whether this platform can read a controller's own sensors at all (API 31+). */
        fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S

        /**
         * One gyroscope sample (Android: rad/s) → the wire's three signed-16 components, in place.
         *
         * The axis frame is straight through, and that is now MEASURED rather than assumed.
         *
         * The wire is a unit passthrough into a virtual DualSense report, whose frame was measured
         * over raw HID on 2026-08-07: slot 0 = Right (pitch), slot 1 = Up (yaw), slot 2 = Backward
         * toward the player (roll), right-handed. Android hands a controller's own sensors over in
         * that same frame — which was the documented expectation, but the numbers pass through a
         * HID driver and InputFlinger's sensor mapper, either of which could have permuted or
         * negated without saying so.
         *
         * Verified 2026-08-07 end to end: a DualSense on Bluetooth to an Android phone, streaming
         * to a Linux host. This path's own first-sample log read `accel 0, 10000, 0` — exactly 1 g
         * on slot 1 — and at the far end `hid-playstation` published gravity as +0.991 g on ABS_Y
         * with every rotation driving its correctly-named axis (yaw→RY, pitch→RX, roll→RZ) and the
         * signs agreeing with gravity's independent witness on 95 of 100 rotating samples.
         *
         * So: no remap. If a future device disagrees, the remap belongs HERE with its own
         * expectations in `PadSensorsTest` — not spread across callers.
         */
        fun gyroToWire(values: FloatArray, out: IntArray) {
            for (i in 0..2) out[i] = Gamepad.motionGyroWire(values.getOrElse(i) { 0f })
        }

        /**
         * One accelerometer sample (Android: m/s², specific force) → the wire's three signed-16
         * components, in place. Same measured frame as [gyroToWire] and the same straight-through
         * mapping; the sign needs no flip, because Android and the DualSense report agree that the
         * axis pointing up reads +1 g at rest (see [Gamepad.motionAccelWire]) — which is precisely
         * what the on-glass run read back, `accel 0, 10000, 0` with the pad lying flat.
         */
        fun accelToWire(values: FloatArray, out: IntArray) {
            for (i in 0..2) out[i] = Gamepad.motionAccelWire(values.getOrElse(i) { 0f })
        }
    }
}
