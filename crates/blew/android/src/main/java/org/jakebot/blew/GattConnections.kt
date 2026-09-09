package org.jakebot.blew

import android.annotation.SuppressLint
import android.bluetooth.*
import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.util.UUID

internal fun interface GattFactory {
    fun open(
        addr: String,
        callback: BluetoothGattCallback,
    ): BluetoothGatt?
}

/**
 * The monitor orders admission, callbacks, operation kicks and retirement, including
 * delivery to Rust. Rust must never hold its lifecycle lock across a JNI call.
 * The factory runs outside the monitor: timeout can retire an unpublished client.
 */
@SuppressLint("MissingPermission")
internal class GattConnections(
    private val factory: GattFactory,
    private val events: GattEvents,
    private val scope: CoroutineScope,
) {
    private companion object {
        const val TAG = "GattConnections"
        const val STATUS_SUCCESS = 0
        const val STATUS_NOT_CONNECTED = 1
        const val STATUS_CHAR_NOT_FOUND = 2
        const val STATUS_GATT_FAILED = 4
    }

    private class Attempt(
        val addr: String,
        val generation: Int,
        scope: CoroutineScope,
    ) {
        var gatt: BluetoothGatt? = null
        var connected = false
        var disconnecting = false
        var released = false
        var mtu = 23
        val queue = GattOperationQueue("gatt-$addr-$generation", scope.coroutineContext)
        val pendingNonces = mutableMapOf<String, Long>()
        var noResponseHandled = false
    }

    private val lock = Any()
    private val attempts = mutableMapOf<String, Attempt>()

    // Also records cancellation before JNI connect dispatch. One watermark per address,
    // not an ever-growing collection of retired attempts; subtraction handles wraparound.
    private val generations = mutableMapOf<String, Int>()

    private fun connectedAttempt(
        addr: String,
        generation: Int,
    ): Attempt? = attempts[addr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }

    private fun isLive(attempt: Attempt): Boolean = attempts[attempt.addr] === attempt

    private fun retire(attempt: Attempt): Boolean {
        if (!isLive(attempt)) return false
        attempts.remove(attempt.addr)
        attempt.queue.close(CancellationException("device ${attempt.addr} disconnected"))
        attempt.pendingNonces.clear()
        return true
    }

    private fun release(
        attempt: Attempt,
        disconnectFirst: Boolean,
    ) {
        val gatt = attempt.gatt ?: return
        if (attempt.released) return
        attempt.released = true
        if (disconnectFirst) {
            refreshGatt(gatt)
            try {
                gatt.disconnect()
            } catch (e: Exception) {
                Log.w(TAG, "disconnect failed for ${attempt.addr}: ${e.message}")
            }
        }
        gatt.close()
    }

    fun connect(
        deviceAddr: String,
        generation: Int,
    ) {
        val attempt: Attempt
        val reconnect: Boolean
        synchronized(lock) {
            val previousGeneration = generations[deviceAddr]
            if (previousGeneration != null && generation - previousGeneration <= 0) return
            generations[deviceAddr] = generation
            val previous = attempts[deviceAddr]
            reconnect = previous != null
            if (previous != null) {
                retire(previous)
                release(previous, disconnectFirst = true)
            }
            attempt = Attempt(deviceAddr, generation, scope)
            attempts[deviceAddr] = attempt
        }
        if (reconnect) {
            scope.launch {
                delay(300)
                openGatt(attempt)
            }
        } else {
            openGatt(attempt)
        }
    }

    private fun openGatt(attempt: Attempt) {
        synchronized(lock) { if (!isLive(attempt)) return }
        val gatt =
            try {
                factory.open(attempt.addr, callbackFor(attempt))
            } catch (e: Exception) {
                Log.w(TAG, "connectGatt failed for ${attempt.addr}: ${e.message}")
                null
            }
        synchronized(lock) {
            if (gatt != null) attempt.gatt = gatt
            if (!isLive(attempt)) {
                release(attempt, disconnectFirst = true)
            } else if (gatt == null) {
                retire(attempt)
                release(attempt, disconnectFirst = true)
                events.onConnectionStateChanged(attempt.addr, attempt.generation, false, 0)
            } else if (attempt.disconnecting) {
                gatt.disconnect()
            }
        }
    }

    fun disconnect(
        deviceAddr: String,
        generation: Int,
    ) {
        synchronized(lock) {
            val attempt = attempts[deviceAddr]?.takeIf { it.generation == generation }
            if (attempt == null) {
                forceClose(deviceAddr, generation)
                return
            }
            attempt.disconnecting = true
            attempt.gatt?.disconnect()
        }
    }

    fun forceClose(
        deviceAddr: String,
        generation: Int,
    ) {
        synchronized(lock) {
            val previous = generations[deviceAddr]
            if (previous == null || generation - previous > 0) generations[deviceAddr] = generation
            val attempt = attempts[deviceAddr]?.takeIf { it.generation == generation } ?: return
            retire(attempt)
            release(attempt, disconnectFirst = true)
            events.onConnectionStateChanged(deviceAddr, generation, false, 0)
        }
    }

    fun refresh(deviceAddr: String): Boolean =
        synchronized(lock) {
            attempts[deviceAddr]?.gatt?.let { refreshGatt(it) } ?: false
        }

    private fun refreshGatt(gatt: BluetoothGatt): Boolean =
        try {
            gatt.javaClass.getMethod("refresh").invoke(gatt) as Boolean
        } catch (e: Exception) {
            Log.w(TAG, "refresh failed: ${e.message}")
            false
        }

    fun getMtu(deviceAddr: String): Int = synchronized(lock) { attempts[deviceAddr]?.mtu ?: 23 }

    private fun <T> completeOp(
        attempt: Attempt,
        key: String,
        value: T,
    ): Boolean {
        if (!isLive(attempt)) return false
        val nonce = attempt.pendingNonces.remove(key) ?: return false
        attempt.queue.completeCurrent(nonce, value)
        return true
    }

    private fun callbackFor(attempt: Attempt): BluetoothGattCallback =
        object : BluetoothGattCallback() {
            override fun onConnectionStateChange(
                gatt: BluetoothGatt,
                status: Int,
                newState: Int,
            ) {
                synchronized(lock) {
                    attempt.gatt = gatt
                    if (!isLive(attempt)) {
                        release(attempt, disconnectFirst = newState == BluetoothProfile.STATE_CONNECTED)
                        return
                    }
                    if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                        retire(attempt)
                        if (status == 133) refreshGatt(gatt)
                        release(attempt, disconnectFirst = false)
                        events.onConnectionStateChanged(attempt.addr, attempt.generation, false, status)
                    } else if (newState == BluetoothProfile.STATE_CONNECTED && !attempt.connected) {
                        if (attempt.disconnecting) {
                            gatt.disconnect()
                            return
                        }
                        attempt.connected = true
                        val q = attempt.queue
                        scope.launch {
                            q.enqueue<Int>("request-mtu", 5000L, kick = {
                                synchronized(lock) {
                                    if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                    val nonce = q.currentNonce() ?: return@enqueue false
                                    attempt.pendingNonces["${attempt.addr}:mtu"] = nonce
                                    gatt.requestMtu(512)
                                }
                            })
                            synchronized(lock) {
                                if (!isLive(attempt) || attempt.disconnecting) return@launch
                                events.onConnectionStateChanged(attempt.addr, attempt.generation, true, 0)
                            }
                        }
                    }
                }
            }

            override fun onMtuChanged(
                gatt: BluetoothGatt,
                mtu: Int,
                status: Int,
            ) {
                synchronized(lock) {
                    if (!isLive(attempt)) return
                    if (status == BluetoothGatt.GATT_SUCCESS) {
                        attempt.mtu = mtu
                        events.onMtuChanged(attempt.addr, attempt.generation, mtu)
                    }
                    completeOp(attempt, "${attempt.addr}:mtu", mtu)
                }
            }

            override fun onServicesDiscovered(
                gatt: BluetoothGatt,
                status: Int,
            ) {
                synchronized(lock) {
                    if (!completeOp(attempt, "${attempt.addr}:services", Unit)) return
                    events.onServicesDiscovered(
                        attempt.addr,
                        attempt.generation,
                        if (status == BluetoothGatt.GATT_SUCCESS) servicesToJson(gatt.services) else "[]",
                    )
                }
            }

            override fun onCharacteristicRead(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                value: ByteArray,
                status: Int,
            ) {
                synchronized(lock) {
                    val uuid = characteristic.uuid.toString()
                    if (!completeOp(attempt, "${attempt.addr}:read:$uuid", Unit)) return
                    events.onCharacteristicRead(attempt.addr, attempt.generation, uuid, value, status)
                }
            }

            override fun onCharacteristicWrite(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                status: Int,
            ) {
                synchronized(lock) {
                    if (!isLive(attempt)) return
                    if (attempt.noResponseHandled) {
                        attempt.noResponseHandled = false
                        return
                    }
                    val uuid = characteristic.uuid.toString()
                    if (!completeOp(attempt, "${attempt.addr}:write:$uuid", status)) return
                    events.onCharacteristicWrite(attempt.addr, attempt.generation, uuid, status)
                }
            }

            override fun onDescriptorWrite(
                gatt: BluetoothGatt,
                descriptor: BluetoothGattDescriptor,
                status: Int,
            ) {
                synchronized(lock) {
                    completeOp(attempt, "${attempt.addr}:cccd:${descriptor.characteristic.uuid}", Unit)
                }
            }

            override fun onCharacteristicChanged(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                value: ByteArray,
            ) {
                synchronized(lock) {
                    if (!isLive(attempt)) return
                    events.onCharacteristicChanged(attempt.addr, attempt.generation, characteristic.uuid.toString(), value)
                }
            }
        }

    fun discoverServices(
        deviceAddr: String,
        generation: Int,
    ): Int {
        synchronized(lock) {
            val attempt =
                attempts[deviceAddr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }
                    ?: return STATUS_NOT_CONNECTED
            val gatt = attempt.gatt ?: return STATUS_NOT_CONNECTED
            val q = attempt.queue
            scope.launch {
                val result =
                    q.enqueue<Unit>(
                        name = "discover-services",
                        timeoutMs = 10000L,
                        kick = {
                            synchronized(lock) {
                                if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                val nonce = q.currentNonce() ?: return@enqueue false
                                val key = "$deviceAddr:services"
                                attempt.pendingNonces[key] = nonce
                                val started = gatt.discoverServices()
                                if (!started) {
                                    attempt.pendingNonces.remove(key)
                                }
                                started
                            }
                        },
                    )
                synchronized(lock) {
                    if (!isLive(attempt)) return@launch
                    if (result.isFailure) {
                        Log.w(TAG, "discoverServices queue failed for $deviceAddr: ${result.exceptionOrNull()?.message}")
                        events.onServicesDiscovered(deviceAddr, generation, "[]")
                    }
                }
            }
            return STATUS_SUCCESS
        }
    }

    fun readCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ): Int {
        synchronized(lock) {
            val attempt =
                attempts[deviceAddr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }
                    ?: return STATUS_NOT_CONNECTED
            val gatt = attempt.gatt ?: return STATUS_NOT_CONNECTED
            val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
            val q = attempt.queue
            scope.launch {
                val result =
                    q.enqueue<Unit>(
                        name = "read-$charUuid",
                        timeoutMs = 5000L,
                        kick = {
                            synchronized(lock) {
                                if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                val nonce = q.currentNonce() ?: return@enqueue false
                                val key = "$deviceAddr:read:$charUuid"
                                attempt.pendingNonces[key] = nonce
                                val started = gatt.readCharacteristic(char)
                                if (!started) {
                                    attempt.pendingNonces.remove(key)
                                }
                                started
                            }
                        },
                    )
                synchronized(lock) {
                    if (!isLive(attempt)) return@launch
                    if (result.isFailure) {
                        Log.w(TAG, "read $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                        events.onCharacteristicRead(deviceAddr, generation, charUuid, byteArrayOf(), BluetoothGatt.GATT_FAILURE)
                    }
                }
            }
            return STATUS_SUCCESS
        }
    }

    fun writeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
        writeType: Int,
    ): Int {
        synchronized(lock) {
            val attempt =
                attempts[deviceAddr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }
                    ?: return STATUS_NOT_CONNECTED
            val gatt = attempt.gatt ?: return STATUS_NOT_CONNECTED
            val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
            val q = attempt.queue
            scope.launch {
                val result =
                    q.enqueue<Int>(
                        name = "write-$charUuid",
                        timeoutMs = 5000L,
                        kick = {
                            synchronized(lock) {
                                if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                val nonce = q.currentNonce() ?: return@enqueue false
                                val nonceKey = "$deviceAddr:write:$charUuid"
                                if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                                    // Mark before the framework can fire onCharacteristicWrite.
                                    attempt.noResponseHandled = true
                                } else {
                                    attempt.pendingNonces[nonceKey] = nonce
                                }
                                val ret = gatt.writeCharacteristic(char, value, writeType)
                                if (ret != BluetoothStatusCodes.SUCCESS) {
                                    attempt.noResponseHandled = false
                                    attempt.pendingNonces.remove(nonceKey)
                                    return@enqueue false
                                }
                                if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                                    // Don't wait for a callback the platform may not deliver.
                                    q.completeCurrent<Int>(nonce, BluetoothGatt.GATT_SUCCESS)
                                    attempt.noResponseHandled = false
                                }
                                true
                            }
                        },
                    )
                synchronized(lock) {
                    if (!isLive(attempt)) return@launch
                    if (result.isFailure) {
                        Log.w(TAG, "write $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                        events.onCharacteristicWrite(deviceAddr, generation, charUuid, BluetoothGatt.GATT_FAILURE)
                    } else if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                        events.onCharacteristicWrite(deviceAddr, generation, charUuid, BluetoothGatt.GATT_SUCCESS)
                    }
                    // For write-with-response, onCharacteristicWrite fires the native
                    // callback after calling completeCurrent — don't duplicate here.
                }
            }
            return STATUS_SUCCESS
        }
    }

    fun subscribeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ): Int {
        synchronized(lock) {
            val attempt =
                attempts[deviceAddr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }
                    ?: return STATUS_NOT_CONNECTED
            val gatt = attempt.gatt ?: return STATUS_NOT_CONNECTED
            val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

            if (!gatt.setCharacteristicNotification(char, true)) return STATUS_GATT_FAILED

            // Write to CCCD to enable notifications on the remote device.
            val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
            val descriptor = char.getDescriptor(cccdUuid) ?: return STATUS_CHAR_NOT_FOUND
            val q = attempt.queue
            scope.launch {
                val result =
                    q.enqueue<Unit>(
                        name = "subscribe-cccd-$charUuid",
                        timeoutMs = 5000L,
                        kick = {
                            synchronized(lock) {
                                if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                val nonce = q.currentNonce() ?: return@enqueue false
                                val key = "$deviceAddr:cccd:$charUuid"
                                attempt.pendingNonces[key] = nonce
                                val ret =
                                    gatt.writeDescriptor(
                                        descriptor,
                                        BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE,
                                    )
                                if (ret != BluetoothStatusCodes.SUCCESS) {
                                    attempt.pendingNonces.remove(key)
                                }
                                ret == BluetoothStatusCodes.SUCCESS
                            }
                        },
                    )
                synchronized(lock) {
                    if (!isLive(attempt)) return@launch
                    if (result.isFailure) {
                        Log.w(TAG, "subscribe $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                    }
                }
            }
            return STATUS_SUCCESS
        }
    }

    fun unsubscribeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ): Int {
        synchronized(lock) {
            val attempt =
                attempts[deviceAddr]?.takeIf { it.generation == generation && it.connected && !it.disconnecting }
                    ?: return STATUS_NOT_CONNECTED
            val gatt = attempt.gatt ?: return STATUS_NOT_CONNECTED
            val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

            // Always disable local notification state first, even if the CCCD
            // write below fails. The remote side will eventually notice via timeout.
            gatt.setCharacteristicNotification(char, false)

            val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
            val descriptor = char.getDescriptor(cccdUuid)
            if (descriptor != null) {
                val q = attempt.queue
                scope.launch {
                    val result =
                        q.enqueue<Unit>(
                            name = "unsubscribe-cccd-$charUuid",
                            timeoutMs = 5000L,
                            kick = {
                                synchronized(lock) {
                                    if (!isLive(attempt) || attempt.disconnecting) return@enqueue false
                                    val nonce = q.currentNonce() ?: return@enqueue false
                                    val key = "$deviceAddr:cccd:$charUuid"
                                    attempt.pendingNonces[key] = nonce
                                    val ret =
                                        gatt.writeDescriptor(
                                            descriptor,
                                            BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE,
                                        )
                                    if (ret != BluetoothStatusCodes.SUCCESS) {
                                        attempt.pendingNonces.remove(key)
                                    }
                                    ret == BluetoothStatusCodes.SUCCESS
                                }
                            },
                        )
                    synchronized(lock) {
                        if (!isLive(attempt)) return@launch
                        if (result.isFailure) {
                            Log.w(TAG, "unsubscribe $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                        }
                    }
                }
            }
            return STATUS_SUCCESS
        }
    }

    private fun findCharacteristic(
        gatt: BluetoothGatt,
        charUuid: String,
    ): BluetoothGattCharacteristic? {
        val uuid = UUID.fromString(charUuid)
        for (service in gatt.services) {
            val char = service.getCharacteristic(uuid)
            if (char != null) return char
        }
        return null
    }

    /**
     * Serialize discovered services to a JSON array. Each service is:
     * {"uuid": "...", "characteristics": [{"uuid": "...", "properties": N}]}
     *
     * We build JSON manually to avoid pulling in a JSON library dependency.
     */
    private fun servicesToJson(services: List<BluetoothGattService>): String {
        val sb = StringBuilder("[")
        for ((i, svc) in services.withIndex()) {
            if (i > 0) sb.append(",")
            sb.append("{\"uuid\":\"").append(svc.uuid).append("\",\"characteristics\":[")
            for ((j, ch) in svc.characteristics.withIndex()) {
                if (j > 0) sb.append(",")
                sb
                    .append("{\"uuid\":\"")
                    .append(ch.uuid)
                    .append("\",\"properties\":")
                    .append(ch.properties)
                    .append("}")
            }
            sb.append("]}")
        }
        sb.append("]")
        return sb.toString()
    }
}

internal interface GattEvents {
    fun onConnectionStateChanged(
        deviceAddr: String,
        generation: Int,
        connected: Boolean,
        gattStatus: Int,
    )

    fun onServicesDiscovered(
        deviceAddr: String,
        generation: Int,
        servicesJson: String,
    )

    fun onCharacteristicRead(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
        status: Int,
    )

    fun onCharacteristicWrite(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        status: Int,
    )

    fun onCharacteristicChanged(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
    )

    fun onMtuChanged(
        deviceAddr: String,
        generation: Int,
        mtu: Int,
    )
}
