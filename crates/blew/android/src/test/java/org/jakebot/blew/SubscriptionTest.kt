package org.jakebot.blew

import org.junit.Assert.*
import org.junit.Test
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

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

    private val addr = "AA:BB:CC:DD:EE:FF"
    private val char = UUID.fromString("0000abcd-0000-1000-8000-00805f9b34fb")

    @Test
    fun tableFollowsTheLatestWrite() {
        val table = SubscriptionTable()
        assertNull(table.withSubscription(addr, char) { it })

        assertTrue(table.update(addr, char, cccd(0x02, 0x00)))
        assertEquals(Subscription.INDICATE, table.withSubscription(addr, char) { it })

        assertTrue(table.update(addr, char, cccd(0x01, 0x00)))
        assertEquals(Subscription.NOTIFY, table.withSubscription(addr, char) { it })

        assertFalse(table.update(addr, char, cccd(0x00, 0x00)))
        assertNull(table.withSubscription(addr, char) { it })

        table.update(addr, char, cccd(0x01, 0x00))
        table.remove(addr)
        assertNull(table.withSubscription(addr, char) { it })
    }

    @Test
    fun writeWaitsForAnInProgressSend() {
        val table = SubscriptionTable()
        table.update(addr, char, cccd(0x02, 0x00))

        val sending = CountDownLatch(1)
        val finishSend = CountDownLatch(1)
        val sender =
            thread {
                table.withSubscription(addr, char) {
                    sending.countDown()
                    finishSend.await()
                }
            }
        assertTrue(sending.await(5, TimeUnit.SECONDS))

        val writer = thread { table.update(addr, char, cccd(0x00, 0x00)) }
        writer.join(200)
        assertTrue("a CCCD write landed during a send", writer.isAlive)

        finishSend.countDown()
        sender.join(5_000)
        writer.join(5_000)
        assertFalse(writer.isAlive)
        assertNull(table.withSubscription(addr, char) { it })
    }
}
