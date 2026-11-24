package dev.cagents.mobile.client

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.cancelChildren
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlin.time.Duration

/** Every candidate failed; [failures] are in the candidates' order. */
class AllFailed(val failures: List<Throwable>) : Exception(failures.firstOrNull()?.message, failures.firstOrNull())

/**
 * Tries [candidates] in order, as happy eyeballs does: each starts when the
 * one before it fails or has had [stagger] without an answer, and the first
 * to succeed wins. The others are cancelled; one that succeeds anyway is
 * handed to [discard]. Throws [AllFailed] when none succeeds.
 *
 * A computer's addresses are tried this way so that a preferred one that is
 * unreachable from where the phone is, such as a Tailscale address with
 * Tailscale off, costs [stagger] rather than a full connection timeout.
 */
suspend fun <C, T> firstToSucceed(
    candidates: List<C>,
    stagger: Duration,
    discard: (T) -> Unit = {},
    attempt: suspend (C) -> T,
): T = coroutineScope {
    require(candidates.isNotEmpty()) { "Nothing to try" }
    val events = Channel<Event<T>>(Channel.UNLIMITED)
    val failures = arrayOfNulls<Throwable>(candidates.size)
    var started = 0
    var failed = 0

    fun startNext() {
        val index = started++
        launch {
            val result = try {
                Result.success(attempt(candidates[index]))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Throwable) {
                Result.failure(e)
            }
            if (events.trySend(Event.Done(index, result)).isFailure) result.onSuccess(discard)
        }
        if (started < candidates.size) {
            val next = started
            launch {
                delay(stagger)
                events.trySend(Event.Waited(next))
            }
        }
    }

    val scope = this
    suspend fun winner(): T {
        startNext()
        while (true) {
            when (val event = events.receive()) {
                // Only the timer of the attempt that is next still counts.
                is Event.Waited -> if (event.next == started) startNext()
                is Event.Done -> event.result.fold(
                    onSuccess = { winner ->
                        events.close()
                        // Results that arrived before closing still need disposing of.
                        while (true) {
                            val late = events.tryReceive().getOrNull() ?: break
                            if (late is Event.Done) late.result.onSuccess(discard)
                        }
                        scope.coroutineContext.cancelChildren()
                        return winner
                    },
                    onFailure = { error ->
                        failures[event.index] = error
                        failed++
                        if (failed == candidates.size) {
                            scope.coroutineContext.cancelChildren()
                            throw AllFailed(failures.map { it!! })
                        }
                        if (started < candidates.size) startNext()
                    },
                )
            }
        }
    }
    winner()
}

private sealed interface Event<out T> {
    class Done<T>(val index: Int, val result: Result<T>) : Event<T>
    class Waited(val next: Int) : Event<Nothing>
}
