package dev.cagents.mobile

import dev.cagents.mobile.client.BridgeSession
import dev.cagents.mobile.client.LineChannel
import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.Event
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import org.junit.Assume.assumeTrue
import kotlin.test.Test
import kotlin.test.assertEquals

/** Runs the shared client through the Swift transport and the real bridge on fw16. */
class NativeSshFixtureTest {
    @Test
    fun mobileV1HandshakePingAndSessionWatch() {
        val args = System.getenv("CAGENTS_TEST_SSH_COMMAND")
        assumeTrue("Run mobile/ios/test-fixture.py with --shared", args != null)
        val process = ProcessBuilder(args!!.split('\n')).redirectError(ProcessBuilder.Redirect.INHERIT).start()
        try {
            runBlocking {
                withTimeout(15_000) {
                    val session = BridgeSession(ProcessLines(process), "cagents-ios-fixture", "1")
                    assertEquals(1, session.handshake().api)
                    val sessions = CompletableDeferred<Event.Sessions>()
                    val reader = launch {
                        session.run { if (it is Event.Sessions) sessions.complete(it) }
                    }
                    try {
                        session.call(Call.Ping)
                        session.call(Call.WatchSessions)
                        assertEquals(emptyList(), sessions.await().sessions)
                    } finally {
                        session.close()
                        reader.cancelAndJoin()
                    }
                }
            }
        } finally {
            process.destroyForcibly()
        }
    }

    private class ProcessLines(private val process: Process) : LineChannel {
        private val input = process.inputStream.bufferedReader()
        private val output = process.outputStream.bufferedWriter()

        override suspend fun readLine(): String? = withContext(Dispatchers.IO) { input.readLine() }

        override suspend fun writeLine(line: String) = withContext(Dispatchers.IO) {
            output.write(line)
            output.newLine()
            output.flush()
        }

        override fun close() {
            process.destroy()
        }
    }
}
