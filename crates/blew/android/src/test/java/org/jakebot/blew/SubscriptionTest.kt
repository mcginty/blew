package org.jakebot.blew

import org.junit.Assert.*
import org.junit.Test

class SubscriptionTest {
    private fun cccd(vararg bytes: Int) = ByteArray(bytes.size) { bytes[it].toByte() }

    @Test
    fun notifyBitSendsNotifications() {
        val sub = Subscription.fromCccd(cccd(0x01, 0x00))
        assertEquals(Subscription.NOTIFY, sub)
        assertFalse(sub!!.confirm)
    }

    @Test
    fun indicateBitSendsIndications() {
        val sub = Subscription.fromCccd(cccd(0x02, 0x00))
        assertEquals(Subscription.INDICATE, sub)
        assertTrue(sub!!.confirm)
    }

    @Test
    fun bothBitsSendNotifications() {
        assertEquals(Subscription.NOTIFY, Subscription.fromCccd(cccd(0x03, 0x00)))
    }

    @Test
    fun noBitsUnsubscribe() {
        assertNull(Subscription.fromCccd(cccd(0x00, 0x00)))
        assertNull(Subscription.fromCccd(cccd(0x04, 0x00)))
        assertNull(Subscription.fromCccd(ByteArray(0)))
        assertNull(Subscription.fromCccd(null))
    }
}
