package org.jakebot.blew

import android.annotation.SuppressLint
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattService
import android.util.Log
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Opens a GATT server, or returns null when the stack won't hand one out.
 *
 * [generation] identifies the server being opened. Every callback it delivers
 * has to carry it back, which is the only way to tell a callback from a server
 * that has since been closed from one belonging to its replacement.
 */
internal fun interface GattServerFactory {
    fun open(generation: Int): BluetoothGattServer?
}

/**
 * Owns the `BluetoothGattServer` handle and the table of what is registered on it.
 *
 * The handle belongs to one instance of the Bluetooth stack. A power cycle
 * tears that instance down and drops the server's registration without
 * telling the holder, and a server kept across it accepts [addService] and
 * never reports the service added. [reset] closes and forgets the server so
 * the next [addService] opens a fresh one, and fails an add left waiting on
 * the old one rather than leaving it to time out.
 *
 * [addService] serializes on [addLock], because Android registers one service
 * at a time. It holds [lock] only to publish what it opened or registered,
 * never across the wait for a callback, so [reset] -- which arrives on the
 * main thread with the adapter broadcast -- is never held up by an add in
 * flight.
 */
@SuppressLint("MissingPermission")
internal class GattServerHost(
    private val factory: GattServerFactory,
    private val addTimeoutMs: Long = ADD_TIMEOUT_MS,
) {
    internal companion object {
        private const val TAG = "GattServerHost"

        /** How long [addService] waits for `onServiceAdded`. */
        const val ADD_TIMEOUT_MS = 5_000L

        /** The stack registered the service. */
        const val SERVICE_OK = 0

        /** There is no GATT server to register it on -- Bluetooth is off. */
        const val SERVICE_UNAVAILABLE = 1

        /** The stack refused the service, or reported it added with an error. */
        const val SERVICE_REJECTED = 2

        /** The stack never reported the service added. */
        const val SERVICE_TIMED_OUT = 3

        /**
         * An earlier service is still registering. The platform holds one
         * pending service per server, so nothing can be added until the
         * callback for that one arrives.
         */
        const val SERVICE_BUSY = 4

        /** Client Characteristic Configuration Descriptor -- the subscription toggle. */
        val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
    }

    /**
     * An [addService] the stack still owes a callback for, resolved by
     * `onServiceAdded` or by [reset].
     *
     * It outlives the caller that gave up on it, and holds the platform's
     * registration slot while it does; see [addService].
     */
    private class PendingAdd {
        val done = CountDownLatch(1)

        /** The stack's `onServiceAdded` status, or null when [reset] abandoned the add. */
        @Volatile
        var status: Int? = null

        fun complete(status: Int) {
            this.status = status
            done.countDown()
        }

        fun abandon() = done.countDown()
    }

    /** Guards [handle], [generation] and [pending]; never held across an add. */
    private val lock = Any()

    /** Serializes [addService]: the stack registers one service at a time. */
    private val addLock = Any()

    private var handle: BluetoothGattServer? = null

    /** Identifies [handle], so a closed server's callbacks can be told apart. */
    private var generation = 0

    /** The add the stack still owes a callback for, if any. */
    private var pending: PendingAdd? = null

    private val characteristics = ConcurrentHashMap<UUID, BluetoothGattCharacteristic>()
    private val staticValues = ConcurrentHashMap<UUID, ByteArray>()

    /** The live server, or null while Bluetooth is off. */
    fun server(): BluetoothGattServer? = synchronized(lock) { handle }

    /** The registered characteristic with [uuid], for notifications. */
    fun characteristic(uuid: UUID): BluetoothGattCharacteristic? = characteristics[uuid]

    /** The value a static characteristic is served from, if it has one. */
    fun staticValue(uuid: UUID): ByteArray? = staticValues[uuid]

    /**
     * Register [service] and wait for the stack to confirm it, returning a
     * `SERVICE_*` code.
     *
     * A `BluetoothGattServer` holds exactly one pending service, and
     * `addService` overwrites it with no guard
     * (`BluetoothGattServer.addService`, which is why its documentation says
     * not to add another service before the callback). The framework then
     * answers whatever is pending *now* when a registration completes: it
     * hands the completion its stored service, writes the completed
     * registration's handles onto that one, and clears the slot — so the next
     * service's own completion is dropped by the `mPendingService == null`
     * check. A timeout here is us giving up on the callback, not the stack
     * giving up the slot, so the slot stays held until the callback arrives or
     * [reset] retires the server, and an add that finds it held returns
     * [SERVICE_BUSY] without touching the platform.
     *
     * [chars] and [statics] join the table only once the stack has the
     * service, so an add that fails leaves nothing to notify on or to serve
     * reads from.
     */
    fun addService(
        service: BluetoothGattService,
        chars: Map<UUID, BluetoothGattCharacteristic>,
        statics: Map<UUID, ByteArray>,
    ): Int {
        synchronized(addLock) {
            val add = PendingAdd()
            val server =
                synchronized(lock) {
                    if (pending != null) return SERVICE_BUSY
                    val server = handle ?: open()
                    if (server == null) return SERVICE_UNAVAILABLE
                    // Registered before the platform call: the callback can
                    // arrive before it returns.
                    pending = add
                    server
                }
            if (!server.addService(service)) {
                // The call never reached the stack, so no callback is owed and
                // the slot is free again.
                synchronized(lock) { if (pending === add) pending = null }
                return SERVICE_REJECTED
            }
            if (!add.done.await(addTimeoutMs, TimeUnit.MILLISECONDS)) {
                Log.w(TAG, "the stack never reported ${service.uuid} added")
                return SERVICE_TIMED_OUT
            }
            val status = add.status ?: return SERVICE_UNAVAILABLE
            if (status != BluetoothGatt.GATT_SUCCESS) return SERVICE_REJECTED
            return publish(server, chars, statics)
        }
    }

    /**
     * Resolve the add `onServiceAdded` answers, if it is one of ours.
     *
     * [generation] is the server the callback came from, which the platform
     * callback carries no sign of on its own: one from the server [reset]
     * closed must not answer an add on its replacement.
     *
     * The callback's `BluetoothGattService` says nothing about *which* add it
     * answers -- the framework passes back whatever it holds as pending, not
     * the service the stack reported on -- so it is not used here. Since only
     * one add is ever outstanding per server, there is nothing to correlate:
     * the callback belongs to the add holding the slot. **Don't reintroduce
     * matching on the service**; it agrees with the add that is waiting in
     * exactly the case where the callback is someone else's.
     */
    fun onServiceAdded(
        generation: Int,
        status: Int,
    ) {
        val add =
            synchronized(lock) {
                if (generation != this.generation) {
                    Log.d(TAG, "ignoring onServiceAdded from closed server $generation")
                    return
                }
                pending.also { pending = null }
            }
        add?.complete(status)
    }

    /**
     * Close the server and forget everything registered on it.
     *
     * Called when the adapter powers off, which invalidates the handle;
     * [addService] opens a new one after this.
     */
    fun reset() {
        val closing: BluetoothGattServer?
        val abandoned: PendingAdd?
        synchronized(lock) {
            closing = handle
            handle = null
            // Retired here rather than when the next server opens, so the
            // closed one's generation is never the current one.
            generation += 1
            // The slot the add was holding belonged to the server being
            // closed; the new one brings its own, so freeing it here is what
            // lets the next add through.
            abandoned = pending
            pending = null
            characteristics.clear()
            staticValues.clear()
        }
        abandoned?.abandon()
        if (closing == null) return
        try {
            closing.close()
        } catch (e: Exception) {
            Log.w(TAG, "closing the GATT server failed: ${e.message}")
        }
    }

    /** Opens a server under [lock], so a [reset] closes what it published. */
    private fun open(): BluetoothGattServer? {
        val opening = generation + 1
        val server = factory.open(opening) ?: return null
        generation = opening
        handle = server
        return server
    }

    private fun publish(
        server: BluetoothGattServer,
        chars: Map<UUID, BluetoothGattCharacteristic>,
        statics: Map<UUID, ByteArray>,
    ): Int =
        synchronized(lock) {
            // [reset] ran while the stack was registering: the server that took
            // the service is gone, and so is the table it would have joined.
            if (handle !== server) return@synchronized SERVICE_UNAVAILABLE
            characteristics.putAll(chars)
            staticValues.putAll(statics)
            SERVICE_OK
        }
}
