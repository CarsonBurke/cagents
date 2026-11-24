package dev.cagents.mobile

import dev.cagents.mobile.client.AllFailed
import dev.cagents.mobile.client.firstToSucceed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.currentTime
import kotlinx.coroutines.test.runTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
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
