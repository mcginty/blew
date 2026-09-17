package org.jakebot.blew

/**
 * What a central enabled on a characteristic by writing its Client
 * Characteristic Configuration Descriptor (0x2902).
 *
 * The central, not the server, chooses between notification and indication:
 * a server may only indicate when the indicate bit is set, and only notify
 * when the notify bit is set. CoreBluetooth and BlueZ enforce this themselves;
 * Android's `notifyCharacteristicChanged` takes whatever `confirm` it is given,
 * so the choice has to be carried from the CCCD write to the send.
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
