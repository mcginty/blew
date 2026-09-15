package org.jakebot.blew

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class GattOperationQueueTest {
    @Test
    fun closeBeforeWorkerStartsReleasesQueuedWaiter() =
        runTest {
            val queue = GattOperationQueue("test", backgroundScope.coroutineContext)
            var kicked = false
            val result =
                backgroundScope.async(start = CoroutineStart.UNDISPATCHED) {
                    queue.enqueue<Unit>("pending", 5000L, kick = {
                        kicked = true
                        true
                    })
                }
            queue.close()
            runCurrent()
            assertTrue(result.await().isFailure)
            assertFalse(kicked)
        }

    @Test
    fun closeReleasesBothCurrentAndQueuedOperations() =
        runTest {
            val queue = GattOperationQueue("test", backgroundScope.coroutineContext)
            var kicks = 0
            val first =
                backgroundScope.async {
                    queue.enqueue<Unit>("first", 5000L, kick = {
                        kicks++
                        true
                    })
                }
            val second =
                backgroundScope.async {
                    queue.enqueue<Unit>("second", 5000L, kick = {
                        kicks++
                        true
                    })
                }
            runCurrent()
            assertEquals(1, kicks)
            queue.close()
            runCurrent()
            assertTrue(first.await().isFailure)
            assertTrue(second.await().isFailure)
            assertEquals(1, kicks)
        }

    @Test
    fun platformExceptionFailsOperationWithoutKillingQueue() =
        runTest {
            val queue = GattOperationQueue("test", backgroundScope.coroutineContext)
            val first =
                backgroundScope.async {
                    queue.enqueue<Unit>("throws", 5000L, kick = { error("platform failure") })
                }
            val second =
                backgroundScope.async {
                    queue.enqueue<Unit>("next", 5000L, kick = {
                        queue.completeCurrent(queue.currentNonce()!!, Unit)
                        true
                    })
                }
            runCurrent()
            assertTrue(first.await().isFailure)
            assertTrue(second.await().isSuccess)
            queue.close()
        }
}
