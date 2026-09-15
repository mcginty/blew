package org.jakebot.blew

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.atomic.AtomicLong
import kotlin.coroutines.CoroutineContext

/**
 * Per-device GATT operation queue.
 *
 * Android's framework already serializes GATT operations per BluetoothGatt instance
 * (mDeviceBusy). This queue adds:
 *  - per-operation timeout (guards against OEM firmware bugs where callbacks never fire),
 *  - cancel-on-disconnect (on close() all pending ops fail with a single stable error),
 *  - FIFO guarantee.
 *
 * Each device gets its own instance; a single worker coroutine drains the queue serially.
 */
class GattOperationQueue(
    tag: String,
    context: CoroutineContext = Dispatchers.Default,
) {
    private val scope = CoroutineScope(context + SupervisorJob(context[Job]))
    private val channel = Channel<Task<*>>(capacity = Channel.UNLIMITED)
    private val worker: Job
    private val taskSeq = AtomicLong(0)
    private val lock = Any()
    private var closed: Throwable? = null
    private val tasks = mutableSetOf<Task<*>>()

    @Volatile private var current: Task<*>? = null

    fun currentNonce(): Long? = current?.nonce

    init {
        worker =
            scope.launch {
                for (task in channel) {
                    task.run()
                }
            }
    }

    suspend fun <T> enqueue(
        name: String,
        timeoutMs: Long,
        kick: () -> Boolean,
        onComplete: (Task<T>) -> Unit = {},
    ): Result<T> {
        val task = Task<T>(name, timeoutMs, kick, onComplete)
        synchronized(lock) {
            closed?.let { return Result.failure(it) }
            tasks.add(task)
            channel.trySend(task).getOrThrow()
        }
        return try {
            task.await()
        } finally {
            synchronized(lock) { tasks.remove(task) }
        }
    }

    @Suppress("UNCHECKED_CAST")
    fun <T> completeCurrent(
        nonce: Long,
        value: T,
    ) {
        val task = current ?: return
        if (task.nonce != nonce) return
        (task as Task<T>).complete(value)
    }

    fun close(reason: Throwable = CancellationException("queue closed")) {
        val pending =
            synchronized(lock) {
                if (closed != null) return
                closed = reason
                channel.close()
                tasks.toList().also { tasks.clear() }
            }
        pending.forEach { it.fail(reason) }
        scope.cancel()
    }

    inner class Task<T>(
        val name: String,
        private val timeoutMs: Long,
        private val kick: () -> Boolean,
        private val onComplete: (Task<T>) -> Unit,
    ) {
        val nonce: Long = taskSeq.getAndIncrement()
        private val deferred = CompletableDeferred<Result<T>>()

        suspend fun run() {
            if (deferred.isCompleted) return
            current = this
            try {
                if (!kick()) {
                    deferred.complete(Result.failure(IllegalStateException("kick returned false for $name")))
                    return
                }
                val result = withTimeoutOrNull(timeoutMs) { deferred.await() }
                if (result == null) {
                    deferred.complete(Result.failure(RuntimeException("$name timed out after ${timeoutMs}ms")))
                }
            } catch (e: Exception) {
                deferred.complete(Result.failure(e))
                if (e is CancellationException) throw e
            } finally {
                current = null
                onComplete(this)
            }
        }

        fun complete(value: T) {
            deferred.complete(Result.success(value))
        }

        fun fail(t: Throwable) {
            deferred.complete(Result.failure(t))
        }

        suspend fun await(): Result<T> = deferred.await()
    }
}
