package dev.cagents.mobile

import dev.cagents.mobile.client.AllFailed
import dev.cagents.mobile.client.firstToSucceed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.currentTime
import kotlinx.coroutines.test.runTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.milliseconds
import kotlin.time.Duration.Companion.seconds

@OptIn(ExperimentalCoroutinesApi::class)
class FirstToSucceedTest {
    private val stagger = 1.seconds

    @Test
    fun theFirstCandidateWinsWhenItAnswers() = runTest {
        val tried = mutableListOf<String>()
        val winner = firstToSucceed(listOf("tailscale", "lan"), stagger) { host ->
            tried += host
            delay(200.milliseconds)
            host
        }
        assertEquals("tailscale", winner)
        assertEquals(listOf("tailscale"), tried)
    }

    @Test
    fun aSilentCandidateCostsOnlyTheStagger() = runTest {
        val cancelled = mutableListOf<String>()
        val winner = firstToSucceed(listOf("tailscale", "lan"), stagger) { host ->
            if (host == "tailscale") {
                try {
                    awaitCancellation()
                } catch (e: CancellationException) {
                    cancelled += host
                    throw e
                }
            }
            delay(100.milliseconds)
            host
        }
        assertEquals("lan", winner)
        assertEquals(1100, currentTime)
        // The loser isn't waited for, but it is cancelled.
        runCurrent()
        assertEquals(listOf("tailscale"), cancelled)
    }

    @Test
    fun aFailureStartsTheNextAtOnce() = runTest {
        val winner = firstToSucceed(listOf("a", "b", "c"), stagger) { host ->
            delay(10.milliseconds)
            if (host != "c") error("no route to $host")
            host
        }
        assertEquals("c", winner)
        assertEquals(30, currentTime)
    }

    @Test
    fun aLateSuccessIsDiscarded() = runTest {
        val discarded = mutableListOf<String>()
        val winner = firstToSucceed(
            listOf("slow", "fast"),
            stagger,
            discard = { discarded += it },
        ) { host ->
            // Both answer at 1.5 s: the second is already in hand when the first wins.
            delay(if (host == "slow") 1_500.milliseconds else 500.milliseconds)
            host
        }
        assertEquals("slow", winner)
        assertEquals(listOf("fast"), discarded)
    }

    @Test
    fun aLoserThatCannotBeInterruptedDoesNotHoldUpTheWinner() = runTest {
        val discarded = mutableListOf<String>()
        val winner = firstToSucceed(listOf("stuck", "lan"), stagger, discard = { discarded += it }) { host ->
            // Like a DNS lookup: cancelling doesn't reach it.
            withContext(NonCancellable) { delay(if (host == "stuck") 10.seconds else 100.milliseconds) }
            host
        }
        assertEquals("lan", winner)
        assertEquals(1100, currentTime)
        advanceUntilIdle()
        assertEquals(listOf("stuck"), discarded)
    }

    @Test
    fun cancellingTheCallerDisposesOfASuccessAlreadyIn() = runTest {
        val discarded = mutableListOf<String>()
        lateinit var race: Job
        race = launch {
            firstToSucceed<String, String>(listOf("a"), stagger, discard = { discarded += it }) { host ->
                delay(100.milliseconds)
                // The caller is cancelled just as the connection is made.
                race.cancel()
                host
            }
        }
        advanceUntilIdle()
        assertTrue(race.isCancelled)
        assertEquals(listOf("a"), discarded)
    }

    @Test
    fun anAttemptsOwnTimeoutCountsAsAFailure() = runTest {
        val winner = firstToSucceed(listOf("a", "b"), stagger) { host ->
            if (host == "a") withTimeout(10.milliseconds) { awaitCancellation() }
            host
        }
        assertEquals("b", winner)
        assertEquals(10, currentTime)
    }

    @Test
    fun everyFailureIsReportedInOrder() = runTest {
        val failed = assertFailsWith<AllFailed> {
            firstToSucceed(listOf("a", "b"), stagger) { host ->
                delay(if (host == "a") 2_000.milliseconds else 10.milliseconds)
                error(host)
            }
        }
        assertEquals(listOf("a", "b"), failed.failures.map { it.message })
    }
}
