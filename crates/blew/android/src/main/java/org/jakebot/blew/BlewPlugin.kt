package org.jakebot.blew

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Plugin
import java.lang.ref.WeakReference

@TauriPlugin
class BlewPlugin(
    private val activity: Activity,
) : Plugin(activity) {
    companion object {
        private const val TAG = "BlewPlugin"
        private const val PERMISSION_REQUEST_CODE = 42_001
        private const val ENABLE_BLUETOOTH_REQUEST_CODE = 42_002

        // Weak: a static strong reference to an Activity keeps the whole
        // destroyed instance alive across recreation. Nothing here needs it to
        // survive its own Activity -- if it has gone, there is no UI thread to
        // put a permission dialog on anyway.
        @Volatile
        private var hostActivity: WeakReference<Activity>? = null

        // Snapshot of the aggregate BLE-permission state as of the last check.
        // `null` means no snapshot recorded yet (initial state).
        @Volatile
        private var lastPermissionsGranted: Boolean? = null

        // Holds no Activity: it is registered on the application context and
        // outlives every recreation.
        private val adapterStateReceiver =
            object : BroadcastReceiver() {
                override fun onReceive(
                    context: Context,
                    intent: Intent,
                ) {
                    if (intent.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
                    when (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                        BluetoothAdapter.STATE_ON -> BlewPluginNative.onAdapterStateChanged(true)
                        BluetoothAdapter.STATE_OFF -> BlewPluginNative.onAdapterStateChanged(false)
                    }
                }
            }

        // load() runs again for every recreated Activity; registering twice
        // would deliver every change twice.
        @Volatile
        private var adapterReceiverRegistered = false

        @JvmStatic
        fun requestBlePermissions() {
            val activity =
                hostActivity?.get() ?: run {
                    Log.w(TAG, "requestBlePermissions called with no live host activity")
                    return
                }
            activity.runOnUiThread { requestOnActivity(activity) }
        }

        @JvmStatic
        fun requestEnableBluetooth() {
            val activity =
                hostActivity?.get() ?: run {
                    Log.w(TAG, "requestEnableBluetooth called with no live host activity")
                    return
                }
            activity.runOnUiThread { requestEnableOnActivity(activity) }
        }

        private fun requestEnableOnActivity(activity: Activity) {
            val adapter =
                (activity.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager)?.adapter
            if (adapter == null) {
                Log.w(TAG, "requestEnableBluetooth: no Bluetooth adapter")
                return
            }
            if (adapter.isEnabled) {
                Log.d(TAG, "requestEnableBluetooth: adapter already on")
                return
            }
            // On 12+ the platform throws SecurityException for this intent
            // without BLUETOOTH_CONNECT; checking first turns that into a log line.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
                !hasPermission(activity, Manifest.permission.BLUETOOTH_CONNECT)
            ) {
                Log.w(TAG, "requestEnableBluetooth: BLUETOOTH_CONNECT not granted")
                return
            }
            // The result code is ignored: acceptance is reported by the
            // ACTION_STATE_CHANGED receivers as the adapter powers on.
            @Suppress("DEPRECATION")
            activity.startActivityForResult(
                Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE),
                ENABLE_BLUETOOTH_REQUEST_CODE,
            )
        }

        private fun requestOnActivity(activity: Activity) {
            val needed = mutableListOf<String>()

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                if (!hasPermission(activity, Manifest.permission.BLUETOOTH_SCAN)) {
                    needed.add(Manifest.permission.BLUETOOTH_SCAN)
                }
                if (!hasPermission(activity, Manifest.permission.BLUETOOTH_CONNECT)) {
                    needed.add(Manifest.permission.BLUETOOTH_CONNECT)
                }
                if (!hasPermission(activity, Manifest.permission.BLUETOOTH_ADVERTISE)) {
                    needed.add(Manifest.permission.BLUETOOTH_ADVERTISE)
                }
            } else {
                if (!hasPermission(activity, Manifest.permission.ACCESS_FINE_LOCATION)) {
                    needed.add(Manifest.permission.ACCESS_FINE_LOCATION)
                }
            }

            if (needed.isNotEmpty()) {
                Log.d(TAG, "requesting BLE permissions: $needed")
                ActivityCompat.requestPermissions(
                    activity,
                    needed.toTypedArray(),
                    PERMISSION_REQUEST_CODE,
                )
            } else {
                Log.d(TAG, "all BLE permissions already granted")
            }
        }

        private fun hasPermission(
            activity: Activity,
            permission: String,
        ): Boolean =
            ContextCompat.checkSelfPermission(activity, permission) ==
                PackageManager.PERMISSION_GRANTED

        private fun computeAggregateGranted(activity: Activity): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                hasPermission(activity, Manifest.permission.BLUETOOTH_SCAN) &&
                    hasPermission(activity, Manifest.permission.BLUETOOTH_CONNECT) &&
                    hasPermission(activity, Manifest.permission.BLUETOOTH_ADVERTISE)
            } else {
                hasPermission(activity, Manifest.permission.ACCESS_FINE_LOCATION)
            }
    }

    override fun load(webView: WebView) {
        super.load(webView)

        hostActivity = WeakReference(activity)
        val ctx = activity.applicationContext
        BleCentralManager.init(ctx)
        BlePeripheralManager.init(ctx)

        lastPermissionsGranted = computeAggregateGranted(activity)

        if (!adapterReceiverRegistered) {
            ctx.registerReceiver(
                adapterStateReceiver,
                IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED),
            )
            adapterReceiverRegistered = true
        }

        if (BlewPluginNative.autoRequestPermissionsEnabled()) {
            requestOnActivity(activity)
        }
        Log.d(TAG, "blew plugin loaded")
    }

    override fun onResume() {
        super.onResume()
        val current = computeAggregateGranted(activity)
        val previous = lastPermissionsGranted
        if (previous != current) {
            lastPermissionsGranted = current
            Log.d(TAG, "permissions changed: granted=$current")
            BlewPluginNative.onPermissionsChanged(current)
        }
    }
}

internal object BlewPluginNative {
    @JvmStatic external fun autoRequestPermissionsEnabled(): Boolean

    @JvmStatic external fun onPermissionsChanged(granted: Boolean)

    @JvmStatic external fun onAdapterStateChanged(powered: Boolean)
}
