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
    }

    override fun load(webView: WebView) {
        super.load(webView)

        val ctx = activity.applicationContext
        BleCentralManager.init(ctx)
        BlePeripheralManager.init(ctx)

        requestBlePermissions()
        Log.d(TAG, "blew plugin loaded")
    }

    private fun requestBlePermissions() {
        val needed = mutableListOf<String>()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Android 12+: BLE permissions + location for scan results
            if (!hasPermission(Manifest.permission.BLUETOOTH_SCAN)) {
                needed.add(Manifest.permission.BLUETOOTH_SCAN)
            }
            if (!hasPermission(Manifest.permission.BLUETOOTH_CONNECT)) {
                needed.add(Manifest.permission.BLUETOOTH_CONNECT)
            }
            if (!hasPermission(Manifest.permission.BLUETOOTH_ADVERTISE)) {
                needed.add(Manifest.permission.BLUETOOTH_ADVERTISE)
            }
        }
        // Location is required on all Android versions for BLE scan
        // results to be delivered.
        if (!hasPermission(Manifest.permission.ACCESS_FINE_LOCATION)) {
            needed.add(Manifest.permission.ACCESS_FINE_LOCATION)
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

    private fun hasPermission(permission: String): Boolean =
        ContextCompat.checkSelfPermission(activity, permission) ==
            PackageManager.PERMISSION_GRANTED
}
