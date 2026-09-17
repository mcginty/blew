package org.jakebot.blew

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class AdapterRenameTest {
    private companion object {
        const val USER = "Жанна Сергеевна Кузнецова-Иванова"
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

    private class Fixture(
        val scope: TestScope,
    ) {
        val adapter = FakeAdapter()
        val rename = AdapterRename(adapter, scope.backgroundScope)
        var ready = 0
        var failed = 0

        fun request(name: String = BEACON) = rename.request(name, { ready++ }, { failed++ })

        /** A request with outcomes of its own, for tests where several overlap. */
        inner class Waiter(
            name: String,
        ) {
            var ready = 0
            var failed = 0
            val ticket = rename.request(name, { ready++ }, { failed++ })!!
        }

        /** The stack applies the oldest pending rename and broadcasts it. */
        fun land() {
            val next = adapter.queued.removeFirst()
            adapter.name = next
            rename.onNameChanged(next)
        }

        fun elapse(ms: Long) {
            scope.advanceTimeBy(ms)
            scope.runCurrent()
        }
    }

    @Test
    fun advertisesOnlyOnceTheRenameHasLanded() =
        runTest {
            val f = Fixture(this)
            assertNotNull(f.request())
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
            f.elapse(5_000)
            assertEquals(1, f.ready)
            assertEquals(0, f.failed)
            assertEquals("the rename is never undone", listOf(BEACON), f.adapter.requests)
            assertEquals(BEACON, f.adapter.name)
        }

    @Test
    fun advertisesAfterTheBackstopWhenTheNameLandedWithoutABroadcast() =
        runTest {
            val f = Fixture(this)
            f.request()
            f.adapter.name = f.adapter.queued.removeFirst()
            f.elapse(999)
            assertEquals(0, f.ready)
            f.elapse(1)
            assertEquals(1, f.ready)
            assertEquals(0, f.failed)
        }

    @Test
    fun failsInsteadOfAdvertisingThePreviousNameWhenTheRenameNeverLands() =
        runTest {
            val f = Fixture(this)
            f.request()
            f.elapse(1_000)
            assertEquals(0, f.ready)
            assertEquals(1, f.failed)
            // A broadcast arriving late doesn't resurrect the start.
            f.land()
            f.elapse(5_000)
            assertEquals(0, f.ready)
            assertEquals(1, f.failed)
        }

    @Test
    fun aNameAlreadyInPlaceAdvertisesImmediately() =
        runTest {
            val f = Fixture(this)
            f.adapter.name = BEACON
            assertNotNull(f.request())
            assertEquals(1, f.ready)
            assertTrue(f.adapter.requests.isEmpty())
            f.elapse(5_000)
            assertEquals(0, f.failed)
        }

    @Test
    fun cancellingBeforeTheRenameLandsNeitherAdvertisesNorFails() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.request()!!)
            f.land()
            f.elapse(5_000)
            assertEquals(0, f.ready)
            assertEquals(0, f.failed)

            val g = Fixture(this)
            g.rename.cancel(g.request()!!)
            g.elapse(5_000)
            assertEquals("a cancelled start isn't failed either", 0, g.failed)
        }

    @Test
    fun aRefusedRenameCallsNeitherCallback() =
        runTest {
            val f = Fixture(this)
            f.adapter.accepts = false
            assertNull(f.request())
            f.elapse(5_000)
            assertEquals(0, f.ready)
            assertEquals(0, f.failed)
        }

    @Test
    fun aRenameStillInFlightIsWaitedForRatherThanRepeated() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.request()!!)
            // The adapter still reads the old name, but the rename is on its way.
            f.request()
            assertEquals(listOf(BEACON), f.adapter.requests)
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun onlyTheLastRenameInFlightReleasesTheStart() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.request("FIRST")!!)
            f.request()
            f.land()
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun anUnrelatedRenameDoesNotReleaseTheStart() =
        runTest {
            val f = Fixture(this)
            f.request()
            f.adapter.name = "Renamed in Settings"
            f.rename.onNameChanged("Renamed in Settings")
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun anEarlierRenamesBackstopDoesNotFailALaterStart() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.request("FIRST")!!)
            f.elapse(500)
            f.request()
            f.elapse(500)
            assertEquals(0, f.failed)
            f.land()
            f.land()
            assertEquals(1, f.ready)
            f.elapse(5_000)
            assertEquals(0, f.failed)
        }

    @Test
    fun aLaterRenameFailsAnEarlierWaiterOnceItLands() =
        runTest {
            val f = Fixture(this)
            val advertisement = f.Waiter(BEACON)
            val application = f.Waiter(USER)
            f.land()
            assertEquals("nothing settles while a rename is still queued", 0, advertisement.failed)
            assertEquals(0, application.ready)
            f.land()
            assertEquals(1, application.ready)
            assertEquals(0, advertisement.ready)
            assertEquals(1, advertisement.failed)
            f.elapse(5_000)
            assertEquals(1, advertisement.failed)
            assertEquals(0, application.failed)
        }

    @Test
    fun waitersForTheSameNameAreAllReleased() =
        runTest {
            val f = Fixture(this)
            val first = f.Waiter(BEACON)
            val second = f.Waiter(BEACON)
            assertEquals(listOf(BEACON), f.adapter.requests)
            f.land()
            assertEquals(1, first.ready)
            assertEquals(1, second.ready)
        }

    @Test
    fun cancellingOneWaiterLeavesTheOthers() =
        runTest {
            val f = Fixture(this)
            val cancelled = f.Waiter(BEACON)
            val kept = f.Waiter(BEACON)
            f.rename.cancel(cancelled.ticket)
            f.land()
            assertEquals(0, cancelled.ready)
            assertEquals(1, kept.ready)
        }

    @Test
    fun anUnconfirmedRenameFailsEveryWaiter() =
        runTest {
            val f = Fixture(this)
            val advertisement = f.Waiter(BEACON)
            val application = f.Waiter(BEACON)
            f.elapse(1_000)
            assertEquals(1, advertisement.failed)
            assertEquals(1, application.failed)
            assertEquals(0, advertisement.ready + application.ready)
        }
}
