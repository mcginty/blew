package org.jakebot.blew

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Plugin

@TauriPlugin
class BlewPlugin(
    private val activity: Activity,
) : Plugin(activity) {
    companion object {
        private const val TAG = "BlewPlugin"
        private const val PERMISSION_REQUEST_CODE = 42_001

        @Volatile
        private var hostActivity: Activity? = null

        // Snapshot of the aggregate BLE-permission state as of the last check.
        // `null` means no snapshot recorded yet (initial state).
        @Volatile
        private var lastPermissionsGranted: Boolean? = null

        @JvmStatic
        fun requestBlePermissions() {
            val activity =
                hostActivity ?: run {
                    Log.w(TAG, "requestBlePermissions called before plugin load")
                    return
                }
            activity.runOnUiThread { requestOnActivity(activity) }
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

        hostActivity = activity
        val ctx = activity.applicationContext
        BleCentralManager.init(ctx)
        BlePeripheralManager.init(ctx)

        lastPermissionsGranted = computeAggregateGranted(activity)

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
}
