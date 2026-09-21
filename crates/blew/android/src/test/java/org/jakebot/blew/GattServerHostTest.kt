package org.jakebot.blew

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattService
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread

class GattServerHostTest {
    private companion object {
        val SERVICE_UUID: UUID = UUID.fromString("00001234-0000-1000-8000-00805f9b34fb")
        val OTHER_SERVICE_UUID: UUID = UUID.fromString("0000f00d-0000-1000-8000-00805f9b34fb")
        val CHAR_UUID: UUID = UUID.fromString("00005678-0000-1000-8000-00805f9b34fb")
        val OTHER_CHAR_UUID: UUID = UUID.fromString("00009abc-0000-1000-8000-00805f9b34fb")
        val VALUE = byteArrayOf(1, 2, 3)
    }

    /** A service that reached the stack, and the server it reached. */
    private class Added(
        val generation: Int,
        val serviceUuid: UUID,
    )

    /** Hands out servers whose `addService` does whatever the test says. */
    private class FakeFactory : GattServerFactory {
        val servers = mutableListOf<BluetoothGattServer>()
        var available = true

        /** What the stack does with a service; the default registers it at once. */
        var onAdd: (Added) -> Boolean = { added ->
            confirm(added)
            true
        }

        lateinit var host: GattServerHost

        /** Counts down as each service reaches the stack. */
        val reached = CountDownLatch(1)

        /** Reports a registration complete, as [added]'s server. */
        fun confirm(
            added: Added,
            status: Int = BluetoothGatt.GATT_SUCCESS,
        ) = host.onServiceAdded(added.generation, status)

        override fun open(generation: Int): BluetoothGattServer? {
            if (!available) return null
            val server = mock(BluetoothGattServer::class.java)
            `when`(server.addService(any(BluetoothGattService::class.java))).thenAnswer { invocation ->
                reached.countDown()
                val service = invocation.getArgument<BluetoothGattService>(0)
                onAdd(Added(generation, service.uuid))
            }
            servers.add(server)
            return server
        }
    }

    private class Fixture(
        addTimeoutMs: Long = 50,
    ) {
        val factory = FakeFactory()
        val host = GattServerHost(factory, addTimeoutMs)

        init {
            factory.host = host
        }

        fun add(
            serviceUuid: UUID = SERVICE_UUID,
            charUuid: UUID = CHAR_UUID,
        ): Int {
            val service = mock(BluetoothGattService::class.java)
            `when`(service.uuid).thenReturn(serviceUuid)
            return host.addService(
                service,
                mapOf(charUuid to mock(BluetoothGattCharacteristic::class.java)),
                mapOf(charUuid to VALUE),
            )
        }
    }

    @Test
    fun servicesShareOneServerAndPublishWhatTheStackRegistered() {
        val f = Fixture()

        assertEquals(GattServerHost.SERVICE_OK, f.add())
        assertEquals(GattServerHost.SERVICE_OK, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))

        assertEquals(1, f.factory.servers.size)
        assertNotNull(f.host.characteristic(CHAR_UUID))
        assertNotNull(f.host.characteristic(OTHER_CHAR_UUID))
        assertArrayEquals(VALUE, f.host.staticValue(CHAR_UUID))
    }

    /**
     * The reported bug: a power cycle drops the server's registration, and a
     * server kept across it takes `addService` and never reports it added.
     */
    @Test
    fun aPowerCycleReplacesTheServerTheNextServiceGoes() {
        val f = Fixture()
        assertEquals(GattServerHost.SERVICE_OK, f.add())
        val stale = f.factory.servers.single()

        f.host.reset()

        verify(stale, times(1)).close()
        assertNull(f.host.server())
        assertNull(f.host.characteristic(CHAR_UUID))
        assertNull(f.host.staticValue(CHAR_UUID))

        assertEquals(GattServerHost.SERVICE_OK, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))
        assertEquals(2, f.factory.servers.size)
        assertNotSame(stale, f.host.server())
        assertNotNull(f.host.characteristic(OTHER_CHAR_UUID))
        // The pre-cycle service is not back: nothing re-added it.
        assertNull(f.host.characteristic(CHAR_UUID))
        verify(f.factory.servers[1], never()).close()
    }

    /** Power off while a service is registering: the add is told, not left to time out. */
    @Test
    fun resetWakesAnAddWaitingOnTheServerItLost() {
        // Long enough that a reset that failed to wake the add would hang the
        // test rather than pass it on the timeout.
        val f = Fixture(addTimeoutMs = 60_000)
        f.factory.onAdd = { true }

        val result = AtomicInteger(Int.MIN_VALUE)
        val adding = thread { result.set(f.add()) }

        assertTrue(f.factory.reached.await(5, TimeUnit.SECONDS))
        f.host.reset()

        adding.join(5_000)
        assertFalse(adding.isAlive)
        assertEquals(GattServerHost.SERVICE_UNAVAILABLE, result.get())
        assertNull(f.host.characteristic(CHAR_UUID))
    }

    @Test
    fun aServiceTheStackNeverConfirmsTimesOutAndPublishesNothing() {
        val f = Fixture()
        f.factory.onAdd = { true }

        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add())
        assertNull(f.host.characteristic(CHAR_UUID))
        assertNull(f.host.staticValue(CHAR_UUID))
    }

    @Test
    fun aServiceTheStackRefusesPublishesNothing() {
        val f = Fixture()
        f.factory.onAdd = { false }
        assertEquals(GattServerHost.SERVICE_REJECTED, f.add())

        f.factory.onAdd = { added ->
            f.factory.confirm(added, BluetoothGatt.GATT_FAILURE)
            true
        }
        assertEquals(GattServerHost.SERVICE_REJECTED, f.add())

        assertNull(f.host.characteristic(CHAR_UUID))
    }

    /** A refused add leaves nothing outstanding, so the next one still works. */
    @Test
    fun aRefusedServiceDoesNotStrandTheNextOne() {
        val f = Fixture()
        f.factory.onAdd = { false }
        assertEquals(GattServerHost.SERVICE_REJECTED, f.add())

        f.factory.onAdd = { added ->
            f.factory.confirm(added)
            true
        }
        assertEquals(GattServerHost.SERVICE_OK, f.add())
        assertNotNull(f.host.characteristic(CHAR_UUID))
    }

    /**
     * A callback that arrives after its own add gave up still owns the
     * platform's registration slot, so nothing may be registered until it
     * lands: `BluetoothGattServer` keeps one pending service and answers
     * whichever is pending when a registration completes, so a second add
     * would be confirmed by the first one's callback and never hear its own.
     */
    @Test
    fun aTimedOutAddKeepsThePlatformSlotUntilItsCallbackArrives() {
        val f = Fixture()
        f.factory.onAdd = { true }
        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add())
        val server = f.factory.servers.single()
        verify(server, times(1)).addService(any(BluetoothGattService::class.java))

        // The add that follows must not reach the stack at all.
        assertEquals(GattServerHost.SERVICE_BUSY, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))
        assertEquals(GattServerHost.SERVICE_BUSY, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))
        verify(server, times(1)).addService(any(BluetoothGattService::class.java))
        assertNull(f.host.characteristic(OTHER_CHAR_UUID))
        assertNull(f.host.characteristic(CHAR_UUID))

        // The late callback frees the slot, and the next add is registered.
        f.host.onServiceAdded(1, BluetoothGatt.GATT_SUCCESS)
        f.factory.onAdd = { added ->
            f.factory.confirm(added)
            true
        }
        assertEquals(GattServerHost.SERVICE_OK, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))
        assertNotNull(f.host.characteristic(OTHER_CHAR_UUID))
        // The one that timed out is still not registered: its late callback
        // said nothing about which service the stack took.
        assertNull(f.host.characteristic(CHAR_UUID))
    }

    /** A power cycle frees the slot: the closed server's pending service went with it. */
    @Test
    fun aPowerCycleFreesTheSlotATimedOutAddWasHolding() {
        val f = Fixture()
        f.factory.onAdd = { true }
        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add())
        assertEquals(GattServerHost.SERVICE_BUSY, f.add())

        f.host.reset()

        f.factory.onAdd = { added ->
            f.factory.confirm(added)
            true
        }
        assertEquals(GattServerHost.SERVICE_OK, f.add())
        assertNotNull(f.host.characteristic(CHAR_UUID))
    }

    /**
     * A callback from the server a power cycle took down arrives while the
     * new server is registering, and carries nothing of its own to tell them
     * apart.
     */
    @Test
    fun aCallbackFromTheClosedServerDoesNotConfirmTheNewOne() {
        val f = Fixture()
        f.factory.onAdd = { true }
        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add())
        val staleGeneration = 1

        f.host.reset()

        f.factory.onAdd = { added ->
            assertNotEquals(staleGeneration, added.generation)
            f.host.onServiceAdded(staleGeneration, BluetoothGatt.GATT_SUCCESS)
            true
        }
        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add())
        assertNull(f.host.characteristic(CHAR_UUID))
    }

    @Test
    fun noServerMeansUnavailable() {
        val f = Fixture()
        f.factory.available = false

        assertEquals(GattServerHost.SERVICE_UNAVAILABLE, f.add())
        assertNull(f.host.server())
        assertTrue(f.factory.servers.isEmpty())
    }

    /** `onServiceAdded` with nothing outstanding is the stack answering a dead add. */
    @Test
    fun aCallbackWithNothingOutstandingIsIgnored() {
        val f = Fixture()
        assertEquals(GattServerHost.SERVICE_OK, f.add())

        f.host.onServiceAdded(1, BluetoothGatt.GATT_SUCCESS)

        f.factory.onAdd = { true }
        assertEquals(GattServerHost.SERVICE_TIMED_OUT, f.add(OTHER_SERVICE_UUID, OTHER_CHAR_UUID))
    }
}
