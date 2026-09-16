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

internal interface BorrowedNameStore {
    fun load(): AdapterNameLease.Borrow?

    fun save(borrow: AdapterNameLease.Borrow?)
}

/**
 * Lends the Bluetooth adapter's name to an advertisement, and gives it back.
 *
 * Android can only advertise the adapter's own name. Renaming the adapter is
 * device-global and persistent, and it lands asynchronously, while the scan
 * response carries whatever name is in place when advertising starts. So a
 * holder is only told to advertise once its name has landed, and the name it
 * replaced is kept in [store] so that a later process can still give it back.
 *
 * Restoring follows one rule: when nothing holds the lease and the adapter
 * carries the name we gave it, put back the one we replaced. Any other name --
 * the user renaming the phone, another app's beacon -- is not ours to
 * overwrite. The record outlives that, so if the other party later puts our
 * name back, the original follows it.
 *
 * `onReady` runs under the lease's monitor and must not take a lock that is
 * held while calling into the lease.
 */
internal class AdapterNameLease(
    private val names: AdapterNames,
    private val store: BorrowedNameStore,
    private val scope: CoroutineScope,
) {
    data class Borrow(
        val theirs: String,
        val ours: String,
    )

    class Ticket internal constructor(
        val name: String,
    )

    private class Pending(
        val ticket: Ticket,
        val onReady: () -> Unit,
    )

    private companion object {
        const val TAG = "AdapterNameLease"

        /** Backstop for an ACTION_LOCAL_NAME_CHANGED that never arrives. */
        const val SETTLE_MS = 1_000L
    }

    private val lock = Any()
    private var borrow: Borrow? = store.load()
    private var holder: Ticket? = null
    private var pending: Pending? = null

    /** Names handed to [AdapterNames.set] whose broadcast hasn't arrived, oldest first. */
    private val issued = ArrayDeque<String>()
    private var generation = 0

    /**
     * Put [name] on the adapter and call [onReady] once it has landed --
     * immediately, if it already has. Returns null, having changed nothing, if
     * the stack refuses the rename.
     */
    fun acquire(
        name: String,
        onReady: () -> Unit,
    ): Ticket? {
        synchronized(lock) {
            // A rename still in flight is what the adapter is about to say.
            val current = issued.lastOrNull() ?: names.get()
            if (current != name) {
                if (!rename(name)) return null
                val lent = borrow
                // Renaming over our own name must not record it as theirs.
                val theirs = if (lent != null && current == lent.ours) lent.theirs else current
                if (theirs != null) {
                    record(Borrow(theirs, name))
                } else {
                    Log.w(TAG, "adapter name unreadable; it can't be restored after advertising")
                }
            }
            val ticket = Ticket(name)
            holder = ticket
            if (issued.isEmpty()) {
                pending = null
                onReady()
            } else {
                pending = Pending(ticket, onReady)
            }
            return ticket
        }
    }

    /** End [ticket]'s hold, dropping its onReady if that hasn't run. A stale ticket is ignored. */
    fun release(ticket: Ticket) {
        synchronized(lock) {
            if (holder !== ticket) return
            holder = null
            pending = null
            reconcileLocked()
        }
    }

    /** Every ACTION_LOCAL_NAME_CHANGED goes here. */
    fun onNameChanged(name: String?) {
        synchronized(lock) {
            val index = if (name == null) -1 else issued.indexOf(name)
            if (name != null && index >= 0) {
                // The stack applies renames in order, so everything before it landed too.
                repeat(index + 1) { issued.removeFirst() }
                if (issued.isEmpty()) settled(name)
            } else if (issued.isEmpty()) {
                reconcileLocked()
            }
        }
    }

    /** Give back a borrowed name if the rule allows it now: at startup, and when the adapter comes on. */
    fun reconcile() {
        synchronized(lock) { reconcileLocked() }
    }

    private fun reconcileLocked() {
        if (holder != null || issued.isNotEmpty()) return
        val lent = borrow ?: return
        when (names.get()) {
            lent.theirs -> record(null)

            // Refused while the adapter is off; the record waits for the next reconcile.
            lent.ours -> rename(lent.theirs)
        }
    }

    private fun settled(name: String) {
        pending?.takeIf { it.ticket.name == name }?.let {
            pending = null
            it.onReady()
        }
        reconcileLocked()
    }

    private fun rename(name: String): Boolean {
        if (!names.set(name)) {
            Log.w(TAG, "the stack refused to rename the adapter")
            return false
        }
        issued.addLast(name)
        val mine = ++generation
        scope.launch {
            delay(SETTLE_MS)
            synchronized(lock) { settleTimedOut(mine) }
        }
        return true
    }

    private fun settleTimedOut(mine: Int) {
        if (mine != generation || issued.isEmpty()) return
        val name = issued.last()
        issued.clear()
        if (names.get() == name) {
            settled(name)
            return
        }
        // Not reconciling here: a restore that never lands would otherwise retry forever.
        Log.w(TAG, "adapter rename unconfirmed after ${SETTLE_MS}ms")
        pending?.let {
            pending = null
            it.onReady()
        }
    }

    private fun record(next: Borrow?) {
        if (next == borrow) return
        borrow = next
        store.save(next)
    }
}
