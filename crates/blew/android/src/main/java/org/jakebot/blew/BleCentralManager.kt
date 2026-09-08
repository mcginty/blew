package org.jakebot.blew

import android.annotation.SuppressLint
import android.bluetooth.*
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.ParcelUuid
import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Singleton managing the Android BLE central role (scanner + GATT client).
 *
 * Kotlin methods are called from Rust via JNI. Android BLE callbacks are
 * forwarded to Rust via [external fun] JNI hooks.
 *
 * ## GATT operation serialization
 *
 * Android's [BluetoothGatt] allows only one in-flight operation at a time
 * (read, write, descriptor write, discover services, request MTU). Each device
 * gets its own [GattOperationQueue] that serializes operations for that device,
 * with per-operation timeouts to guard against firmware bugs where callbacks
 * never fire.
 */
@SuppressLint("MissingPermission")
object BleCentralManager {
    private const val TAG = "BleCentralManager"

    // Status codes returned to Rust via JNI.
    private const val STATUS_SUCCESS = 0
    private const val STATUS_NOT_CONNECTED = 1
    private const val STATUS_CHAR_NOT_FOUND = 2
    private const val STATUS_GATT_BUSY = 3
    private const val STATUS_GATT_FAILED = 4

    /** [forceClose] generation meaning "whichever attempt is live". */
    private const val ANY_GENERATION = 0

    private var context: Context? = null
    private var bluetoothManager: BluetoothManager? = null
    private var adapter: BluetoothAdapter? = null

    /**
     * One connection attempt: the exact [BluetoothGatt] client created for it,
     * plus the generation Rust assigned.
     *
     * Android identifies a GATT callback only by device address, so a retired
     * attempt's late callback is otherwise indistinguishable from the live
     * one's. [gatt] is recorded as soon as `connectGatt()` returns rather than
     * on `STATE_CONNECTED`, because a timeout before the connection completes
     * still has to close that exact client — an unowned one leaks a clientIf
     * slot, and Android caps those at around seven.
     */
    private class Attempt(
        val addr: String,
        val generation: Int,
    ) {
        @Volatile var gatt: BluetoothGatt? = null

        /** Set on `STATE_CONNECTED`. Gates the GATT operations. */
        @Volatile var connected = false

        /**
         * Set when the attempt is retired. Covers the window where the attempt
         * exists but [gatt] does not, so whichever side loses that race still
         * closes the handle.
         */
        @Volatile var abandoned = false

        /** Ensures the client is closed at most once. */
        val released = AtomicBoolean(false)
    }

    // The live connection attempt per device address.
    private val attempts = ConcurrentHashMap<String, Attempt>()

    /** Guards [attempts] transitions and the per-device tables cleared with them. */
    private val connectLock = Any()

    // Per-device MTU (default 23 until negotiated).
    private val mtuMap = ConcurrentHashMap<String, Int>()

    // Per-device GATT operation queues.
    private val gattQueues = ConcurrentHashMap<String, GattOperationQueue>()

    // Nonces for in-flight GATT operations. Callbacks consume these before
    // completing the queue or notifying Rust so stale callbacks are ignored.
    private val pendingNonces = ConcurrentHashMap<String, Long>()

    // Device addresses with a write-without-response already completed from the
    // kick lambda. onCharacteristicWrite may still fire on some devices; the
    // entry here tells the callback to skip completeCurrent and the native
    // notification (the coroutine has already delivered both).
    private val noResponseHandled = ConcurrentHashMap<String, Boolean>()

    // Coroutine scope for launching GATT operation coroutines.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    // ── L2CAP state ──
    private val l2cap =
        L2capSocketManager(
            tag = TAG,
            onData = { socketId, data -> nativeOnL2capChannelData(socketId, data) },
            onClosed = { socketId, error -> nativeOnL2capChannelClosed(socketId, error) },
        )

    // ── JNI hooks (Kotlin → Rust) ──

    @JvmStatic
    external fun nativeOnDeviceDiscovered(
        deviceAddr: String,
        deviceName: String?,
        rssi: Int,
        serviceUuids: String,
        manufacturerData: String,
        serviceData: String,
    )

    @JvmStatic
    external fun nativeOnConnectionStateChanged(
        deviceAddr: String,
        connected: Boolean,
        gattStatus: Int,
        generation: Int,
    )

    @JvmStatic
    external fun nativeOnServicesDiscovered(
        deviceAddr: String,
        servicesJson: String,
    )

    @JvmStatic
    external fun nativeOnCharacteristicRead(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
        status: Int,
    )

    @JvmStatic
    external fun nativeOnCharacteristicWrite(
        deviceAddr: String,
        charUuid: String,
        status: Int,
    )

    @JvmStatic
    external fun nativeOnCharacteristicChanged(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
    )

    @JvmStatic
    external fun nativeOnMtuChanged(
        deviceAddr: String,
        mtu: Int,
    )

    @JvmStatic
    external fun nativeOnAdapterStateChanged(powered: Boolean)

    // ── L2CAP JNI hooks ──

    @JvmStatic
    external fun nativeOnL2capChannelOpened(
        deviceAddr: String,
        socketId: Int,
        fromServer: Boolean,
    )

    @JvmStatic
    external fun nativeOnL2capChannelData(
        socketId: Int,
        data: ByteArray,
    )

    @JvmStatic
    external fun nativeOnL2capChannelClosed(
        socketId: Int,
        error: String?,
    )

    @JvmStatic
    external fun nativeOnL2capChannelError(
        deviceAddr: String,
        errorMessage: String,
    )

    private val adapterStateReceiver =
        object : BroadcastReceiver() {
            override fun onReceive(
                context: Context,
                intent: Intent,
            ) {
                if (intent.action == BluetoothAdapter.ACTION_STATE_CHANGED) {
                    val state = intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
                    when (state) {
                        BluetoothAdapter.STATE_ON -> nativeOnAdapterStateChanged(true)
                        BluetoothAdapter.STATE_OFF -> nativeOnAdapterStateChanged(false)
                    }
                }
            }
        }

    @Volatile
    private var receiverRegistered = false

    @JvmStatic
    fun init(ctx: Context) {
        context = ctx
        bluetoothManager = ctx.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        adapter = bluetoothManager?.adapter
        Log.d(TAG, "initialized, adapter=${adapter != null}")
        // Registering the same receiver twice delivers every adapter state
        // change twice. init() runs again whenever the host activity is
        // recreated -- a rotation or a dark-mode toggle is enough -- and
        // nothing ever unregisters, so the duplicates would accumulate.
        if (!receiverRegistered) {
            val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
            ctx.registerReceiver(adapterStateReceiver, filter)
            receiverRegistered = true
        }
    }

    // ── Per-device queue helper ──

    private fun queueFor(addr: String): GattOperationQueue = gattQueues.getOrPut(addr) { GattOperationQueue("gatt-$addr") }

    // ── Connection attempt ownership ──

    /** The GATT client for [addr], or null unless it is connected. */
    private fun connectedGatt(addr: String): BluetoothGatt? = attempts[addr]?.takeIf { it.connected }?.gatt

    /** Whether [attempt] still owns its address. */
    private fun isLive(attempt: Attempt): Boolean = attempts[attempt.addr] === attempt

    /**
     * Consume the pending nonce at [key] and complete [attempt]'s queue with
     * [value]. Returns false — meaning the caller should drop the callback —
     * when [attempt] has been superseded, or when no operation was waiting.
     *
     * Every GATT callback routes its bookkeeping through here. Android reports
     * these by address alone, so a retired attempt's late callback would
     * otherwise consume the nonce its replacement is waiting on and complete
     * that queue with a result belonging to a different connection. The check
     * and the mutation share [connectLock] with retirement, so a check that
     * passes cannot be followed by a mutation that lands on the replacement.
     */
    private fun <T> completeOp(
        attempt: Attempt,
        key: String,
        value: T,
    ): Boolean =
        synchronized(connectLock) {
            if (attempts[attempt.addr] !== attempt) {
                return@synchronized false
            }
            val nonce = pendingNonces.remove(key) ?: return@synchronized false
            gattQueues[attempt.addr]?.completeCurrent(nonce, value)
            true
        }

    /**
     * Drop [attempt] from the live slot and clear the per-device tables it
     * owns. Returns false when it had already been superseded, in which case
     * nothing is touched — that state belongs to a newer attempt now.
     */
    private fun retire(attempt: Attempt): Boolean =
        synchronized(connectLock) {
            if (attempts[attempt.addr] !== attempt) {
                return@synchronized false
            }
            attempts.remove(attempt.addr)
            attempt.abandoned = true
            mtuMap.remove(attempt.addr)
            noResponseHandled.remove(attempt.addr)
            pendingNonces.keys.removeIf { it.startsWith("${attempt.addr}:") }
            gattQueues.remove(attempt.addr)?.close(CancellationException("device ${attempt.addr} disconnected"))
            true
        }

    /**
     * Close [attempt]'s platform client, at most once.
     *
     * A no-op while [Attempt.gatt] is unset, and deliberately without
     * consuming the guard: the [openGatt] about to publish that handle sees
     * [Attempt.abandoned] and closes it instead, so it is never leaked.
     */
    private fun release(
        attempt: Attempt,
        disconnectFirst: Boolean,
    ) {
        val gatt = attempt.gatt ?: return
        if (!attempt.released.compareAndSet(false, true)) return
        if (disconnectFirst) {
            try {
                gatt.disconnect()
            } catch (e: Exception) {
                Log.w(TAG, "release: disconnect threw for ${attempt.addr}: ${e.message}")
            }
        }
        gatt.close()
    }

    // ── Scanning ──

    private var scanCallback: ScanCallback? = null

    @JvmStatic
    fun startScan(
        serviceUuids: Array<String>,
        lowPower: Boolean = false,
    ) {
        val scanner =
            adapter?.bluetoothLeScanner ?: run {
                Log.e(TAG, "scanner not available")
                return
            }

        stopScan()

        val filters =
            if (serviceUuids.isNotEmpty()) {
                serviceUuids.map { uuid ->
                    ScanFilter
                        .Builder()
                        .setServiceUuid(ParcelUuid(UUID.fromString(uuid)))
                        .build()
                }
            } else {
                null
            }

        val scanMode =
            if (lowPower) {
                ScanSettings.SCAN_MODE_LOW_POWER
            } else {
                ScanSettings.SCAN_MODE_LOW_LATENCY
            }
        val settings =
            ScanSettings
                .Builder()
                .setScanMode(scanMode)
                .build()

        scanCallback =
            object : ScanCallback() {
                override fun onScanResult(
                    callbackType: Int,
                    result: ScanResult,
                ) {
                    val device = result.device
                    val addr = device.address
                    val name = device.name
                    val rssi = result.rssi

                    val uuids =
                        result.scanRecord
                            ?.serviceUuids
                            ?.joinToString(",") { it.uuid.toString() }
                            ?: ""

                    // "<companyId>:<hex>" entries, comma separated, matching the
                    // comma-joined encoding already used for service UUIDs.
                    val manufacturerData =
                        result.scanRecord?.manufacturerSpecificData?.let { sparse ->
                            (0 until sparse.size()).joinToString(",") { i ->
                                "${sparse.keyAt(i)}:${sparse.valueAt(i).toHex()}"
                            }
                        } ?: ""

                    val serviceData =
                        result.scanRecord
                            ?.serviceData
                            ?.entries
                            ?.joinToString(",") { (uuid, bytes) -> "${uuid.uuid}:${bytes.toHex()}" }
                            ?: ""

                    nativeOnDeviceDiscovered(addr, name, rssi, uuids, manufacturerData, serviceData)
                }

                override fun onScanFailed(errorCode: Int) {
                    Log.e(TAG, "scan failed: errorCode=$errorCode")
                }
            }

        scanner.startScan(filters, settings, scanCallback)
        Log.d(TAG, "scan started (filters=${serviceUuids.size} UUIDs)")
    }

    @JvmStatic
    fun stopScan() {
        scanCallback?.let { cb ->
            adapter?.bluetoothLeScanner?.stopScan(cb)
            scanCallback = null
        }
    }

    // ── GATT callback ──

    /**
     * A [BluetoothGattCallback] bound to one [Attempt].
     *
     * Android hands a callback the device address it belongs to and nothing
     * else, so a callback shared across attempts cannot tell a retired
     * attempt's late report from the live one's. Binding the attempt in makes
     * the identity exact — and it is exact from the moment `connectGatt()` is
     * called, rather than from whenever the connecting thread gets around to
     * publishing the handle it returns.
     */
    private fun callbackFor(attempt: Attempt): BluetoothGattCallback =
        object : BluetoothGattCallback() {
            override fun onConnectionStateChange(
                gatt: BluetoothGatt,
                status: Int,
                newState: Int,
            ) {
                val addr = attempt.addr
                // The connecting thread may not have published the handle yet.
                // This is the same instance it is about to store, and the
                // teardown paths below need it now.
                attempt.gatt = gatt

                if (newState == BluetoothProfile.STATE_CONNECTED) {
                    if (!isLive(attempt)) {
                        // Retired before the connection came up. Release the
                        // client rather than leave it holding a clientIf slot.
                        Log.d(TAG, "connected on a retired attempt for $addr; closing")
                        release(attempt, disconnectFirst = true)
                        return
                    }
                    attempt.connected = true
                    // Capture the queue reference before launching so a racing
                    // disconnect (which removes the entry from gattQueues) can't
                    // cause this coroutine to create an orphaned queue.
                    val q = queueFor(addr)
                    scope.launch {
                        // Enqueue MTU request so other ops queue behind it per device.
                        val mtuResult =
                            q.enqueue<Int>(
                                name = "request-mtu",
                                timeoutMs = 5000L,
                                kick = {
                                    val nonce = q.currentNonce() ?: return@enqueue false
                                    val key = "$addr:mtu"
                                    pendingNonces[key] = nonce
                                    val started = gatt.requestMtu(512)
                                    if (!started) {
                                        pendingNonces.remove(key)
                                    }
                                    started
                                },
                            )
                        if (mtuResult.isFailure) {
                            Log.w(TAG, "MTU negotiation failed for $addr: ${mtuResult.exceptionOrNull()?.message}")
                        }
                        // The attempt can be retired while the MTU exchange is
                        // in flight. Reporting connected now would hand Rust a
                        // completion for a client that is already closed.
                        if (!isLive(attempt)) {
                            Log.d(TAG, "MTU completed for a retired attempt on $addr")
                            return@launch
                        }
                        nativeOnConnectionStateChanged(addr, true, 0, attempt.generation)
                    }
                } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                    val owned = retire(attempt)
                    // Status 133 is the Android BLE zombie signal. Flush the
                    // client-side service cache before close() so the next
                    // connectGatt() on this address starts with a clean slate.
                    if (status == 133) {
                        if (refreshGatt(gatt)) {
                            Log.d(TAG, "flushed GATT cache for $addr after status=133")
                        }
                    }
                    release(attempt, disconnectFirst = false)
                    if (!owned) {
                        // Superseded already, and whoever retired it reported
                        // the disconnect. Reporting again would name a
                        // generation that no longer owns the address.
                        Log.d(TAG, "ignoring disconnect for a retired attempt on $addr")
                        return
                    }
                    nativeOnConnectionStateChanged(addr, false, status, attempt.generation)
                }
            }

            override fun onMtuChanged(
                gatt: BluetoothGatt,
                mtu: Int,
                status: Int,
            ) {
                val addr = attempt.addr
                val negotiated =
                    synchronized(connectLock) {
                        if (attempts[addr] !== attempt) {
                            return@synchronized false
                        }
                        if (status == BluetoothGatt.GATT_SUCCESS) {
                            mtuMap[addr] = mtu
                        }
                        val nonce = pendingNonces.remove("$addr:mtu")
                        if (nonce != null) {
                            gattQueues[addr]?.completeCurrent<Int>(nonce, mtu)
                        }
                        status == BluetoothGatt.GATT_SUCCESS
                    }
                if (negotiated) {
                    nativeOnMtuChanged(addr, mtu)
                }
                // Do NOT call nativeOnConnectionStateChanged here; the coroutine in the CONNECTED branch does it.
            }

            override fun onServicesDiscovered(
                gatt: BluetoothGatt,
                status: Int,
            ) {
                val addr = attempt.addr
                if (!completeOp(attempt, "$addr:services", Unit)) return
                if (status == BluetoothGatt.GATT_SUCCESS) {
                    nativeOnServicesDiscovered(addr, servicesToJson(gatt.services))
                } else {
                    nativeOnServicesDiscovered(addr, "[]")
                }
            }

            override fun onCharacteristicRead(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                value: ByteArray,
                status: Int,
            ) {
                val addr = attempt.addr
                val charUuid = characteristic.uuid.toString()
                if (!completeOp(attempt, "$addr:read:$charUuid", Unit)) return
                nativeOnCharacteristicRead(addr, charUuid, value, status)
            }

            override fun onCharacteristicWrite(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                status: Int,
            ) {
                val addr = attempt.addr
                val handledByKick =
                    synchronized(connectLock) {
                        isLive(attempt) && noResponseHandled.remove(addr) != null
                    }
                if (handledByKick) {
                    // Kick lambda already completed the queue and fired the
                    // native callback for this no-response write.
                    return
                }
                val charUuid = characteristic.uuid.toString()
                if (!completeOp(attempt, "$addr:write:$charUuid", Unit)) return
                nativeOnCharacteristicWrite(
                    addr,
                    charUuid,
                    status,
                )
            }

            override fun onDescriptorWrite(
                gatt: BluetoothGatt,
                descriptor: BluetoothGattDescriptor,
                status: Int,
            ) {
                val addr = attempt.addr
                val charUuid = descriptor.characteristic.uuid.toString()
                completeOp(attempt, "$addr:cccd:$charUuid", Unit)
            }

            override fun onCharacteristicChanged(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                value: ByteArray,
            ) {
                // Notifications are passive; don't touch the queue. Still
                // gated: a retired attempt's notification would surface to the
                // app as data from the connection that replaced it.
                if (!isLive(attempt)) return
                nativeOnCharacteristicChanged(
                    attempt.addr,
                    characteristic.uuid.toString(),
                    value,
                )
            }
        }

    // ── Connection management ──

    /**
     * Begin a connection attempt to [deviceAddr], tagged with the [generation]
     * Rust assigned. Every callback and completion for this attempt carries
     * that generation back, so a retired attempt cannot be mistaken for the
     * one that replaced it.
     */
    @JvmStatic
    fun connect(
        deviceAddr: String,
        generation: Int,
    ) {
        val ctx =
            context ?: run {
                Log.e(TAG, "context not initialized")
                return
            }

        val attempt = Attempt(deviceAddr, generation)
        val stale =
            synchronized(connectLock) {
                val previous = attempts[deviceAddr]
                if (previous != null) {
                    retire(previous)
                }
                attempts[deviceAddr] = attempt
                previous
            }

        if (stale == null) {
            openGatt(ctx, attempt)
            return
        }

        // Close the stale client to avoid leaking clientIf slots. Android has
        // a limit of ~7 concurrent GATT clients. Flush its service cache and
        // give the stack ~300ms to release the client-IF before the next
        // connectGatt — back-to-back attempts on the same address can be
        // silently dropped on some vendors.
        stale.gatt?.let { refreshGatt(it) }
        release(stale, disconnectFirst = true)
        Log.d(TAG, "closed stale GATT for $deviceAddr")
        scope.launch {
            delay(300)
            openGatt(ctx, attempt)
        }
    }

    /**
     * Report [attempt] as failed, but only if it still owns its address. A
     * superseded attempt has nothing to report: the report would name a
     * generation Rust no longer has a waiter for, against an address that now
     * belongs to a live connection.
     */
    private fun failAttempt(attempt: Attempt) {
        if (!retire(attempt)) return
        nativeOnConnectionStateChanged(attempt.addr, false, 0, attempt.generation)
    }

    private fun openGatt(
        ctx: Context,
        attempt: Attempt,
    ) {
        val addr = attempt.addr
        if (attempt.abandoned) {
            // Retired during the delay before a reconnect. Creating a client
            // only to close it would take a clientIf slot for nothing.
            Log.d(TAG, "attempt for $addr abandoned before connectGatt")
            return
        }
        val device =
            adapter?.getRemoteDevice(addr) ?: run {
                Log.e(TAG, "could not get remote device $addr")
                failAttempt(attempt)
                return
            }
        // TRANSPORT_LE ensures we connect over BLE, not classic Bluetooth.
        val gatt = device.connectGatt(ctx, false, callbackFor(attempt), BluetoothDevice.TRANSPORT_LE)
        if (gatt == null) {
            Log.e(TAG, "connectGatt returned null for $addr")
            failAttempt(attempt)
            return
        }
        // Owned from creation, not from STATE_CONNECTED: a timeout before the
        // connection completes still has to close this exact client, and a
        // client nothing holds is one nothing can close.
        attempt.gatt = gatt
        if (attempt.abandoned) {
            // Retired while connectGatt was in flight, so whoever retired it
            // found no handle to close. Close it here instead.
            Log.d(TAG, "attempt for $addr abandoned during connectGatt; closing")
            release(attempt, disconnectFirst = true)
            return
        }
        Log.d(TAG, "connecting to $addr")
    }

    @JvmStatic
    fun disconnect(deviceAddr: String) {
        attempts[deviceAddr]?.gatt?.let { gatt ->
            gatt.disconnect()
            Log.d(TAG, "disconnecting from $deviceAddr")
        }
    }

    /**
     * Synchronously tear down the GATT handle for [deviceAddr] without waiting
     * for [BluetoothGattCallback.onConnectionStateChange]. Called from Rust
     * when the normal disconnect callback path cannot be trusted (connect
     * timeout, disconnect whose callback never arrived, status-133 zombie).
     *
     * [generation] names the attempt to tear down, so a timeout that has
     * already been superseded cannot close a newer attempt's client. Pass
     * [ANY_GENERATION] to close whichever attempt is live, for callers with no
     * particular attempt in mind.
     *
     * Flushes the client-side service cache with `refresh()` before closing
     * so the next connectGatt() starts clean, then emits a synthetic
     * [nativeOnConnectionStateChanged]`(addr, false, 0, generation)` so any
     * Rust-side state waiting on the disconnect callback unblocks. Nothing is
     * emitted when the named attempt was already gone: that report would carry
     * a generation which no longer owns the address.
     */
    @JvmStatic
    fun forceClose(
        deviceAddr: String,
        generation: Int,
    ) {
        val attempt =
            attempts[deviceAddr]?.takeIf {
                generation == ANY_GENERATION || it.generation == generation
            }
        if (attempt == null || !retire(attempt)) {
            // The named attempt is already gone, and whoever retired it
            // reported the disconnect. Reporting one here would name a
            // generation that no longer owns the address, and Rust would
            // apply it to whatever replaced it.
            Log.d(TAG, "forceClose: no live attempt for $deviceAddr (generation=$generation)")
            return
        }
        attempt.gatt?.let { refreshGatt(it) }
        release(attempt, disconnectFirst = true)
        Log.d(TAG, "forceClose: tore down GATT for $deviceAddr (generation=${attempt.generation})")
        nativeOnConnectionStateChanged(deviceAddr, false, 0, attempt.generation)
    }

    private fun refreshGatt(gatt: BluetoothGatt): Boolean =
        try {
            val method = gatt.javaClass.getMethod("refresh")
            method.invoke(gatt) as Boolean
        } catch (e: Exception) {
            Log.w(TAG, "refresh failed: ${e.message}")
            false
        }

    /**
     * Clear the GATT service cache for [deviceAddr] by invoking the hidden
     * `BluetoothGatt.refresh()` method via reflection. Returns false if no
     * active GATT handle exists or the reflective call throws. Used to
     * recover from stale cached service tables after peer reboots (status
     * 133 errors).
     */
    @JvmStatic
    fun refresh(deviceAddr: String): Boolean {
        val gatt = attempts[deviceAddr]?.gatt ?: return false
        return refreshGatt(gatt)
    }

    // ── GATT operations (serialized via per-device queue) ──

    @JvmStatic
    fun discoverServices(deviceAddr: String): Int {
        val gatt = connectedGatt(deviceAddr) ?: return STATUS_NOT_CONNECTED
        val q = queueFor(deviceAddr)
        scope.launch {
            val result =
                q.enqueue<Unit>(
                    name = "discover-services",
                    timeoutMs = 10000L,
                    kick = {
                        val nonce = q.currentNonce() ?: return@enqueue false
                        val key = "$deviceAddr:services"
                        pendingNonces[key] = nonce
                        val started = gatt.discoverServices()
                        if (!started) {
                            pendingNonces.remove(key)
                        }
                        started
                    },
                )
            if (result.isFailure) {
                Log.w(TAG, "discoverServices queue failed for $deviceAddr: ${result.exceptionOrNull()?.message}")
                nativeOnServicesDiscovered(deviceAddr, "[]")
            }
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun readCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = connectedGatt(deviceAddr) ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
        val q = queueFor(deviceAddr)
        scope.launch {
            val result =
                q.enqueue<Unit>(
                    name = "read-$charUuid",
                    timeoutMs = 5000L,
                    kick = {
                        val nonce = q.currentNonce() ?: return@enqueue false
                        val key = "$deviceAddr:read:$charUuid"
                        pendingNonces[key] = nonce
                        val started = gatt.readCharacteristic(char)
                        if (!started) {
                            pendingNonces.remove(key)
                        }
                        started
                    },
                )
            if (result.isFailure) {
                Log.w(TAG, "read $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                nativeOnCharacteristicRead(deviceAddr, charUuid, byteArrayOf(), BluetoothGatt.GATT_FAILURE)
            }
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun writeCharacteristic(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
        writeType: Int,
    ): Int {
        val gatt = connectedGatt(deviceAddr) ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
        val q = queueFor(deviceAddr)
        scope.launch {
            val result =
                q.enqueue<Int>(
                    name = "write-$charUuid",
                    timeoutMs = 5000L,
                    kick = {
                        val nonce = q.currentNonce() ?: return@enqueue false
                        val nonceKey = "$deviceAddr:write:$charUuid"
                        if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                            // Mark before the framework can fire onCharacteristicWrite.
                            noResponseHandled[deviceAddr] = true
                        } else {
                            pendingNonces[nonceKey] = nonce
                        }
                        val ret = gatt.writeCharacteristic(char, value, writeType)
                        if (ret != BluetoothStatusCodes.SUCCESS) {
                            noResponseHandled.remove(deviceAddr)
                            pendingNonces.remove(nonceKey)
                            return@enqueue false
                        }
                        if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                            // Don't wait for a callback the platform may not deliver.
                            q.completeCurrent<Int>(nonce, BluetoothGatt.GATT_SUCCESS)
                            noResponseHandled.remove(deviceAddr)
                        }
                        true
                    },
                )
            if (result.isFailure) {
                Log.w(TAG, "write $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                nativeOnCharacteristicWrite(deviceAddr, charUuid, BluetoothGatt.GATT_FAILURE)
            } else if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
                nativeOnCharacteristicWrite(deviceAddr, charUuid, BluetoothGatt.GATT_SUCCESS)
            }
            // For write-with-response, onCharacteristicWrite fires the native
            // callback after calling completeCurrent — don't duplicate here.
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun subscribeCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = connectedGatt(deviceAddr) ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

        if (!gatt.setCharacteristicNotification(char, true)) return STATUS_GATT_FAILED

        // Write to CCCD to enable notifications on the remote device.
        val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
        val descriptor = char.getDescriptor(cccdUuid) ?: return STATUS_CHAR_NOT_FOUND
        val q = queueFor(deviceAddr)
        scope.launch {
            val result =
                q.enqueue<Unit>(
                    name = "subscribe-cccd-$charUuid",
                    timeoutMs = 5000L,
                    kick = {
                        val nonce = q.currentNonce() ?: return@enqueue false
                        val key = "$deviceAddr:cccd:$charUuid"
                        pendingNonces[key] = nonce
                        val ret =
                            gatt.writeDescriptor(
                                descriptor,
                                BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE,
                            )
                        if (ret != BluetoothStatusCodes.SUCCESS) {
                            pendingNonces.remove(key)
                        }
                        ret == BluetoothStatusCodes.SUCCESS
                    },
                )
            if (result.isFailure) {
                Log.w(TAG, "subscribe $charUuid queue failed: ${result.exceptionOrNull()?.message}")
            }
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun unsubscribeCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = connectedGatt(deviceAddr) ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

        // Always disable local notification state first, even if the CCCD
        // write below fails. The remote side will eventually notice via timeout.
        gatt.setCharacteristicNotification(char, false)

        val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
        val descriptor = char.getDescriptor(cccdUuid)
        if (descriptor != null) {
            val q = queueFor(deviceAddr)
            scope.launch {
                val result =
                    q.enqueue<Unit>(
                        name = "unsubscribe-cccd-$charUuid",
                        timeoutMs = 5000L,
                        kick = {
                            val nonce = q.currentNonce() ?: return@enqueue false
                            val key = "$deviceAddr:cccd:$charUuid"
                            pendingNonces[key] = nonce
                            val ret =
                                gatt.writeDescriptor(
                                    descriptor,
                                    BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE,
                                )
                            if (ret != BluetoothStatusCodes.SUCCESS) {
                                pendingNonces.remove(key)
                            }
                            ret == BluetoothStatusCodes.SUCCESS
                        },
                    )
                if (result.isFailure) {
                    Log.w(TAG, "unsubscribe $charUuid queue failed: ${result.exceptionOrNull()?.message}")
                }
            }
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun isPowered(): Boolean = adapter?.isEnabled == true

    @JvmStatic
    fun getMtu(deviceAddr: String): Int = mtuMap[deviceAddr] ?: 23

    // ── L2CAP ──

    @JvmStatic
    fun openL2capChannel(
        deviceAddr: String,
        psm: Int,
    ) {
        if (android.os.Build.VERSION.SDK_INT < 29) {
            nativeOnL2capChannelError(deviceAddr, "L2CAP requires API 29+")
            return
        }

        val device =
            adapter?.getRemoteDevice(deviceAddr) ?: run {
                nativeOnL2capChannelError(deviceAddr, "device not found")
                return
            }

        // connect() and the read loop are both blocking; they belong on the IO
        // dispatcher rather than on a raw thread per channel.
        scope.launch(Dispatchers.IO) {
            try {
                val socket = device.createInsecureL2capChannel(psm)
                socket.connect()
                val socketId = l2cap.register(socket)
                nativeOnL2capChannelOpened(deviceAddr, socketId, false)
                l2cap.startReadLoop(socketId, deviceAddr, socket)
            } catch (e: Exception) {
                Log.e(TAG, "L2CAP connect failed: ${e.message}")
                nativeOnL2capChannelError(deviceAddr, e.message ?: "connect failed")
            }
        }
    }

    @JvmStatic
    fun writeL2cap(
        socketId: Int,
        data: ByteArray,
    ) = l2cap.write(socketId, data)

    @JvmStatic
    fun closeL2cap(socketId: Int) = l2cap.close(socketId)

    /** Set from `L2capConfig::read_chunk_size` so socket reads match the configured size. */
    @JvmStatic
    fun setL2capReadBufferSize(bytes: Int) {
        l2cap.readBufferSize = bytes
    }

    // ── Helpers ──

    private fun ByteArray.toHex(): String {
        val sb = StringBuilder(size * 2)
        for (b in this) sb.append("%02x".format(b))
        return sb.toString()
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
