package org.jakebot.blew

import android.annotation.SuppressLint
import android.bluetooth.*
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.BluetoothLeAdvertiser
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import android.os.ParcelUuid
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Singleton managing the Android BLE peripheral role (GATT server + advertiser).
 *
 * Kotlin methods are called from Rust via JNI. Callbacks from Android BLE are
 * forwarded to Rust via [external fun] JNI hooks.
 */
@SuppressLint("MissingPermission")
object BlePeripheralManager {
    private const val TAG = "BlePeripheralManager"

    /** startAdvertising handed the request to the stack. */
    const val ADVERTISE_OK = 0

    /** No advertiser — Bluetooth is off, or the radio cannot advertise. */
    const val ADVERTISE_UNAVAILABLE = 1

    /** An advertisement is already running or starting. */
    const val ADVERTISE_ALREADY = 2

    private var context: Context? = null
    private var bluetoothManager: BluetoothManager? = null

    /**
     * Hosts the blocking L2CAP accept loop. `BluetoothServerSocket.accept()`
     * has no async form, so it has to block something; a managed dispatcher is
     * a better host for that than a raw thread.
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private var gattServer: BluetoothGattServer? = null
    private var advertiser: BluetoothLeAdvertiser? = null

    // Track connected devices for notification delivery.
    private val connectedDevices = ConcurrentHashMap<String, BluetoothDevice>()

    // Track which (device, characteristic) pairs are subscribed for notifications.
    private val subscriptions = ConcurrentHashMap<String, MutableSet<UUID>>()

    // Map characteristic UUID -> BluetoothGattCharacteristic for notification sending.
    private val characteristics = ConcurrentHashMap<UUID, BluetoothGattCharacteristic>()

    // Static characteristic values — auto-responded on read, matching CoreBluetooth behaviour.
    private val staticValues = ConcurrentHashMap<UUID, ByteArray>()

    // Latch to serialize addService calls (Android requires waiting for onServiceAdded).
    @Volatile private var serviceAddedLatch: CountDownLatch? = null

    // Per-device semaphore to serialize notifyCharacteristicChanged calls.
    // Android's BluetoothGattServer only allows one in-flight notification per
    // device — subsequent calls before onNotificationSent are silently dropped.
    private val notifySemaphores = ConcurrentHashMap<String, java.util.concurrent.Semaphore>()

    private fun getNotifySemaphore(addr: String): java.util.concurrent.Semaphore =
        notifySemaphores.getOrPut(addr) { java.util.concurrent.Semaphore(1) }

    private fun acquireNotify(
        addr: String,
        timeoutMs: Long = 5000,
    ): Boolean = getNotifySemaphore(addr).tryAcquire(timeoutMs, TimeUnit.MILLISECONDS)

    private fun releaseNotify(addr: String) {
        notifySemaphores[addr]?.release()
    }

    // Rust-assigned id for the notify call currently in flight per device.
    // Stored on accept and echoed back through [nativeOnNotificationSent] so a
    // busy-retry can never resolve a newer call with an older callback. Safe
    // as a single slot per device: the semaphore above serializes sends.
    private val notifySeqByDevice = ConcurrentHashMap<String, Long>()

    // ── L2CAP state ──
    private val l2cap =
        L2capSocketManager(
            tag = TAG,
            onData = { socketId, data -> nativeOnL2capChannelData(socketId, data) },
            onClosed = { socketId, error -> nativeOnL2capChannelClosed(socketId, error) },
            startId = 100_000,
        )

    @Volatile private var l2capServerSocket: BluetoothServerSocket? = null

    // Serializes addService calls (Android requires waiting for onServiceAdded
    // before adding the next service).
    private val serviceAddLock = Any()

    @JvmStatic
    external fun nativeOnReadRequest(
        requestId: Int,
        deviceAddr: String,
        serviceUuid: String,
        charUuid: String,
        offset: Int,
    )

    @JvmStatic
    external fun nativeOnWriteRequest(
        requestId: Int,
        deviceAddr: String,
        serviceUuid: String,
        charUuid: String,
        offset: Int,
        value: ByteArray,
        responseNeeded: Boolean,
    )

    @JvmStatic
    external fun nativeOnSubscriptionChanged(
        deviceAddr: String,
        charUuid: String,
        subscribed: Boolean,
    )

    @JvmStatic
    external fun nativeOnConnectionStateChanged(
        deviceAddr: String,
        connected: Boolean,
    )

    @JvmStatic
    external fun nativeOnAdapterStateChanged(powered: Boolean)

    /**
     * Reports that a previously accepted [`notifyCharacteristic`] call reached
     * the stack's `onNotificationSent` callback. `seq` is the id Rust assigned
     * when the call was made; `status` is the BluetoothGatt status code.
     */
    @JvmStatic
    external fun nativeOnNotificationSent(
        deviceAddr: String,
        seq: Long,
        status: Int,
    )

    // ── L2CAP JNI hooks ──

    @JvmStatic
    external fun nativeOnL2capServerOpened(psm: Int)

    @JvmStatic
    external fun nativeOnL2capServerError(errorMessage: String)

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

    /** Async outcome of [startAdvertising], from the stack's AdvertiseCallback. */
    @JvmStatic
    external fun nativeOnAdvertisingResult(
        requestId: Int,
        success: Boolean,
        errorCode: Int,
    )

    @JvmStatic
    external fun nativeOnL2capChannelClosed(
        socketId: Int,
        error: String?,
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
        val adapter = bluetoothManager?.adapter
        // The advertiser is deliberately not cached here: getBluetoothLeAdvertiser
        // returns null while Bluetooth is off, and nothing refreshes a cached
        // null when it comes back on. It is resolved per startAdvertising call.
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

    private val gattCallback =
        object : BluetoothGattServerCallback() {
            override fun onServiceAdded(
                status: Int,
                service: BluetoothGattService?,
            ) {
                Log.d(TAG, "onServiceAdded status=$status uuid=${service?.uuid}")
                serviceAddedLatch?.countDown()
            }

            override fun onConnectionStateChange(
                device: BluetoothDevice,
                status: Int,
                newState: Int,
            ) {
                val addr = device.address
                if (newState == BluetoothProfile.STATE_CONNECTED) {
                    connectedDevices[addr] = device
                    nativeOnConnectionStateChanged(addr, true)
                } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                    connectedDevices.remove(addr)
                    subscriptions.remove(addr)
                    // Drain and remove the notify semaphore so a reconnect starts fresh.
                    notifySemaphores.remove(addr)?.drainPermits()
                    nativeOnConnectionStateChanged(addr, false)
                }
            }

            override fun onNotificationSent(
                device: BluetoothDevice,
                status: Int,
            ) {
                val addr = device.address
                releaseNotify(addr)
                notifySeqByDevice.remove(addr)?.let { seq ->
                    nativeOnNotificationSent(addr, seq, status)
                }
            }

            override fun onCharacteristicReadRequest(
                device: BluetoothDevice,
                requestId: Int,
                offset: Int,
                characteristic: BluetoothGattCharacteristic,
            ) {
                // Auto-respond for static characteristics (matches CoreBluetooth behaviour
                // where characteristics with a non-nil value are served by the framework).
                val staticValue = staticValues[characteristic.uuid]
                if (staticValue != null) {
                    if (offset > staticValue.size) {
                        gattServer?.sendResponse(
                            device,
                            requestId,
                            BluetoothGatt.GATT_INVALID_OFFSET,
                            offset,
                            null,
                        )
                        return
                    }
                    gattServer?.sendResponse(
                        device,
                        requestId,
                        BluetoothGatt.GATT_SUCCESS,
                        offset,
                        staticValue.copyOfRange(offset, staticValue.size),
                    )
                    return
                }

                nativeOnReadRequest(
                    requestId,
                    device.address,
                    characteristic.service.uuid.toString(),
                    characteristic.uuid.toString(),
                    offset,
                )
            }

            override fun onCharacteristicWriteRequest(
                device: BluetoothDevice,
                requestId: Int,
                characteristic: BluetoothGattCharacteristic,
                preparedWrite: Boolean,
                responseNeeded: Boolean,
                offset: Int,
                value: ByteArray?,
            ) {
                nativeOnWriteRequest(
                    requestId,
                    device.address,
                    characteristic.service.uuid.toString(),
                    characteristic.uuid.toString(),
                    offset,
                    value ?: ByteArray(0),
                    responseNeeded,
                )
            }

            override fun onDescriptorWriteRequest(
                device: BluetoothDevice,
                requestId: Int,
                descriptor: BluetoothGattDescriptor,
                preparedWrite: Boolean,
                responseNeeded: Boolean,
                offset: Int,
                value: ByteArray?,
            ) {
                // Client Characteristic Configuration Descriptor (0x2902) — subscription toggle.
                val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
                if (descriptor.uuid == cccdUuid) {
                    val charUuid = descriptor.characteristic.uuid
                    val addr = device.address
                    val subscribed = value != null && value.isNotEmpty() && value[0].toInt() != 0

                    if (subscribed) {
                        subscriptions.getOrPut(addr) { mutableSetOf() }.add(charUuid)
                    } else {
                        subscriptions[addr]?.remove(charUuid)
                    }

                    nativeOnSubscriptionChanged(addr, charUuid.toString(), subscribed)
                }

                if (responseNeeded) {
                    gattServer?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, null)
                }
            }
        }

    private fun ensureGattServer() {
        if (gattServer == null) {
            gattServer = bluetoothManager?.openGattServer(context, gattCallback)
        }
    }

    /**
     * Add a GATT service. Called from Rust via JNI.
     *
     * Parameters are kept flat to simplify JNI marshalling:
     * - serviceUuid: service UUID string
     * - charUuids: array of characteristic UUID strings
     * - charProperties: array of property bitflags (matching Android's BluetoothGattCharacteristic constants)
     * - charPermissions: array of permission bitflags
     * - charValues: array of initial values (empty byte arrays for dynamic characteristics)
     */
    @JvmStatic
    fun addService(
        serviceUuid: String,
        charUuids: Array<String>,
        charProperties: IntArray,
        charPermissions: IntArray,
        charValues: Array<ByteArray>,
    ) {
        synchronized(serviceAddLock) {
            ensureGattServer()

            val service =
                BluetoothGattService(
                    UUID.fromString(serviceUuid),
                    BluetoothGattService.SERVICE_TYPE_PRIMARY,
                )

            val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

            for (i in charUuids.indices) {
                val uuid = UUID.fromString(charUuids[i])
                val props = charProperties[i]
                val perms = charPermissions[i]

                val char = BluetoothGattCharacteristic(uuid, props, perms)

                // Set static value if non-empty.
                if (charValues[i].isNotEmpty()) {
                    char.value = charValues[i]
                    staticValues[uuid] = charValues[i]
                }

                // Add CCCD if the characteristic supports notifications or indications.
                if (props and (
                        BluetoothGattCharacteristic.PROPERTY_NOTIFY or
                            BluetoothGattCharacteristic.PROPERTY_INDICATE
                    ) != 0
                ) {
                    val cccd =
                        BluetoothGattDescriptor(
                            cccdUuid,
                            BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
                        )
                    char.addDescriptor(cccd)
                }

                characteristics[uuid] = char
                service.addCharacteristic(char)
            }

            val latch = CountDownLatch(1)
            serviceAddedLatch = latch
            gattServer?.addService(service)
            if (!latch.await(5, TimeUnit.SECONDS)) {
                Log.w(TAG, "addService timed out for $serviceUuid")
            }
            Log.d(TAG, "added service $serviceUuid with ${charUuids.size} characteristics")
        }
    }

    /**
     * Guards [advertiseCallback] / [advertiseRequestId] / [advertiser].
     *
     * start, stop and cancel arrive on JNI threads while AdvertiseCallback
     * fires on the stack's own; unsynchronized, a stop could pass through the
     * gap between start deciding to advertise and recording its callback, and
     * advertising would begin after stop had returned.
     */
    private val advertiseLock = Any()

    private var advertiseCallback: AdvertiseCallback? = null

    /** Request id of the live [advertiseCallback], for [cancelAdvertising]. */
    private var advertiseRequestId: Int = 0

    /**
     * Begin advertising. Returns [ADVERTISE_OK] when the request was handed to
     * the stack, or a failure code for something that went wrong before that.
     *
     * Success is *not* confirmed by the return value — the stack reports that
     * asynchronously through [nativeOnAdvertisingResult].
     */
    @JvmStatic
    fun startAdvertising(
        name: String,
        serviceUuids: Array<String>,
        requestId: Int,
    ): Int = synchronized(advertiseLock) { startAdvertisingLocked(name, serviceUuids, requestId) }

    private fun startAdvertisingLocked(
        name: String,
        serviceUuids: Array<String>,
        requestId: Int,
    ): Int {
        // Android can only stop an advertisement by handing back the exact
        // AdvertiseCallback it was started with. Overwriting the stored one
        // would leave the previous advertisement running with nothing able to
        // reach it, so a second start is refused rather than accepted.
        if (advertiseCallback != null) {
            Log.w(TAG, "startAdvertising called while already advertising")
            return ADVERTISE_ALREADY
        }
        // Resolved per call: null while Bluetooth is off, and valid again once
        // it comes back on.
        val adv =
            bluetoothManager?.adapter?.bluetoothLeAdvertiser ?: run {
                Log.e(TAG, "advertiser not available (is Bluetooth on?)")
                return ADVERTISE_UNAVAILABLE
            }
        // Remembered so stopAdvertising passes the same instance back.
        advertiser = adv
        advertiseRequestId = requestId

        bluetoothManager?.adapter?.name = name

        val settings =
            AdvertiseSettings
                .Builder()
                .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
                .setConnectable(true)
                .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_HIGH)
                .build()

        val dataBuilder =
            AdvertiseData
                .Builder()
                .setIncludeDeviceName(false)
        for (uuid in serviceUuids) {
            dataBuilder.addServiceUuid(ParcelUuid(UUID.fromString(uuid)))
        }
        val data = dataBuilder.build()

        // Scan response can carry the device name.
        val scanResponse =
            AdvertiseData
                .Builder()
                .setIncludeDeviceName(true)
                .build()

        advertiseCallback =
            object : AdvertiseCallback() {
                override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
                    Log.d(TAG, "advertising started")
                    nativeOnAdvertisingResult(requestId, true, 0)
                }

                override fun onStartFailure(errorCode: Int) {
                    Log.e(TAG, "advertising failed: errorCode=$errorCode")
                    // Nothing started, so there is nothing to stop -- release
                    // the slot or every later start would report ALREADY.
                    synchronized(advertiseLock) {
                        if (advertiseRequestId == requestId) {
                            advertiseCallback = null
                        }
                    }
                    // Outside the monitor: this crosses into Rust, which takes
                    // its own lock, and there is no reason to hold both.
                    nativeOnAdvertisingResult(requestId, false, errorCode)
                }
            }

        adv.startAdvertising(settings, data, scanResponse, advertiseCallback)
        return ADVERTISE_OK
    }

    @JvmStatic
    fun stopAdvertising() {
        synchronized(advertiseLock) {
            advertiseCallback?.let { cb ->
                advertiser?.stopAdvertising(cb)
                advertiseCallback = null
            }
        }
        Log.d(TAG, "advertising stopped")
    }

    /**
     * Tear down [requestId] if it is still the live request, otherwise do
     * nothing.
     *
     * Used when Rust gives up on a start: the callback cannot be
     * un-registered, so the advertisement has to be stopped explicitly or it
     * runs on with nothing able to reach it.
     */
    @JvmStatic
    fun cancelAdvertising(requestId: Int) {
        // `synchronized` is reentrant, so the nested stopAdvertising is fine.
        synchronized(advertiseLock) {
            if (advertiseRequestId == requestId && advertiseCallback != null) {
                Log.d(TAG, "cancelling advertising request $requestId")
                stopAdvertising()
            }
        }
    }

    /**
     * Send a notification/indication on a characteristic to a single
     * subscribed device.
     *
     * `confirm` selects the ATT write kind: `false` is a Handle Value
     * Notification, `true` a Handle Value Indication. `seq` is Rust's
     * monotonically increasing id for this call, echoed back through
     * [nativeOnNotificationSent] from `onNotificationSent`.
     *
     * Returns:
     *   0 = accepted (the stack reports completion via onNotificationSent)
     *   1 = busy (semaphore not available — caller should retry after a short delay)
     *   2 = device not connected or not subscribed to this characteristic
     *   3 = characteristic not found
     */
    @JvmStatic
    fun notifyCharacteristic(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
        confirm: Boolean,
        seq: Long,
    ): Int {
        val uuid = UUID.fromString(charUuid)
        val char = characteristics[uuid] ?: return 3
        val device = connectedDevices[deviceAddr] ?: return 2
        val subs = subscriptions[deviceAddr] ?: return 2
        if (uuid !in subs) return 2
        if (!acquireNotify(deviceAddr, timeoutMs = 50)) return 1
        val status = sendNotification(device, char, value, confirm)
        if (status != BluetoothStatusCodes.SUCCESS) {
            releaseNotify(deviceAddr)
            return 1
        }
        notifySeqByDevice[deviceAddr] = seq
        return 0
    }

    /**
     * Send a single notification/indication, handling the API 33+ / legacy
     * split. Returns `BluetoothStatusCodes.SUCCESS` when the stack accepted
     * the value. On API < 33, synchronizes on [char] to prevent concurrent
     * `char.value` races when multiple devices are notified from different
     * threads.
     */
    private fun sendNotification(
        device: BluetoothDevice,
        char: BluetoothGattCharacteristic,
        value: ByteArray,
        confirm: Boolean,
    ): Int =
        if (Build.VERSION.SDK_INT >= 33) {
            gattServer?.notifyCharacteristicChanged(device, char, confirm, value)
                ?: BluetoothStatusCodes.ERROR
        } else {
            @Suppress("DEPRECATION")
            synchronized(char) {
                char.value = value
                val sent = gattServer?.notifyCharacteristicChanged(device, char, confirm) ?: false
                if (sent) BluetoothStatusCodes.SUCCESS else BluetoothStatusCodes.ERROR
            }
        }

    @JvmStatic
    fun respondToRead(
        deviceAddr: String,
        requestId: Int,
        value: ByteArray,
    ) {
        val device = connectedDevices[deviceAddr] ?: return
        gattServer?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, value)
    }

    @JvmStatic
    fun respondToReadError(
        deviceAddr: String,
        requestId: Int,
    ) {
        val device = connectedDevices[deviceAddr] ?: return
        gattServer?.sendResponse(
            device,
            requestId,
            BluetoothGatt.GATT_FAILURE,
            0,
            null,
        )
    }

    @JvmStatic
    fun respondToWrite(
        deviceAddr: String,
        requestId: Int,
        success: Boolean,
    ) {
        val device = connectedDevices[deviceAddr] ?: return
        val status = if (success) BluetoothGatt.GATT_SUCCESS else BluetoothGatt.GATT_FAILURE
        gattServer?.sendResponse(device, requestId, status, 0, null)
    }

    @JvmStatic
    fun isPowered(): Boolean = bluetoothManager?.adapter?.isEnabled == true

    @JvmStatic
    fun areBlePermissionsGranted(): Boolean {
        val ctx = context ?: return false

        fun granted(p: String) =
            androidx.core.content.ContextCompat
                .checkSelfPermission(ctx, p) ==
                android.content.pm.PackageManager.PERMISSION_GRANTED
        return if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S) {
            arrayOf(
                android.Manifest.permission.BLUETOOTH_SCAN,
                android.Manifest.permission.BLUETOOTH_CONNECT,
                android.Manifest.permission.BLUETOOTH_ADVERTISE,
            ).all(::granted)
        } else {
            granted(android.Manifest.permission.ACCESS_FINE_LOCATION)
        }
    }

    // ── L2CAP ──

    @JvmStatic
    fun openL2capServer() {
        if (android.os.Build.VERSION.SDK_INT < 29) {
            nativeOnL2capServerError("L2CAP requires API 29+")
            return
        }

        val adapter =
            bluetoothManager?.adapter ?: run {
                nativeOnL2capServerError("adapter not available")
                return
            }

        try {
            val serverSocket = adapter.listenUsingInsecureL2capChannel()
            l2capServerSocket = serverSocket
            val psm = serverSocket.psm
            nativeOnL2capServerOpened(psm)

            // accept() blocks, and so does each accepted socket's read loop;
            // both belong on the IO dispatcher rather than on raw threads.
            scope.launch(Dispatchers.IO) {
                while (true) {
                    try {
                        val socket = serverSocket.accept()
                        val addr = socket.remoteDevice.address
                        val socketId = l2cap.register(socket)
                        nativeOnL2capChannelOpened(addr, socketId, true)
                        l2cap.startReadLoopAsync(socketId, addr, socket)
                    } catch (e: Exception) {
                        Log.d(TAG, "L2CAP accept ended: ${e.message}")
                        break
                    }
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "L2CAP server failed: ${e.message}")
            nativeOnL2capServerError(e.message ?: "server open failed")
        }
    }

    @JvmStatic
    fun closeL2capServer() {
        try {
            l2capServerSocket?.close()
        } catch (_: Exception) {
        }
        l2capServerSocket = null
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
}
