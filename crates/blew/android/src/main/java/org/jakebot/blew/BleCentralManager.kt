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
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import java.util.UUID

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

    private var context: Context? = null
    private var bluetoothManager: BluetoothManager? = null
    private var adapter: BluetoothAdapter? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val connections =
        GattConnections(
            factory = { addr, callback ->
                val ctx = context ?: error("context not initialized")
                adapter?.getRemoteDevice(addr)?.connectGatt(ctx, false, callback, BluetoothDevice.TRANSPORT_LE)
            },
            scope = scope,
            events =
                object : GattEvents {
                    override fun onConnectionStateChanged(
                        deviceAddr: String,
                        generation: Int,
                        connected: Boolean,
                        gattStatus: Int,
                    ) = nativeOnConnectionStateChanged(deviceAddr, generation, connected, gattStatus)

                    override fun onServicesDiscovered(
                        deviceAddr: String,
                        generation: Int,
                        servicesJson: String,
                    ) = nativeOnServicesDiscovered(deviceAddr, generation, servicesJson)

                    override fun onCharacteristicRead(
                        deviceAddr: String,
                        generation: Int,
                        charUuid: String,
                        value: ByteArray,
                        status: Int,
                    ) = nativeOnCharacteristicRead(deviceAddr, generation, charUuid, value, status)

                    override fun onCharacteristicWrite(
                        deviceAddr: String,
                        generation: Int,
                        charUuid: String,
                        status: Int,
                    ) = nativeOnCharacteristicWrite(deviceAddr, generation, charUuid, status)

                    override fun onCharacteristicChanged(
                        deviceAddr: String,
                        generation: Int,
                        charUuid: String,
                        value: ByteArray,
                    ) = nativeOnCharacteristicChanged(deviceAddr, generation, charUuid, value)

                    override fun onMtuChanged(
                        deviceAddr: String,
                        generation: Int,
                        mtu: Int,
                    ) = nativeOnMtuChanged(deviceAddr, generation, mtu)
                },
        )

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
        generation: Int,
        connected: Boolean,
        gattStatus: Int,
    )

    @JvmStatic
    external fun nativeOnServicesDiscovered(
        deviceAddr: String,
        generation: Int,
        servicesJson: String,
    )

    @JvmStatic
    external fun nativeOnCharacteristicRead(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
        status: Int,
    )

    @JvmStatic
    external fun nativeOnCharacteristicWrite(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        status: Int,
    )

    @JvmStatic
    external fun nativeOnCharacteristicChanged(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
    )

    @JvmStatic
    external fun nativeOnMtuChanged(
        deviceAddr: String,
        generation: Int,
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

    @JvmStatic
    fun connect(
        deviceAddr: String,
        generation: Int,
    ) = connections.connect(deviceAddr, generation)

    @JvmStatic
    fun disconnect(
        deviceAddr: String,
        generation: Int,
    ) = connections.disconnect(deviceAddr, generation)

    @JvmStatic
    fun forceClose(
        deviceAddr: String,
        generation: Int,
    ) = connections.forceClose(deviceAddr, generation)

    @JvmStatic
    fun refresh(deviceAddr: String) = connections.refresh(deviceAddr)

    @JvmStatic
    fun discoverServices(
        deviceAddr: String,
        generation: Int,
    ) = connections.discoverServices(deviceAddr, generation)

    @JvmStatic
    fun readCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ) = connections.readCharacteristic(deviceAddr, generation, charUuid)

    @JvmStatic
    fun writeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
        value: ByteArray,
        writeType: Int,
    ) = connections.writeCharacteristic(deviceAddr, generation, charUuid, value, writeType)

    @JvmStatic
    fun subscribeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ) = connections.subscribeCharacteristic(deviceAddr, generation, charUuid)

    @JvmStatic
    fun unsubscribeCharacteristic(
        deviceAddr: String,
        generation: Int,
        charUuid: String,
    ) = connections.unsubscribeCharacteristic(deviceAddr, generation, charUuid)

    @JvmStatic
    fun getMtu(deviceAddr: String) = connections.getMtu(deviceAddr)

    @JvmStatic
    fun isPowered(): Boolean = adapter?.isEnabled == true

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
}
