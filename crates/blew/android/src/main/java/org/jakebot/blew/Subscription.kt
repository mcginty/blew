package org.jakebot.blew

import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * What a central enabled on a characteristic by writing its Client
 * Characteristic Configuration Descriptor (0x2902).
 *
 * The central, not the server, chooses between notification and indication:
 * a server may only indicate when the indicate bit is set, and only notify
 * when the notify bit is set. Android's `notifyCharacteristicChanged` takes
 * whatever `confirm` it is given, so the choice has to be carried from the
 * CCCD write to the send.
 */
internal enum class Subscription {
    NOTIFY,
    INDICATE,
    ;

    /** The `confirm` argument for `notifyCharacteristicChanged`. */
    val confirm: Boolean get() = this == INDICATE

    companion object {
        private const val NOTIFY_BIT = 0x01
        private const val INDICATE_BIT = 0x02

        /**
         * Parses a CCCD value, or returns null when neither bit is set (an
         * unsubscribe). A central that enables both gets notifications: they
         * are what it can accept at the lower cost.
         */
        fun fromCccd(value: ByteArray?): Subscription? {
            val bits = value?.firstOrNull()?.toInt() ?: return null
            return when {
                bits and NOTIFY_BIT != 0 -> NOTIFY
                bits and INDICATE_BIT != 0 -> INDICATE
                else -> null
            }
        }
    }
}

/**
 * Each device's [Subscription] per characteristic.
 *
 * A CCCD write and a send for the same device take the same lock, so a send
 * reads the subscription and hands it to the stack without a rewrite landing
 * in between: sending with a stale `confirm` puts the wrong PDU on the wire.
 */
internal class SubscriptionTable {
    private val devices = ConcurrentHashMap<String, HashMap<UUID, Subscription>>()

    /** Applies a CCCD write, returning whether the device is now subscribed. */
    fun update(
        addr: String,
        charUuid: UUID,
        cccd: ByteArray?,
    ): Boolean {
        val subscription = Subscription.fromCccd(cccd)
        val chars = devices.getOrPut(addr) { HashMap() }
        synchronized(chars) {
            if (subscription != null) chars[charUuid] = subscription else chars.remove(charUuid)
        }
        return subscription != null
    }

    fun remove(addr: String) {
        devices.remove(addr)
    }

    /**
     * Runs [block] with the device's current subscription, holding the lock
     * [update] takes. Returns null, without running [block], when the device
     * isn't subscribed. Keep [block] to the send itself.
     */
    fun <T> withSubscription(
        addr: String,
        charUuid: UUID,
        block: (Subscription) -> T,
    ): T? {
        val chars = devices[addr] ?: return null
        return synchronized(chars) { chars[charUuid]?.let(block) }
    }
}
