package org.jakebot.blew

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class AdapterNameLeaseTest {
    private companion object {
        const val USER = "Jake's Pixel"
        const val BEACON = "RO3JHAAY"
    }

    /** Applies renames only when told to, as the stack does asynchronously. */
    private class FakeAdapter : AdapterNames {
        var name: String? = USER
        var accepts = true
        val queued = ArrayDeque<String>()
        val requests = mutableListOf<String>()

        override fun get(): String? = name

        override fun set(name: String): Boolean {
            if (!accepts) return false
            requests.add(name)
            queued.addLast(name)
            return true
        }
    }

    private class FakeStore(
        var saved: AdapterNameLease.Borrow? = null,
    ) : BorrowedNameStore {
        override fun load() = saved

        override fun save(borrow: AdapterNameLease.Borrow?) {
            saved = borrow
        }
    }

    private class Fixture(
        scope: TestScope,
        val store: FakeStore = FakeStore(),
    ) {
        val adapter = FakeAdapter()
        val lease = AdapterNameLease(adapter, store, scope.backgroundScope)
        var advertised = 0

        fun acquire(name: String = BEACON) = lease.acquire(name) { advertised++ }

        /** The stack applies the oldest pending rename and broadcasts it. */
        fun land() {
            val next = adapter.queued.removeFirst()
            adapter.name = next
            lease.onNameChanged(next)
        }

        /** Someone other than blew renames the adapter. */
        fun renamedElsewhere(name: String) {
            adapter.name = name
            lease.onNameChanged(name)
        }
    }

    @Test
    fun advertisesOnlyOnceTheNameHasLandedAndGivesItBackAfter() =
        runTest {
            val f = Fixture(this)
            val ticket = f.acquire()!!
            assertEquals(0, f.advertised)
            assertEquals(AdapterNameLease.Borrow(USER, BEACON), f.store.saved)

            f.land()
            assertEquals(1, f.advertised)

            f.lease.release(ticket)
            assertEquals(listOf(BEACON, USER), f.adapter.requests)
            assertNotNull("kept until the restore lands", f.store.saved)
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(f.store.saved)
        }

    @Test
    fun advertisesAfterTheBackstopWhenNoBroadcastArrives() =
        runTest {
            val f = Fixture(this)
            f.acquire()
            f.adapter.name = f.adapter.queued.removeFirst()
            advanceTimeBy(999)
            runCurrent()
            assertEquals(0, f.advertised)
            advanceTimeBy(1)
            runCurrent()
            assertEquals(1, f.advertised)
        }

    @Test
    fun aNameAlreadyInPlaceIsNeitherSetNorRestored() =
        runTest {
            val f = Fixture(this)
            f.adapter.name = BEACON
            val ticket = f.acquire()!!
            assertEquals(1, f.advertised)
            f.lease.release(ticket)
            assertTrue(f.adapter.requests.isEmpty())
            assertNull(f.store.saved)
        }

    @Test
    fun releasingBeforeTheNameLandsNeverAdvertisesAndStillRestores() =
        runTest {
            val f = Fixture(this)
            val ticket = f.acquire()!!
            f.lease.release(ticket)
            f.land()
            advanceTimeBy(5_000)
            runCurrent()
            assertEquals(0, f.advertised)
            assertEquals(listOf(BEACON, USER), f.adapter.requests)
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(f.store.saved)
        }

    @Test
    fun aRefusedRenameChangesNothing() =
        runTest {
            val f = Fixture(this)
            f.adapter.accepts = false
            assertNull(f.acquire())
            assertEquals(0, f.advertised)
            assertNull(f.store.saved)
        }

    @Test
    fun aRestoreRefusedWhileTheAdapterIsOffWaitsForReconcile() =
        runTest {
            val f = Fixture(this)
            val ticket = f.acquire()!!
            f.land()
            f.adapter.accepts = false
            f.lease.release(ticket)
            assertEquals(BEACON, f.adapter.name)
            assertNotNull(f.store.saved)

            f.adapter.accepts = true
            f.lease.reconcile()
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(f.store.saved)
        }

    @Test
    fun aNameChosenElsewhereWhileAdvertisingIsLeftAlone() =
        runTest {
            val f = Fixture(this)
            val ticket = f.acquire()!!
            f.land()
            f.renamedElsewhere("Renamed in Settings")
            f.lease.release(ticket)
            assertEquals("Renamed in Settings", f.adapter.name)
            assertEquals(listOf(BEACON), f.adapter.requests)
        }

    @Test
    fun whenAnotherAppPutsOurNameBackTheOriginalFollows() =
        runTest {
            val f = Fixture(this)
            val ticket = f.acquire()!!
            f.land()
            // Another app borrows the name from us, and we stop while it holds it.
            f.renamedElsewhere("OTHER-BEACON")
            f.lease.release(ticket)
            assertEquals("OTHER-BEACON", f.adapter.name)

            // It gives back what it took, which was our beacon name.
            f.renamedElsewhere(BEACON)
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(f.store.saved)
        }

    @Test
    fun aNameBorrowedByAPreviousProcessIsGivenBackAtStartup() =
        runTest {
            val store = FakeStore(AdapterNameLease.Borrow(USER, BEACON))
            val f = Fixture(this, store)
            f.adapter.name = BEACON
            f.lease.reconcile()
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(store.saved)
        }

    @Test
    fun aNameBorrowedByAPreviousProcessIsResumedNotRecordedAsTheirs() =
        runTest {
            val store = FakeStore(AdapterNameLease.Borrow(USER, BEACON))
            val f = Fixture(this, store)
            f.adapter.name = BEACON
            val ticket = f.acquire("NEWBEACON")!!
            assertEquals(AdapterNameLease.Borrow(USER, "NEWBEACON"), store.saved)
            f.land()
            f.lease.release(ticket)
            f.land()
            assertEquals(USER, f.adapter.name)
        }

    @Test
    fun restartingWhileTheRestoreIsInFlightKeepsTheOriginal() =
        runTest {
            val f = Fixture(this)
            val first = f.acquire()!!
            f.land()
            f.lease.release(first)
            // The restore to USER is queued but hasn't landed; the adapter still reads BEACON.
            val second = f.acquire("NEWBEACON")!!
            assertEquals(AdapterNameLease.Borrow(USER, "NEWBEACON"), f.store.saved)
            f.land()
            assertEquals("the restore landing isn't this holder's name", 1, f.advertised)
            f.land()
            assertEquals(2, f.advertised)
            f.lease.release(second)
            f.land()
            assertEquals(USER, f.adapter.name)
            assertNull(f.store.saved)
        }

    @Test
    fun aStaleTicketCannotReleaseItsSuccessor() =
        runTest {
            val f = Fixture(this)
            val first = f.acquire()!!
            f.land()
            f.lease.release(first)
            f.land()
            val second = f.acquire()!!
            f.land()
            f.lease.release(first)
            assertEquals(BEACON, f.adapter.name)
            assertTrue(f.adapter.queued.isEmpty())
            f.lease.release(second)
            f.land()
            assertEquals(USER, f.adapter.name)
        }
}
