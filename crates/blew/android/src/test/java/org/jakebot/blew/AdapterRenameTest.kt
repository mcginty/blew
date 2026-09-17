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

        fun accepted(name: String = BEACON) = (request(name) as AdapterRename.Outcome.Accepted).ticket

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
            f.accepted()
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
            f.accepted()
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
            f.accepted()
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
    fun aNameAlreadyInPlaceIsReadyImmediately() =
        runTest {
            val f = Fixture(this)
            f.adapter.name = BEACON
            f.accepted()
            assertEquals(1, f.ready)
            assertTrue(f.adapter.requests.isEmpty())
            f.elapse(5_000)
            assertEquals(0, f.failed)
        }

    @Test
    fun cancellingBeforeTheRenameLandsNeitherReadiesNorFails() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.accepted())
            f.land()
            f.elapse(5_000)
            assertEquals(0, f.ready)
            assertEquals(0, f.failed)

            val g = Fixture(this)
            g.rename.cancel(g.accepted())
            g.elapse(5_000)
            assertEquals("a cancelled request isn't failed either", 0, g.failed)
        }

    @Test
    fun aRefusedRenameCallsNeitherCallback() =
        runTest {
            val f = Fixture(this)
            f.adapter.accepts = false
            assertSame(AdapterRename.Outcome.Refused, f.request())
            f.elapse(5_000)
            assertEquals(0, f.ready)
            assertEquals(0, f.failed)
        }

    @Test
    fun aRenameStillInFlightIsWaitedForRatherThanRepeated() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.accepted())
            // The adapter still reads the old name, but the rename is on its way.
            f.accepted()
            assertEquals(listOf(BEACON), f.adapter.requests)
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun onlyTheLastRenameAskedForReleasesTheRequest() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.accepted("FIRST"))
            f.accepted()
            f.land()
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun anUnrelatedRenameDoesNotReleaseTheRequest() =
        runTest {
            val f = Fixture(this)
            f.accepted()
            f.adapter.name = "Renamed in Settings"
            f.rename.onNameChanged("Renamed in Settings")
            assertEquals(0, f.ready)
            f.land()
            assertEquals(1, f.ready)
        }

    @Test
    fun anEarlierRenamesBackstopDoesNotFailALaterRequest() =
        runTest {
            val f = Fixture(this)
            f.rename.cancel(f.accepted("FIRST"))
            f.elapse(500)
            f.accepted()
            f.elapse(500)
            assertEquals(0, f.failed)
            f.land()
            f.land()
            assertEquals(1, f.ready)
            f.elapse(5_000)
            assertEquals(0, f.failed)
        }

    @Test
    fun aSecondRequestWhileOneWaitsIsRefusedAndLeavesTheFirstAlone() =
        runTest {
            val f = Fixture(this)
            f.accepted()
            var secondCalled = false
            for (name in listOf(BEACON, USER)) {
                val outcome = f.rename.request(name, { secondCalled = true }, { secondCalled = true })
                assertSame(AdapterRename.Outcome.Busy, outcome)
            }
            assertEquals("a refused request renames nothing", listOf(BEACON), f.adapter.requests)
            f.land()
            f.elapse(5_000)
            assertEquals(1, f.ready)
            assertEquals(0, f.failed)
            assertFalse(secondCalled)
        }

    @Test
    fun theNextRequestIsAcceptedOnceTheWaiterIsResolved() =
        runTest {
            val f = Fixture(this)
            f.accepted()
            f.land()
            f.accepted(USER)
            f.land()
            assertEquals(2, f.ready)

            val g = Fixture(this)
            g.accepted()
            g.elapse(1_000)
            assertEquals(1, g.failed)
            assertTrue("a failed waiter frees the slot", g.request(USER) is AdapterRename.Outcome.Accepted)
        }
}
