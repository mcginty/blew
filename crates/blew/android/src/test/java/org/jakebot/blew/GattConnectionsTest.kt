package org.jakebot.blew

import android.bluetooth.*
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

@OptIn(ExperimentalCoroutinesApi::class)
class GattConnectionsTest {
    private companion object {
        const val ADDR = "AA:BB:CC:DD:EE:FF"
        val UUID_VALUE: UUID = UUID.fromString("00001234-0000-1000-8000-00805f9b34fb")
    }

    private class Client(
        val callback: BluetoothGattCallback,
    ) {
        val gatt: BluetoothGatt = mock(BluetoothGatt::class.java)
        val characteristic: BluetoothGattCharacteristic = mock(BluetoothGattCharacteristic::class.java)
        val descriptor: BluetoothGattDescriptor = mock(BluetoothGattDescriptor::class.java)

        init {
            val service = mock(BluetoothGattService::class.java)
            `when`(characteristic.uuid).thenReturn(UUID_VALUE)
            `when`(descriptor.characteristic).thenReturn(characteristic)
            `when`(characteristic.getDescriptor(any(UUID::class.java))).thenReturn(descriptor)
            `when`(service.getCharacteristic(UUID_VALUE)).thenReturn(characteristic)
            `when`(service.uuid).thenReturn(UUID_VALUE)
            `when`(service.characteristics).thenReturn(listOf(characteristic))
            `when`(gatt.services).thenReturn(listOf(service))
            `when`(gatt.requestMtu(512)).thenReturn(true)
            `when`(gatt.discoverServices()).thenReturn(true)
            `when`(gatt.readCharacteristic(characteristic)).thenReturn(true)
            `when`(gatt.setCharacteristicNotification(characteristic, true)).thenReturn(true)
        }

        fun connected() = callback.onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)

        fun disconnected() = callback.onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_DISCONNECTED)

        fun mtu(value: Int = 512) = callback.onMtuChanged(gatt, value, 0)
    }

    private class FakeGattFactory : GattFactory {
        val clients = mutableListOf<Client>()
        var beforeReturn: (Client) -> Unit = {}

        override fun open(
            addr: String,
            callback: BluetoothGattCallback,
        ): BluetoothGatt {
            val client = Client(callback)
            clients.add(client)
            beforeReturn(client)
            return client.gatt
        }
    }

    private class Fixture(
        scope: TestScope,
    ) {
        val factory = FakeGattFactory()
        val events: GattEvents = mock(GattEvents::class.java)
        val connections = GattConnections(factory, events, scope.backgroundScope)
    }

    @Test
    fun timeoutsWithoutCallbacksReleaseEveryClientExactlyOnce() =
        runTest {
            val f = Fixture(this)
            for (generation in 1..12) {
                f.connections.connect(ADDR, generation)
                val client = f.factory.clients.last()
                f.connections.forceClose(ADDR, generation)
                f.connections.forceClose(ADDR, generation)
                client.disconnected()
                verify(client.gatt, times(1)).close()
                verify(client.gatt, times(1)).disconnect()
                verify(f.events, times(1)).onConnectionStateChanged(ADDR, generation, false, 0)
            }
            assertEquals(12, f.factory.clients.size)
        }

    @Test
    fun connectedCallbackBeforeFactoryReturnsOwnsItsClient() =
        runTest {
            val f = Fixture(this)
            f.factory.beforeReturn = { it.connected() }
            f.connections.connect(ADDR, 1)
            runCurrent()
            val client = f.factory.clients.single()
            verify(client.gatt).requestMtu(512)
            client.mtu()
            runCurrent()
            verify(f.events).onConnectionStateChanged(ADDR, 1, true, 0)
            verify(client.gatt, never()).close()
            f.connections.forceClose(ADDR, 1)
            verify(client.gatt).close()
        }

    @Test
    fun disconnectedCallbackBeforeFactoryReturnsClosesOnlyOnce() =
        runTest {
            val f = Fixture(this)
            f.factory.beforeReturn = { it.disconnected() }
            f.connections.connect(ADDR, 1)
            verify(
                f.factory.clients
                    .single()
                    .gatt,
                times(1),
            ).close()
            verify(f.events, times(1)).onConnectionStateChanged(ADDR, 1, false, 0)
        }

    @Test
    fun retirementWhileFactoryIsBlockedClosesTheUnpublishedHandle() =
        runTest {
            val f = Fixture(this)
            val entered = CountDownLatch(1)
            val resume = CountDownLatch(1)
            f.factory.beforeReturn = {
                entered.countDown()
                check(resume.await(5, TimeUnit.SECONDS))
            }
            val connecting = thread { f.connections.connect(ADDR, 1) }
            try {
                assertTrue(entered.await(5, TimeUnit.SECONDS))
                f.connections.forceClose(ADDR, 1)
            } finally {
                resume.countDown()
                connecting.join(5000)
            }
            assertFalse(connecting.isAlive)
            verify(
                f.factory.clients
                    .single()
                    .gatt,
                times(1),
            ).close()
            f.factory.beforeReturn = {}
            f.connections.connect(ADDR, 2)
            verify(
                f.factory.clients
                    .last()
                    .gatt,
                never(),
            ).close()
            f.connections.forceClose(ADDR, 2)
        }

    @Test
    fun cancellationBeforeDispatchPreventsLateConnectAndOldDisconnectCannotCloseReplacement() =
        runTest {
            val f = Fixture(this)
            f.connections.forceClose(ADDR, 1)
            f.connections.connect(ADDR, 1)
            assertTrue(f.factory.clients.isEmpty())
            f.connections.connect(ADDR, 2)
            f.connections.disconnect(ADDR, 1)
            f.connections.forceClose(ADDR, 1)
            verify(
                f.factory.clients
                    .single()
                    .gatt,
                never(),
            ).disconnect()
            verifyNoInteractions(f.events)
            f.connections.forceClose(ADDR, 2)
        }

    @Test
    fun lateMtuAndConnectionCallbacksCannotCompleteReplacement() =
        runTest {
            val f = Fixture(this)
            f.connections.connect(ADDR, 1)
            val old = f.factory.clients.single()
            old.connected()
            runCurrent()
            f.connections.forceClose(ADDR, 1)
            f.connections.connect(ADDR, 2)
            val live = f.factory.clients.last()
            live.connected()
            runCurrent()
            clearInvocations(f.events)
            old.mtu(100)
            old.connected()
            old.disconnected()
            runCurrent()
            verifyNoInteractions(f.events)
            assertEquals(23, f.connections.getMtu(ADDR))
            verify(live.gatt, never()).close()
            live.mtu(256)
            runCurrent()
            verify(f.events).onConnectionStateChanged(ADDR, 2, true, 0)
            assertEquals(256, f.connections.getMtu(ADDR))
            f.connections.forceClose(ADDR, 2)
        }

    @Test
    fun retiredMtuContinuationAndQueuedOperationFailureCannotReportAgainstReplacement() =
        runTest {
            val f = Fixture(this)
            f.connections.connect(ADDR, 1)
            val old = f.factory.clients.single()
            old.connected()
            runCurrent()
            old.mtu()
            // The MTU waiter is ready, but its coroutine has not resumed.
            f.connections.readCharacteristic(ADDR, 1, UUID_VALUE.toString())
            f.connections.forceClose(ADDR, 1)
            f.connections.connect(ADDR, 2)
            clearInvocations(f.events)
            runCurrent()
            verifyNoInteractions(f.events)
            verify(old.gatt, never()).readCharacteristic(old.characteristic)
            f.connections.forceClose(ADDR, 2)
        }

    @Test
    fun allLateGattResultsLeaveReplacementOperationsIntact() =
        runTest {
            val f = Fixture(this)
            f.connections.connect(ADDR, 1)
            val old = f.factory.clients.single()
            old.connected()
            runCurrent()
            old.mtu()
            runCurrent()
            f.connections.forceClose(ADDR, 1)
            f.connections.connect(ADDR, 2)
            val live = f.factory.clients.last()
            live.connected()
            runCurrent()
            live.mtu()
            runCurrent()
            clearInvocations(f.events)
            assertEquals(0, f.connections.discoverServices(ADDR, 2))
            runCurrent()
            old.callback.onServicesDiscovered(old.gatt, 0)
            verifyNoInteractions(f.events)
            live.callback.onServicesDiscovered(live.gatt, 0)
            runCurrent()
            verify(
                f.events,
            ).onServicesDiscovered(
                ADDR,
                2,
                "[{\"uuid\":\"$UUID_VALUE\",\"characteristics\":[" +
                    "{\"uuid\":\"$UUID_VALUE\",\"properties\":0}]}]",
            )
            clearInvocations(f.events)
            assertEquals(0, f.connections.readCharacteristic(ADDR, 2, UUID_VALUE.toString()))
            runCurrent()
            old.callback.onCharacteristicRead(old.gatt, old.characteristic, byteArrayOf(1), 0)
            old.callback.onCharacteristicChanged(old.gatt, old.characteristic, byteArrayOf(1))
            verifyNoInteractions(f.events)
            live.callback.onCharacteristicRead(live.gatt, live.characteristic, byteArrayOf(2), 0)
            runCurrent()
            verify(f.events).onCharacteristicRead(ADDR, 2, UUID_VALUE.toString(), byteArrayOf(2), 0)
            clearInvocations(f.events)
            assertEquals(
                0,
                f.connections.writeCharacteristic(
                    ADDR,
                    2,
                    UUID_VALUE.toString(),
                    byteArrayOf(3),
                    BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT,
                ),
            )
            runCurrent()
            old.callback.onCharacteristicWrite(old.gatt, old.characteristic, 0)
            verifyNoInteractions(f.events)
            live.callback.onCharacteristicWrite(live.gatt, live.characteristic, 0)
            runCurrent()
            verify(f.events).onCharacteristicWrite(ADDR, 2, UUID_VALUE.toString(), 0)
            clearInvocations(f.events)
            assertEquals(0, f.connections.subscribeCharacteristic(ADDR, 2, UUID_VALUE.toString()))
            f.connections.readCharacteristic(ADDR, 2, UUID_VALUE.toString())
            runCurrent()
            clearInvocations(live.gatt)
            old.callback.onDescriptorWrite(old.gatt, old.descriptor, 0)
            runCurrent()
            verify(live.gatt, never()).readCharacteristic(live.characteristic)
            live.callback.onDescriptorWrite(live.gatt, live.descriptor, 0)
            runCurrent()
            verify(live.gatt).readCharacteristic(live.characteristic)
            f.connections.forceClose(ADDR, 2)
        }

    @Test
    fun retiredReconnectDelayDoesNotCreateAnotherClient() =
        runTest {
            val f = Fixture(this)
            f.connections.connect(ADDR, 1)
            f.connections.connect(ADDR, 2)
            runCurrent()
            f.connections.forceClose(ADDR, 2)
            advanceTimeBy(300)
            runCurrent()
            assertEquals(1, f.factory.clients.size)
            verify(
                f.factory.clients
                    .single()
                    .gatt,
            ).close()
        }

    @Test
    fun disconnectDuringMtuNeverReportsConnected() =
        runTest {
            val f = Fixture(this)
            f.connections.connect(ADDR, 1)
            val client = f.factory.clients.single()
            client.connected()
            runCurrent()
            f.connections.disconnect(ADDR, 1)
            client.mtu()
            runCurrent()
            verify(f.events, never()).onConnectionStateChanged(ADDR, 1, true, 0)
            client.disconnected()
            verify(f.events).onConnectionStateChanged(ADDR, 1, false, 0)
        }
}
