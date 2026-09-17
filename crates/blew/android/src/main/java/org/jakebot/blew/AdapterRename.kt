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
 * Renames the Bluetooth adapter and tells the requester once the rename has
 * taken effect.
 *
 * Android applies a rename asynchronously, and an advertisement snapshots
 * whatever name is in place when it starts. Starting straight after renaming
 * therefore advertises the previous name -- and fails outright with
 * `ADVERTISE_FAILED_DATA_TOO_LARGE` when that name is too long for the scan
 * response. So a request's `onReady` only runs once its name has landed.
 *
 * One request waits at a time. Another arriving meanwhile -- an application's
 * rename during a named start, say -- is refused with [Outcome.Busy] rather
 * than queued or allowed to replace the first: the two would be fighting over
 * one device-global name, and replacing a waiter silently strands its caller.
 *
 * Only the last name asked for is tracked, so a broadcast for any other name
 * is ignored. That can't put a wrong name on air: an advertisement takes the
 * name in place when it starts, which is the name that broadcast announced.
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
    class Ticket internal constructor()

    sealed interface Outcome {
        /** The request is waiting for its name, or has already been told it landed. */
        class Accepted(
            val ticket: Ticket,
        ) : Outcome

        /** The stack refused the rename. */
        object Refused : Outcome

        /** Another request is still waiting for its name. */
        object Busy : Outcome
    }

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

    /** The last name handed to [AdapterNames.set], until its broadcast arrives. */
    private var inFlight: String? = null
    private var generation = 0

    /**
     * Put [name] on the adapter and call [onReady] once it has landed --
     * immediately, if it already has -- or [onFailed] if it hasn't within
     * [SETTLE_MS]. Neither is called unless the outcome is [Outcome.Accepted].
     */
    fun request(
        name: String,
        onReady: () -> Unit,
        onFailed: () -> Unit,
    ): Outcome {
        synchronized(lock) {
            if (pending != null) return Outcome.Busy
            // A rename still in flight is what the adapter is about to say.
            val current = inFlight ?: names.get()
            if (current != name) {
                if (!names.set(name)) {
                    Log.w(TAG, "the stack refused to rename the adapter")
                    return Outcome.Refused
                }
                inFlight = name
                val mine = ++generation
                scope.launch {
                    delay(SETTLE_MS)
                    synchronized(lock) { timedOut(mine) }?.invoke()
                }
            }
            val ticket = Ticket()
            if (inFlight == null) {
                onReady()
            } else {
                pending = Pending(ticket, onReady, onFailed)
            }
            return Outcome.Accepted(ticket)
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
            if (name == null || name != inFlight) return
            inFlight = null
            pending?.let {
                pending = null
                it.onReady()
            }
        }
    }

    /** Returns the failure to deliver once the monitor is released, if any. */
    private fun timedOut(mine: Int): (() -> Unit)? {
        if (mine != generation) return null
        val name = inFlight ?: return null
        inFlight = null
        val waiting = pending ?: return null
        pending = null
        if (names.get() == name) {
            waiting.onReady()
            return null
        }
        // Starting now would put the previous name on air, which is the bug this exists to prevent.
        Log.w(TAG, "adapter rename to '$name' didn't take effect within ${SETTLE_MS}ms")
        return waiting.onFailed
    }
}
