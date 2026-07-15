package io.unom.punktfunk.kit

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.util.Log
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean

/**
 * BLE transport for a Steam Controller 2 paired directly with the device (no Puck). The standard
 * HID service (0x1812) is claimed by the OS (and would feed the pad through the ordinary input
 * stack in lizard-crippled form), so this talks Valve's vendor GATT service instead — the same
 * approach Steam itself uses on hosts without a dongle.
 *
 * GATT operations are serialized by a small state machine (connect → MTU → discover → subscribe
 * each notify char → lizard-off → ready); duplicate callbacks (the Android stack sometimes fires
 * `onMtuChanged` twice) are ignored. Notified state reports arrive with the report-id byte
 * stripped by the transport, so `0x45` (`ID_STATE_BLE`) is re-prepended for ≥40-byte payloads —
 * the wire then carries the same id-first framing as USB.
 *
 * Requires BLUETOOTH_CONNECT (the caller gates on it); connection priority is bumped to HIGH to
 * pull the connection interval from ~50 ms down to ~11 ms.
 */
@SuppressLint("MissingPermission")
class Sc2BleLink(
    private val context: Context,
    private val onReport: (report: ByteArray, len: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    private enum class State { IDLE, CONNECTING, MTU_REQUESTED, DISCOVERING, SUBSCRIBING, READY }

    private val manager = context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager

    private var gatt: BluetoothGatt? = null
    private var writeChar: BluetoothGattCharacteristic? = null
    private val pendingSubs = mutableListOf<BluetoothGattCharacteristic>()
    private var subsIndex = 0
    private val writeBusy = AtomicBoolean(false)
    private var lizardTicker: Thread? = null

    @Volatile private var state = State.IDLE

    /** Bonded devices that look like a Steam Controller (name heuristic — BLE exposes no PID here). */
    fun pairedControllers(): List<BluetoothDevice> = runCatching {
        manager.adapter?.bondedDevices.orEmpty().filter { dev ->
            val n = runCatching { dev.name }.getOrNull() ?: return@filter false
            NAME_HINTS.any { n.contains(it, ignoreCase = true) }
        }
    }.getOrDefault(emptyList())

    /** Connect to the bonded controller at [address]. Reports start flowing once READY. */
    fun start(address: String): Boolean {
        val adapter = manager.adapter ?: return false
        if (!adapter.isEnabled) return false
        val device = runCatching { adapter.getRemoteDevice(address) }.getOrNull() ?: return false
        state = State.CONNECTING
        gatt = device.connectGatt(context, false, callback, BluetoothDevice.TRANSPORT_LE)
        return true
    }

    /**
     * Replay one raw report from the host: output reports (rumble) ride WRITE_NO_RESPONSE so they
     * can't queue behind acks at the 25 Hz resend rate; feature reports (settings) use an acked
     * write. The report-id byte stays in the payload (the firmware's vendor-channel framing).
     */
    fun writeRaw(kind: Int, data: ByteArray) {
        if (state != State.READY || data.isEmpty()) return
        val g = gatt ?: return
        val ch = writeChar ?: return
        runCatching {
            ch.value = data
            ch.writeType = if (kind == 0) {
                BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
            } else {
                BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
            }
            g.writeCharacteristic(ch)
        }
    }

    private fun sendLizardOff() {
        if (state != State.READY) return
        val g = gatt ?: return
        val ch = writeChar ?: return
        if (!writeBusy.compareAndSet(false, true)) return // previous acked write still in flight
        runCatching {
            ch.value = Sc2Device.DISABLE_LIZARD
            ch.writeType = BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
            if (!g.writeCharacteristic(ch)) writeBusy.set(false)
        }.onFailure { writeBusy.set(false) }
    }

    /** Disconnect and stop the lizard ticker. Idempotent; does not fire [onClosed]. */
    fun stop() {
        lizardTicker?.interrupt()
        lizardTicker = null
        runCatching { gatt?.disconnect() }
        runCatching { gatt?.close() }
        gatt = null
        writeChar = null
        pendingSubs.clear()
        subsIndex = 0
        state = State.IDLE
    }

    private val callback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            when (newState) {
                BluetoothProfile.STATE_CONNECTED -> {
                    // ~11 ms connection interval instead of the ~50 ms default — input latency.
                    g.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH)
                    if (state == State.CONNECTING) {
                        state = State.MTU_REQUESTED
                        if (!g.requestMtu(DESIRED_MTU)) {
                            state = State.DISCOVERING
                            g.discoverServices()
                        }
                    }
                }
                BluetoothProfile.STATE_DISCONNECTED -> {
                    val wasLive = state != State.IDLE
                    runCatching { g.close() }
                    gatt = null
                    writeChar = null
                    pendingSubs.clear()
                    subsIndex = 0
                    state = State.IDLE
                    if (wasLive) onClosed()
                }
            }
        }

        override fun onMtuChanged(g: BluetoothGatt, mtu: Int, status: Int) {
            if (state != State.MTU_REQUESTED) return // fired twice on some stacks — act once
            state = State.DISCOVERING
            g.discoverServices()
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            if (state != State.DISCOVERING || status != BluetoothGatt.GATT_SUCCESS) return
            val valve = g.getService(VALVE_SERVICE) ?: run {
                Log.e(TAG, "Valve vendor service missing — not an SC2?")
                return
            }
            pendingSubs.clear()
            writeChar = null
            for (ch in valve.characteristics) {
                val short = shortUuid(ch.uuid) ?: continue
                val canNotify = ch.properties and BluetoothGattCharacteristic.PROPERTY_NOTIFY != 0
                val canWrite = ch.properties and (
                    BluetoothGattCharacteristic.PROPERTY_WRITE or
                        BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE
                    ) != 0
                if (canNotify && short in NOTIFY_LOW..NOTIFY_HIGH) pendingSubs.add(ch)
                if (canWrite && short in WRITE_LOW..WRITE_HIGH && writeChar == null) writeChar = ch
            }
            subsIndex = 0
            state = State.SUBSCRIBING
            subscribeNext(g)
        }

        override fun onDescriptorWrite(g: BluetoothGatt, d: BluetoothGattDescriptor, status: Int) {
            if (state == State.SUBSCRIBING) subscribeNext(g)
        }

        override fun onCharacteristicWrite(g: BluetoothGatt, ch: BluetoothGattCharacteristic, status: Int) {
            writeBusy.set(false)
        }

        override fun onCharacteristicChanged(g: BluetoothGatt, ch: BluetoothGattCharacteristic) {
            val data = ch.value ?: return
            // BLE strips the report-id prefix; restore 0x45 on state-sized payloads so the raw
            // wire framing matches USB. Short payloads (battery/status) pass through as-is.
            if (data.size >= 40) {
                val framed = ByteArray(data.size + 1)
                framed[0] = Sc2Device.ID_STATE_BLE.toByte()
                System.arraycopy(data, 0, framed, 1, data.size)
                onReport(framed, framed.size)
            } else {
                onReport(data, data.size)
            }
        }
    }

    private fun subscribeNext(g: BluetoothGatt) {
        if (subsIndex >= pendingSubs.size) {
            state = State.READY
            Log.i(TAG, "SC2 BLE link up (${pendingSubs.size} notify chars)")
            sendLizardOff()
            // The firmware watchdog re-enables lizard mode; refresh on SDL's cadence until the
            // host's Steam takes over via the raw plane (its writes land through writeRaw too).
            lizardTicker = Thread({
                while (state == State.READY) {
                    try {
                        Thread.sleep(Sc2Device.LIZARD_REFRESH_MS)
                    } catch (_: InterruptedException) {
                        return@Thread
                    }
                    sendLizardOff()
                }
            }, "pf-sc2-lizard").apply { isDaemon = true; start() }
            return
        }
        val ch = pendingSubs[subsIndex++]
        g.setCharacteristicNotification(ch, true)
        val cccd = ch.getDescriptor(CCCD) ?: return subscribeNext(g)
        cccd.value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
        if (!g.writeDescriptor(cccd)) subscribeNext(g) // lose this one, try the rest
    }

    /** The 32-bit short id of a Valve vendor UUID, or null for foreign UUIDs. */
    private fun shortUuid(uuid: UUID): Long? {
        val s = uuid.toString()
        if (!s.endsWith(VALVE_UUID_TAIL)) return null
        return s.substring(0, 8).toLongOrNull(16)
    }

    private companion object {
        const val TAG = "Sc2BleLink"

        val VALVE_SERVICE: UUID = UUID.fromString("100f6c32-1735-4313-b402-38567131e5f3")
        const val VALVE_UUID_TAIL = "-1735-4313-b402-38567131e5f3"
        const val NOTIFY_LOW = 0x100f6c75L
        const val NOTIFY_HIGH = 0x100f6c7aL
        const val WRITE_LOW = 0x100f6cb5L
        const val WRITE_HIGH = 0x100f6cbeL
        val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

        val NAME_HINTS = listOf("Steam Ctrl", "Steam Controller", "SteamController", "Valve")

        /** Enough for a state payload (45 B) + ATT header with margin. */
        const val DESIRED_MTU = 100
    }
}
