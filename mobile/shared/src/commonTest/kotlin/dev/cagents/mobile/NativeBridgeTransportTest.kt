package dev.cagents.mobile

import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.NativeBridgeTransport
import dev.cagents.mobile.client.NativeSshChannel
import dev.cagents.mobile.client.NativeSshOpening
import dev.cagents.mobile.client.NativeSshOperation
import dev.cagents.mobile.client.NativeSshReading
import dev.cagents.mobile.client.NativeSshWriting
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.test.runTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class NativeBridgeTransportTest {
    @Test
    fun aConnectionThatArrivesAfterCancellationIsClosed() = runTest {
        lateinit var opening: NativeSshOpening
        var cancelled = false
        val transport = NativeBridgeTransport { completion ->
            opening = completion
            NativeSshOperation { cancelled = true }
        }
        val connecting = async(start = CoroutineStart.UNDISPATCHED) { transport.open() }
        connecting.cancelAndJoin()
        assertTrue(cancelled)

        val source = TestChannel()
        opening.opened(source)
        assertTrue(source.closed)
    }

    @Test
    fun keyMismatchRemainsAnActionableConnectionFailure() = runTest {
        val transport = NativeBridgeTransport { opening ->
            opening.hostKeyChanged("new", "old", "computer")
            NativeSshOperation {}
        }
        val error = assertFailsWith<ConnectFailure.HostKeyChanged> { transport.open() }
        assertEquals("new", error.fingerprint)
        assertEquals("old", error.pinned)
        assertEquals("computer", error.host)
    }

    @Test
    fun cancellingAReadClosesTheNativeStream() = runTest {
        val source = TestChannel()
        val connection = NativeBridgeTransport {
            it.opened(source)
            NativeSshOperation {}
        }.open()
        val reader = async(start = CoroutineStart.UNDISPATCHED) { connection.readLine() }
        reader.cancelAndJoin()
        assertTrue(source.closed)
    }

    private class TestChannel : NativeSshChannel {
        var closed = false

        override fun read(completion: NativeSshReading) = Unit
        override fun write(line: String, completion: NativeSshWriting) = completion.written()
        override fun close() { closed = true }
    }
}
