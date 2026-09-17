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
 * Renames the Bluetooth adapter and tells each requester once the rename has
 * taken effect.
 *
 * Android applies a rename asynchronously, and an advertisement snapshots
 * whatever name is in place when it starts. Starting straight after renaming
 * therefore advertises the previous name -- and fails outright with
 * `ADVERTISE_FAILED_DATA_TOO_LARGE` when that name is too long for the scan
 * response. So a request's `onReady` only runs once its name has landed.
 *
 * Requests can overlap: a named advertisement and an application's own
 * rename, say. The stack applies renames in order, so the last one requested
 * is the name the adapter ends up with. Once every rename has landed, requests
 * for that name are ready and the rest fail, since the name they waited for
 * was overwritten.
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
    private val pending = mutableListOf<Pending>()

    /** Names handed to [AdapterNames.set] whose broadcast hasn't arrived, oldest first. */
    private val issued = ArrayDeque<String>()
    private var generation = 0

    /**
     * Put [name] on the adapter and call [onReady] once it has landed --
     * immediately, if it already has. [onFailed] runs instead if the rename
     * hasn't landed within [SETTLE_MS], or a later request renames the adapter
     * to something else. Returns null, having called neither, if the stack
     * refuses the rename.
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
                    synchronized(lock) { timedOut(mine) }.forEach { it() }
                }
            }
            val ticket = Ticket(name)
            if (issued.isEmpty()) {
                onReady()
            } else {
                pending.add(Pending(ticket, onReady, onFailed))
            }
            return ticket
        }
    }

    /** Drop [ticket]'s request if it is still waiting. */
    fun cancel(ticket: Ticket) {
        synchronized(lock) {
            pending.removeAll { it.ticket === ticket }
        }
    }

    /** Every ACTION_LOCAL_NAME_CHANGED goes here. */
    fun onNameChanged(name: String?) {
        val failed =
            synchronized(lock) {
                val index = if (name == null) -1 else issued.indexOf(name)
                if (name == null || index < 0) return
                // The stack applies renames in order, so everything before it landed too.
                repeat(index + 1) { issued.removeFirst() }
                if (issued.isEmpty()) settle(name) else emptyList()
            }
        failed.forEach { it() }
    }

    /**
     * Every rename has landed, leaving [landed] on the adapter -- or null when
     * it can't be trusted to. Releases the requests for that name and returns
     * the failures of the rest, to deliver once the monitor is released.
     */
    private fun settle(landed: String?): List<() -> Unit> {
        val waiting = pending.toList()
        pending.clear()
        val failed = mutableListOf<() -> Unit>()
        for (request in waiting) {
            if (request.ticket.name == landed) request.onReady() else failed.add(request.onFailed)
        }
        return failed
    }

    private fun timedOut(mine: Int): List<() -> Unit> {
        if (mine != generation || issued.isEmpty()) return emptyList()
        val name = issued.last()
        issued.clear()
        if (names.get() == name) return settle(name)
        // Starting now would put the previous name on air, which is the bug this exists to prevent.
        Log.w(TAG, "adapter rename to '$name' didn't take effect within ${SETTLE_MS}ms")
        return settle(null)
    }
}
