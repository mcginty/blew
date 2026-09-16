package org.jakebot.blew

import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

internal interface AdapterNames {
    /** The adapter's name, or null when it can't be read. */
    fun get(): String?

    /** Ask the stack to rename the adapter, which it applies asynchronously. False if it refuses. */
    fun set(name: String): Boolean
}

/**
 * Renames the Bluetooth adapter for a named advertisement, and holds the
 * advertisement back until the rename has taken effect.
 *
 * Android can only advertise the adapter's own name, and a rename lands
 * asynchronously while the scan response snapshots whatever name is in place
 * when advertising starts. Starting straight after renaming therefore
 * advertises the previous name -- and fails outright with
 * `ADVERTISE_FAILED_DATA_TOO_LARGE` when that name is too long for the scan
 * response. So a request's `onReady` only runs once its name has landed.
 *
 * The rename is never undone: the previous name is not recorded, and putting it
 * back is the application's business.
 *
 * `onReady` runs under this class's monitor, so [cancel] can't slip in between
 * deciding to advertise and advertising; it must not take a lock that is held
 * while calling in here. `onFailed` runs after the monitor is released, and may
 * therefore arrive after [cancel].
 */
internal class AdapterRename(
    private val names: AdapterNames,
    private val scope: CoroutineScope,
) {
    class Ticket internal constructor(
        val name: String,
    )

    private class Pending(
        val ticket: Ticket,
        val onReady: () -> Unit,
        val onFailed: () -> Unit,
    )

    private companion object {
        const val TAG = "AdapterRename"

        /** How long to wait for an ACTION_LOCAL_NAME_CHANGED before reading the name back. */
        const val SETTLE_MS = 1_000L
    }

    private val lock = Any()
    private var pending: Pending? = null

    /** Names handed to [AdapterNames.set] whose broadcast hasn't arrived, oldest first. */
    private val issued = ArrayDeque<String>()
    private var generation = 0

    /**
     * Put [name] on the adapter and call [onReady] once it has landed --
     * immediately, if it already has. If it hasn't landed within [SETTLE_MS],
     * [onFailed] runs instead. Returns null, having called neither, if the
     * stack refuses the rename.
     */
    fun request(
        name: String,
        onReady: () -> Unit,
        onFailed: () -> Unit,
    ): Ticket? {
        synchronized(lock) {
            // A rename still in flight is what the adapter is about to say.
            val current = issued.lastOrNull() ?: names.get()
            if (current != name) {
                if (!names.set(name)) {
                    Log.w(TAG, "the stack refused to rename the adapter")
                    return null
                }
                issued.addLast(name)
                val mine = ++generation
                scope.launch {
                    delay(SETTLE_MS)
                    synchronized(lock) { timedOut(mine) }?.invoke()
                }
            }
            val ticket = Ticket(name)
            if (issued.isEmpty()) {
                pending = null
                onReady()
            } else {
                pending = Pending(ticket, onReady, onFailed)
            }
            return ticket
        }
    }

    /** Drop [ticket]'s request if it is still waiting. */
    fun cancel(ticket: Ticket) {
        synchronized(lock) {
            if (pending?.ticket === ticket) pending = null
        }
    }

    /** Every ACTION_LOCAL_NAME_CHANGED goes here. */
    fun onNameChanged(name: String?) {
        synchronized(lock) {
            val index = if (name == null) -1 else issued.indexOf(name)
            if (name == null || index < 0) return
            // The stack applies renames in order, so everything before it landed too.
            repeat(index + 1) { issued.removeFirst() }
            if (issued.isEmpty()) ready(name)
        }
    }

    private fun ready(name: String) {
        pending?.takeIf { it.ticket.name == name }?.let {
            pending = null
            it.onReady()
        }
    }

    /** Returns the failure to deliver once the monitor is released, if any. */
    private fun timedOut(mine: Int): (() -> Unit)? {
        if (mine != generation || issued.isEmpty()) return null
        val name = issued.last()
        issued.clear()
        if (names.get() == name) {
            ready(name)
            return null
        }
        // Advertising now would put the previous name on air, which is the bug this exists to prevent.
        Log.w(TAG, "adapter rename to '$name' didn't take effect within ${SETTLE_MS}ms")
        val failed = pending ?: return null
        pending = null
        return failed.onFailed
    }
}
