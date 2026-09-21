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

    /** The stack refused to rename the adapter for a named advertisement. */
    const val ADVERTISE_NAME_REJECTED = 3

    /** A [setAdapterName] is still waiting for its name, so a named advertisement can't rename. */
    const val ADVERTISE_RENAME_BUSY = 4

    /**
     * Error code reported through [nativeOnAdvertisingResult] when the adapter
     * rename a named advertisement waits for never took effect. Negative, so it
     * can't collide with an `AdvertiseCallback` error.
     */
    const val ADVERTISE_FAILED_RENAME_UNCONFIRMED = -1

    /** notifyCharacteristic handed a notification to the stack; [nativeOnNotificationSent] follows. */
    const val NOTIFY_SENT = 0

    /** notifyCharacteristic handed an indication to the stack; [nativeOnNotificationSent] follows. */
    const val NOTIFY_INDICATED = 1

    /** The device is not connected, or not subscribed to the characteristic. */
    const val NOTIFY_NOT_SUBSCRIBED = 2

    /** No local characteristic has that UUID. */
    const val NOTIFY_CHAR_NOT_FOUND = 3

    /** The stack refused the value; no [nativeOnNotificationSent] will follow. */
    const val NOTIFY_REJECTED = 4

    /** setAdapterName handed the rename to [AdapterRename]; the outcome follows asynchronously. */
    const val RENAME_OK = 0

    /** No [AdapterRename] yet: [init] hasn't run. */
    const val RENAME_UNAVAILABLE = 1

    /** The stack refused the rename -- Bluetooth is off, or BLUETOOTH_CONNECT isn't granted. */
    const val RENAME_REJECTED = 2

    /** Another rename -- a named advertisement's, or another [setAdapterName] -- is still waiting. */
    const val RENAME_BUSY = 3

    private var context: Context? = null
    private var bluetoothManager: BluetoothManager? = null

    /**
     * Hosts the blocking L2CAP accept loop. `BluetoothServerSocket.accept()`
     * has no async form, so it has to block something; a managed dispatcher is
     * a better host for that than a raw thread.
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    // Owns the GATT server and the services registered on it, including
    // dropping both when a power cycle invalidates them.
    private val gattServer: GattServerHost =
        GattServerHost(
            GattServerFactory { generation ->
                bluetoothManager?.openGattServer(context, gattCallback(generation))
            },
        )

    private var advertiser: BluetoothLeAdvertiser? = null

    // Track connected devices for notification delivery.
    private val connectedDevices = ConcurrentHashMap<String, BluetoothDevice>()

    // What each device enabled per characteristic through its CCCD write.
    private val subscriptions = SubscriptionTable()

    // ── L2CAP state ──
    private val l2cap =
        L2capSocketManager(
            tag = TAG,
            onData = { socketId, data -> nativeOnL2capChannelData(socketId, data) },
            onClosed = { socketId, error -> nativeOnL2capChannelClosed(socketId, error) },
            startId = 100_000,
        )

    @Volatile private var l2capServerSocket: BluetoothServerSocket? = null

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
     * Reports `onNotificationSent` for [deviceAddr]. Rust sends at most one
     * value per device at a time, so the device alone identifies the send.
     */
    @JvmStatic
    external fun nativeOnNotificationSent(
        deviceAddr: String,
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

    /** Async outcome of [setAdapterName]: whether the new name took effect. */
    @JvmStatic
    external fun nativeOnAdapterRenameResult(
        requestId: Int,
        success: Boolean,
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
                when (intent.action) {
                    BluetoothAdapter.ACTION_STATE_CHANGED -> {
                        val state = intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
                        when (state) {
                            BluetoothAdapter.STATE_ON -> nativeOnAdapterStateChanged(true)
                            BluetoothAdapter.STATE_OFF -> onAdapterOff()
                        }
                    }

                    BluetoothAdapter.ACTION_LOCAL_NAME_CHANGED -> {
                        adapterRename?.onNameChanged(intent.getStringExtra(BluetoothAdapter.EXTRA_LOCAL_NAME))
                    }
                }
            }
        }

    /**
     * Drop everything that belonged to the stack instance the adapter just
     * took down.
     *
     * Android invalidates the GATT server and every connection on it without
     * reporting either, so the state has to be dropped from here. A server
     * kept across the cycle takes [addService] and never reports the service
     * added, which leaves the peripheral advertising a service table Android
     * no longer has.
     *
     * The lost connections are reported before the adapter event, so an
     * application sees its peers go before the radio they were on.
     */
    private fun onAdapterOff() {
        gattServer.reset()
        for (addr in connectedDevices.keys.toList()) {
            // The disconnect callback may still arrive for the same device;
            // whichever gets the entry out of the map reports it, once.
            if (connectedDevices.remove(addr) != null) {
                subscriptions.remove(addr)
                nativeOnConnectionStateChanged(addr, false)
            }
        }
        nativeOnAdapterStateChanged(false)
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
        if (adapterRename == null) {
            adapterRename = AdapterRename(adapterNames, scope)
        }
        // Registering the same receiver twice delivers every adapter state
        // change twice. init() runs again whenever the host activity is
        // recreated -- a rotation or a dark-mode toggle is enough -- and
        // nothing ever unregisters, so the duplicates would accumulate.
        if (!receiverRegistered) {
            val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
            filter.addAction(BluetoothAdapter.ACTION_LOCAL_NAME_CHANGED)
            ctx.registerReceiver(adapterStateReceiver, filter)
            receiverRegistered = true
        }
    }

    /** Renames the adapter for named advertisements and [setAdapterName]; see [AdapterRename]. */
    @Volatile
    private var adapterRename: AdapterRename? = null

    private val adapterNames: AdapterNames = AndroidAdapterNames()

    /** The adapter's name, or null when it can't be read. */
    @JvmStatic
    fun getAdapterName(): String? = adapterNames.get()

    /**
     * Rename the adapter for the application. Returns [RENAME_OK] once the
     * request is in hand, and reports through [nativeOnAdapterRenameResult]
     * when the name has taken effect or failed to.
     */
    @JvmStatic
    fun setAdapterName(
        name: String,
        requestId: Int,
    ): Int {
        val rename = adapterRename ?: return RENAME_UNAVAILABLE
        val outcome =
            rename.request(
                name,
                onReady = { nativeOnAdapterRenameResult(requestId, true) },
                onFailed = { nativeOnAdapterRenameResult(requestId, false) },
            )
        return when (outcome) {
            is AdapterRename.Outcome.Accepted -> RENAME_OK
            AdapterRename.Outcome.Refused -> RENAME_REJECTED
            AdapterRename.Outcome.Busy -> RENAME_BUSY
        }
    }

    private class AndroidAdapterNames : AdapterNames {
        override fun get(): String? =
            try {
                bluetoothManager?.adapter?.name
            } catch (e: SecurityException) {
                null
            }

        override fun set(name: String): Boolean =
            try {
                bluetoothManager?.adapter?.setName(name) == true
            } catch (e: SecurityException) {
                false
            }
    }

    /**
     * A callback for the server [generation] identifies.
     *
     * One instance per server, rather than one shared: `onServiceAdded` says
     * nothing about which server it came from, and a callback from a server
     * the adapter invalidated must not answer an add on its replacement.
     */
    private fun gattCallback(generation: Int): BluetoothGattServerCallback =
        object : BluetoothGattServerCallback() {
            override fun onServiceAdded(
                status: Int,
                service: BluetoothGattService?,
            ) {
                // The service is the framework's pending one, not necessarily
                // the one the stack reported on, so it is logged and not used;
                // see [GattServerHost.onServiceAdded].
                Log.d(TAG, "onServiceAdded status=$status pending=${service?.uuid}")
                gattServer.onServiceAdded(generation, status)
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
                    // [onAdapterOff] drops connections the stack never reports
                    // losing; taking the entry is what decides who reports it.
                    if (connectedDevices.remove(addr) != null) {
                        subscriptions.remove(addr)
                        nativeOnConnectionStateChanged(addr, false)
                    }
                }
            }

            override fun onNotificationSent(
                device: BluetoothDevice,
                status: Int,
            ) {
                nativeOnNotificationSent(device.address, status)
            }

            override fun onCharacteristicReadRequest(
                device: BluetoothDevice,
                requestId: Int,
                offset: Int,
                characteristic: BluetoothGattCharacteristic,
            ) {
                // Auto-respond for static characteristics (matches CoreBluetooth behaviour
                // where characteristics with a non-nil value are served by the framework).
                val staticValue = gattServer.staticValue(characteristic.uuid)
                if (staticValue != null) {
                    if (offset > staticValue.size) {
                        gattServer.server()?.sendResponse(
                            device,
                            requestId,
                            BluetoothGatt.GATT_INVALID_OFFSET,
                            offset,
                            null,
                        )
                        return
                    }
                    gattServer.server()?.sendResponse(
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
                if (descriptor.uuid == GattServerHost.CCCD_UUID) {
                    val charUuid = descriptor.characteristic.uuid
                    val addr = device.address
                    val subscribed = subscriptions.update(addr, charUuid, value)
                    nativeOnSubscriptionChanged(addr, charUuid.toString(), subscribed)
                }

                if (responseNeeded) {
                    gattServer.server()?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, null)
                }
            }
        }

    /**
     * Add a GATT service, returning a [GattServerHost] `SERVICE_*` code once
     * the stack has confirmed it -- or said why it hasn't.
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
    ): Int {
        val service =
            BluetoothGattService(
                UUID.fromString(serviceUuid),
                BluetoothGattService.SERVICE_TYPE_PRIMARY,
            )

        val chars = HashMap<UUID, BluetoothGattCharacteristic>()
        val statics = HashMap<UUID, ByteArray>()

        for (i in charUuids.indices) {
            val uuid = UUID.fromString(charUuids[i])
            val props = charProperties[i]
            val perms = charPermissions[i]

            val char = BluetoothGattCharacteristic(uuid, props, perms)

            // Set static value if non-empty.
            if (charValues[i].isNotEmpty()) {
                char.value = charValues[i]
                statics[uuid] = charValues[i]
            }

            // Add CCCD if the characteristic supports notifications or indications.
            if (props and (
                    BluetoothGattCharacteristic.PROPERTY_NOTIFY or
                        BluetoothGattCharacteristic.PROPERTY_INDICATE
                ) != 0
            ) {
                val cccd =
                    BluetoothGattDescriptor(
                        GattServerHost.CCCD_UUID,
                        BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
                    )
                char.addDescriptor(cccd)
            }

            chars[uuid] = char
            service.addCharacteristic(char)
        }

        val result = gattServer.addService(service, chars, statics)
        if (result == GattServerHost.SERVICE_OK) {
            Log.d(TAG, "added service $serviceUuid with ${charUuids.size} characteristics")
        } else {
            Log.w(TAG, "addService failed for $serviceUuid (result=$result)")
        }
        return result
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

    /** Request id of the live [advertiseCallback], for [stopAdvertising]. */
    private var advertiseRequestId: Int = 0

    /** The adapter rename the live [advertiseCallback] waits for, if it advertises a name. */
    private var advertiseRenameTicket: AdapterRename.Ticket? = null

    /**
     * Begin advertising. Returns [ADVERTISE_OK] when the request was handed to
     * the stack, or a failure code for something that went wrong before that.
     *
     * A non-null [name] is permission to rename the adapter, which is the only
     * name Android can advertise. The rename is left in place afterwards. The
     * stack only sees the request once the rename has landed; if it doesn't
     * land, the start fails through [nativeOnAdvertisingResult] with
     * [ADVERTISE_FAILED_RENAME_UNCONFIRMED]. See [AdapterRename].
     *
     * Success is *not* confirmed by the return value — the stack reports that
     * asynchronously through [nativeOnAdvertisingResult].
     */
    @JvmStatic
    fun startAdvertising(
        name: String?,
        serviceUuids: Array<String>,
        requestId: Int,
    ): Int = synchronized(advertiseLock) { startAdvertisingLocked(name, serviceUuids, requestId) }

    private fun startAdvertisingLocked(
        name: String?,
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
            name?.let {
                AdvertiseData
                    .Builder()
                    .setIncludeDeviceName(true)
                    .build()
            }

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
                            advertiseRenameTicket = null
                        }
                    }
                    // Outside the monitor: this crosses into Rust, which takes
                    // its own lock, and there is no reason to hold both.
                    nativeOnAdvertisingResult(requestId, false, errorCode)
                }
            }

        val callback = advertiseCallback
        if (name == null) {
            adv.startAdvertising(settings, data, scanResponse, callback)
            return ADVERTISE_OK
        }
        val rename =
            adapterRename ?: run {
                advertiseCallback = null
                return ADVERTISE_UNAVAILABLE
            }
        // onReady may run after this returns, from the name broadcast. Stop
        // cancels the ticket before touching the advertiser, which drops a
        // start that hasn't run yet, so a late one can't outlive it.
        val outcome =
            rename.request(
                name,
                onReady = { adv.startAdvertising(settings, data, scanResponse, callback) },
                onFailed = { renameUnconfirmed(requestId) },
            )
        if (outcome !is AdapterRename.Outcome.Accepted) {
            advertiseCallback = null
            return if (outcome is AdapterRename.Outcome.Busy) ADVERTISE_RENAME_BUSY else ADVERTISE_NAME_REJECTED
        }
        advertiseRenameTicket = outcome.ticket
        return ADVERTISE_OK
    }

    private fun renameUnconfirmed(requestId: Int) {
        synchronized(advertiseLock) {
            // Nothing started, so there is nothing to stop; just free the slot.
            if (advertiseRequestId == requestId) {
                advertiseCallback = null
                advertiseRenameTicket = null
            }
        }
        // Outside the monitor, as in onStartFailure. A stale id is ignored by Rust.
        nativeOnAdvertisingResult(requestId, false, ADVERTISE_FAILED_RENAME_UNCONFIRMED)
    }

    /**
     * Stop [requestId] if it is still the live request, otherwise do nothing.
     *
     * Both an explicit stop and the cleanup of an abandoned start arrive here,
     * and either can be overtaken by a newer start that has already claimed
     * the advertiser. Stopping whatever happens to be running would tear that
     * newer request down and leave it waiting on a callback that never comes,
     * so the teardown is qualified by request id instead.
     *
     * The callback cannot be un-registered, so a start Rust has given up on
     * has to be stopped explicitly or it runs on with nothing able to reach it.
     */
    @JvmStatic
    fun stopAdvertising(requestId: Int) {
        synchronized(advertiseLock) {
            val cb = advertiseCallback
            if (cb == null || advertiseRequestId != requestId) {
                return
            }
            advertiseRenameTicket?.let { adapterRename?.cancel(it) }
            advertiseRenameTicket = null
            advertiser?.stopAdvertising(cb)
            advertiseCallback = null
        }
        Log.d(TAG, "advertising stopped (request $requestId)")
    }

    /**
     * Send a value on a characteristic to a single subscribed device, as
     * whatever the device enabled in its CCCD write.
     *
     * The stack takes one value per device until `onNotificationSent`, and
     * drops or refuses a second. Rust serializes calls per device and waits for
     * [nativeOnNotificationSent] after [NOTIFY_SENT] or [NOTIFY_INDICATED].
     */
    @JvmStatic
    fun notifyCharacteristic(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
    ): Int {
        val uuid = UUID.fromString(charUuid)
        val char = gattServer.characteristic(uuid) ?: return NOTIFY_CHAR_NOT_FOUND
        val device = connectedDevices[deviceAddr] ?: return NOTIFY_NOT_SUBSCRIBED
        return sendToSubscriber(device, char, value)
    }

    /**
     * Send [value] as whatever [device] currently subscribes to on [char],
     * returning a `NOTIFY_*` code. The subscription is read and used under the
     * lock the CCCD write handler takes, so a rewrite can't change notification
     * vs. indication midway, and it is the only thing that decides between them.
     */
    private fun sendToSubscriber(
        device: BluetoothDevice,
        char: BluetoothGattCharacteristic,
        value: ByteArray,
    ): Int =
        subscriptions.withSubscription(device.address, char.uuid) { subscription ->
            when {
                !sendNotification(device, char, value, subscription.confirm) -> NOTIFY_REJECTED
                subscription.confirm -> NOTIFY_INDICATED
                else -> NOTIFY_SENT
            }
        } ?: NOTIFY_NOT_SUBSCRIBED

    /**
     * Send a single notification or indication, handling the API 33+ / legacy
     * split. On API < 33, synchronizes on [char] to prevent concurrent
     * `char.value` races when multiple devices are notified from different
     * threads.
     */
    private fun sendNotification(
        device: BluetoothDevice,
        char: BluetoothGattCharacteristic,
        value: ByteArray,
        confirm: Boolean,
    ): Boolean =
        if (Build.VERSION.SDK_INT >= 33) {
            gattServer.server()?.notifyCharacteristicChanged(device, char, confirm, value) ==
                BluetoothStatusCodes.SUCCESS
        } else {
            @Suppress("DEPRECATION")
            synchronized(char) {
                char.value = value
                gattServer.server()?.notifyCharacteristicChanged(device, char, confirm) ?: false
            }
        }

    @JvmStatic
    fun respondToRead(
        deviceAddr: String,
        requestId: Int,
        value: ByteArray,
    ) {
        val device = connectedDevices[deviceAddr] ?: return
        gattServer.server()?.sendResponse(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, value)
    }

    @JvmStatic
    fun respondToReadError(
        deviceAddr: String,
        requestId: Int,
    ) {
        val device = connectedDevices[deviceAddr] ?: return
        gattServer.server()?.sendResponse(
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
        gattServer.server()?.sendResponse(device, requestId, status, 0, null)
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
    fun openL2capServer(secure: Boolean) {
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
            val serverSocket =
                if (secure) {
                    adapter.listenUsingL2capChannel()
                } else {
                    adapter.listenUsingInsecureL2capChannel()
                }
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
