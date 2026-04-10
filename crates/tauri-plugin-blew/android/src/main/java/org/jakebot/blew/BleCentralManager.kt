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
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit

/**
 * Singleton managing the Android BLE central role (scanner + GATT client).
 *
 * Kotlin methods are called from Rust via JNI. Android BLE callbacks are
 * forwarded to Rust via [external fun] JNI hooks.
 *
 * ## GATT operation serialization
 *
 * Android's [BluetoothGatt] allows only one in-flight operation at a time
 * (read, write, descriptor write, discover services, request MTU). Issuing
 * a second operation while one is pending causes the second to silently fail.
 *
 * We use a single global [Semaphore] with one permit to serialize all GATT
 * operations across all devices. Android's Bluetooth controller has a single
 * HCI command queue — concurrent operations on different [BluetoothGatt]
 * objects can silently fail. The semaphore is acquired before issuing an
 * operation and released in the corresponding callback (or immediately for
 * fire-and-forget operations like write-without-response).
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

    private var context: Context? = null
    private var bluetoothManager: BluetoothManager? = null
    private var adapter: BluetoothAdapter? = null

    // Active GATT connections keyed by device address.
    private val gattConnections = ConcurrentHashMap<String, BluetoothGatt>()

    // Per-device MTU (default 23 until negotiated).
    private val mtuMap = ConcurrentHashMap<String, Int>()

    // Global GATT operation semaphore (1 permit = 1 op at a time across ALL
    // devices). Android's Bluetooth controller has a single HCI command queue;
    // concurrent operations on different BluetoothGatt objects can silently fail.
    private val gattSemaphore = Semaphore(1)

    // Devices whose global semaphore hold is for MTU negotiation (acquired in
    // onConnectionStateChange, released in onMtuChanged). Tracked so we can
    // release the semaphore on disconnect if MTU never completed.
    private val mtuHoldDevices: MutableSet<String> = ConcurrentHashMap.newKeySet()

    // Devices with a pending write-with-response. Used by onCharacteristicWrite
    // to avoid double-releasing the semaphore for write-without-response.
    private val pendingWriteResponse: MutableSet<String> = ConcurrentHashMap.newKeySet()

    // ── L2CAP state ──
    private val l2cap =
        L2capSocketManager(
            tag = TAG,
            onData = { socketId, data -> nativeOnL2capChannelData(socketId, data) },
            onClosed = { socketId -> nativeOnL2capChannelClosed(socketId) },
        )

    // ── JNI hooks (Kotlin → Rust) ──

    @JvmStatic
    external fun nativeOnDeviceDiscovered(
        deviceAddr: String,
        deviceName: String?,
        rssi: Int,
        serviceUuids: String,
    )

    @JvmStatic
    external fun nativeOnConnectionStateChanged(
        deviceAddr: String,
        connected: Boolean,
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
    external fun nativeOnL2capChannelClosed(socketId: Int)

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

    fun init(ctx: Context) {
        context = ctx
        bluetoothManager = ctx.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        adapter = bluetoothManager?.adapter
        Log.d(TAG, "initialized, adapter=${adapter != null}")
        val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
        ctx.registerReceiver(adapterStateReceiver, filter)
    }

    // ── GATT operation serialization helpers ──

    /**
     * Acquire the global GATT semaphore, blocking up to [timeoutMs].
     * Returns true if acquired, false on timeout (GATT busy).
     */
    private fun acquireGatt(timeoutMs: Long = 5000): Boolean = gattSemaphore.tryAcquire(timeoutMs, TimeUnit.MILLISECONDS)

    /** Release the global GATT semaphore after an operation completes. */
    private fun releaseGatt() {
        gattSemaphore.release()
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

                    nativeOnDeviceDiscovered(addr, name, rssi, uuids)
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

    private val gattCallback =
        object : BluetoothGattCallback() {
            override fun onConnectionStateChange(
                gatt: BluetoothGatt,
                status: Int,
                newState: Int,
            ) {
                val addr = gatt.device.address

                if (newState == BluetoothProfile.STATE_CONNECTED) {
                    gattConnections[addr] = gatt
                    // Hold the global GATT semaphore during MTU negotiation so
                    // that no operations (from any device) proceed until the
                    // chipset finishes. Don't notify Rust yet — wait for onMtuChanged.
                    gattSemaphore.drainPermits()
                    mtuHoldDevices.add(addr)
                    if (!gatt.requestMtu(512)) {
                        // MTU request failed — release semaphore and notify Rust anyway.
                        Log.w(TAG, "requestMtu failed for $addr, proceeding without MTU negotiation")
                        mtuHoldDevices.remove(addr)
                        releaseGatt()
                        nativeOnConnectionStateChanged(addr, true)
                    }
                } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                    gattConnections.remove(addr)
                    mtuMap.remove(addr)
                    // If this device was holding the semaphore for MTU negotiation,
                    // release it so other devices aren't deadlocked.
                    if (mtuHoldDevices.remove(addr)) {
                        releaseGatt()
                    }
                    // For regular operations, gatt.close() triggers pending
                    // callbacks (with error status) which release the semaphore.
                    gatt.close()
                    nativeOnConnectionStateChanged(addr, false)
                }
            }

            override fun onMtuChanged(
                gatt: BluetoothGatt,
                mtu: Int,
                status: Int,
            ) {
                val addr = gatt.device.address
                if (status == BluetoothGatt.GATT_SUCCESS) {
                    mtuMap[addr] = mtu
                    nativeOnMtuChanged(addr, mtu)
                }
                // Release the global semaphore held since onConnectionStateChange.
                mtuHoldDevices.remove(addr)
                releaseGatt()
                // NOW notify Rust — MTU is negotiated, GATT is ready for operations.
                nativeOnConnectionStateChanged(addr, true)
            }

            override fun onServicesDiscovered(
                gatt: BluetoothGatt,
                status: Int,
            ) {
                val addr = gatt.device.address
                // Release the GATT semaphore (acquired in discoverServices).
                releaseGatt()
                if (status == BluetoothGatt.GATT_SUCCESS) {
                    val json = servicesToJson(gatt.services)
                    nativeOnServicesDiscovered(addr, json)
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
                // Release the GATT semaphore (acquired in readCharacteristic).
                releaseGatt()
                nativeOnCharacteristicRead(
                    gatt.device.address,
                    characteristic.uuid.toString(),
                    value,
                    status,
                )
            }

            override fun onCharacteristicWrite(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                status: Int,
            ) {
                // Only release the semaphore for write-with-response. For
                // write-without-response it was already released in writeCharacteristic().
                if (pendingWriteResponse.remove(gatt.device.address)) {
                    releaseGatt()
                }
                nativeOnCharacteristicWrite(
                    gatt.device.address,
                    characteristic.uuid.toString(),
                    status,
                )
            }

            override fun onDescriptorWrite(
                gatt: BluetoothGatt,
                descriptor: BluetoothGattDescriptor,
                status: Int,
            ) {
                // Release the GATT semaphore (acquired in subscribeCharacteristic).
                releaseGatt()
            }

            override fun onCharacteristicChanged(
                gatt: BluetoothGatt,
                characteristic: BluetoothGattCharacteristic,
                value: ByteArray,
            ) {
                // Notifications don't hold the GATT semaphore — they are passive.
                nativeOnCharacteristicChanged(
                    gatt.device.address,
                    characteristic.uuid.toString(),
                    value,
                )
            }
        }

    // ── Connection management ──

    @JvmStatic
    fun connect(deviceAddr: String) {
        val ctx =
            context ?: run {
                Log.e(TAG, "context not initialized")
                return
            }

        // Close any stale GATT connection to avoid leaking clientIf slots.
        // Android has a limit of ~7 concurrent GATT clients.
        gattConnections.remove(deviceAddr)?.let { oldGatt ->
            oldGatt.disconnect()
            oldGatt.close()
            Log.d(TAG, "closed stale GATT for $deviceAddr")
        }

        val device =
            adapter?.getRemoteDevice(deviceAddr) ?: run {
                Log.e(TAG, "could not get remote device $deviceAddr")
                return
            }
        // TRANSPORT_LE ensures we connect over BLE, not classic Bluetooth.
        val gatt = device.connectGatt(ctx, false, gattCallback, BluetoothDevice.TRANSPORT_LE)
        if (gatt == null) {
            Log.e(TAG, "connectGatt returned null for $deviceAddr")
            nativeOnConnectionStateChanged(deviceAddr, false)
            return
        }
        Log.d(TAG, "connecting to $deviceAddr")
    }

    @JvmStatic
    fun disconnect(deviceAddr: String) {
        gattConnections[deviceAddr]?.let { gatt ->
            gatt.disconnect()
            Log.d(TAG, "disconnecting from $deviceAddr")
        }
    }

    // ── GATT operations (serialized via global semaphore) ──

    @JvmStatic
    fun discoverServices(deviceAddr: String): Int {
        val gatt = gattConnections[deviceAddr] ?: return STATUS_NOT_CONNECTED
        if (!acquireGatt(10000)) return STATUS_GATT_BUSY
        if (!gatt.discoverServices()) {
            releaseGatt()
            return STATUS_GATT_FAILED
        }
        // onServicesDiscovered will release the semaphore.
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun readCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = gattConnections[deviceAddr] ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
        if (!acquireGatt()) return STATUS_GATT_BUSY
        if (!gatt.readCharacteristic(char)) {
            releaseGatt()
            return STATUS_GATT_FAILED
        }
        // onCharacteristicRead will release the semaphore.
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun writeCharacteristic(
        deviceAddr: String,
        charUuid: String,
        value: ByteArray,
        writeType: Int,
    ): Int {
        val gatt = gattConnections[deviceAddr] ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND
        if (!acquireGatt()) return STATUS_GATT_BUSY
        val result = gatt.writeCharacteristic(char, value, writeType)
        if (result != BluetoothStatusCodes.SUCCESS) {
            releaseGatt()
            return STATUS_GATT_FAILED
        }
        if (writeType == BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) {
            releaseGatt()
        } else {
            pendingWriteResponse.add(deviceAddr)
        }
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun subscribeCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = gattConnections[deviceAddr] ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

        if (!gatt.setCharacteristicNotification(char, true)) return STATUS_GATT_FAILED

        // Write to CCCD to enable notifications on the remote device.
        val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
        val descriptor = char.getDescriptor(cccdUuid) ?: return STATUS_CHAR_NOT_FOUND
        if (!acquireGatt()) return STATUS_GATT_BUSY
        val result =
            gatt.writeDescriptor(
                descriptor,
                BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE,
            )
        if (result != BluetoothStatusCodes.SUCCESS) {
            releaseGatt()
            return STATUS_GATT_FAILED
        }
        // onDescriptorWrite will release the semaphore.
        return STATUS_SUCCESS
    }

    @JvmStatic
    fun unsubscribeCharacteristic(
        deviceAddr: String,
        charUuid: String,
    ): Int {
        val gatt = gattConnections[deviceAddr] ?: return STATUS_NOT_CONNECTED
        val char = findCharacteristic(gatt, charUuid) ?: return STATUS_CHAR_NOT_FOUND

        // Always disable local notification state first, even if the CCCD
        // write below fails (GATT busy). The remote side will eventually
        // notice via timeout.
        gatt.setCharacteristicNotification(char, false)

        val cccdUuid = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
        val descriptor = char.getDescriptor(cccdUuid)
        if (descriptor != null) {
            if (!acquireGatt()) return STATUS_GATT_BUSY
            val result =
                gatt.writeDescriptor(
                    descriptor,
                    BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE,
                )
            if (result != BluetoothStatusCodes.SUCCESS) {
                releaseGatt()
                return STATUS_GATT_FAILED
            }
            // onDescriptorWrite will release.
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

        Thread {
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
        }.start()
    }

    @JvmStatic
    fun writeL2cap(
        socketId: Int,
        data: ByteArray,
    ) = l2cap.write(socketId, data)

    @JvmStatic
    fun closeL2cap(socketId: Int) = l2cap.close(socketId)

    // ── Helpers ──

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
