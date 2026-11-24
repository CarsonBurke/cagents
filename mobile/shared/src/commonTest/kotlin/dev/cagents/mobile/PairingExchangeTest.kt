package dev.cagents.mobile

import dev.cagents.mobile.pairing.PairOutcome
import dev.cagents.mobile.pairing.PairingExchange
import kotlinx.coroutines.async
import kotlinx.coroutines.test.runTest
import kotlinx.serialization.json.jsonPrimitive
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class PairingExchangeTest {
    private val key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6QFlFdL7Pg8Y/hI2km8NKJTUX2ybmLopqs+i19BSkc"

    private suspend fun FakeConnection.expectRequest() {
        val request = next()
        assertEquals("pair", request["type"]!!.jsonPrimitive.content)
        assertEquals(key, request["key"]!!.jsonPrimitive.content)
        assertEquals("Pixel 9", request["name"]!!.jsonPrimitive.content)
    }

    @Test
    fun sendsTheRequestThenWaitsForThePersonToAllow() = runTest {
        val connection = FakeConnection()
        var waited = 0
        val outcome = async { PairingExchange.pair(connection.clientSide, "$key\n", " Pixel 9 ") { waited++ } }
        connection.expectRequest()
        connection.send("""{"type":"pair_waiting"}""")
        connection.send("""{"type":"pair_waiting","later":true}""")
        connection.send("""{"type":"pair_progress"}""")
        connection.send("""{"type":"paired","host":"workstation"}""")
        assertEquals(PairOutcome.Paired("workstation"), outcome.await())
        assertEquals(1, waited)
    }

    @Test
    fun pairedWithoutAHostName() = runTest {
        val connection = FakeConnection()
        val outcome = async { PairingExchange.pair(connection.clientSide, key, "Pixel 9") {} }
        connection.expectRequest()
        connection.send("""{"type":"pair_waiting"}""")
        connection.send("""{"type":"paired"}""")
        assertEquals(PairOutcome.Paired(null), outcome.await())
    }

    @Test
    fun declined() = runTest {
        val connection = FakeConnection()
        val outcome = async { PairingExchange.pair(connection.clientSide, key, "Pixel 9") {} }
        connection.expectRequest()
        connection.send("""{"type":"pair_waiting"}""")
        connection.send("""{"type":"pair_declined"}""")
        assertEquals(PairOutcome.Declined, outcome.await())
    }

    @Test
    fun anErrorMayComeInsteadOfWaiting() = runTest {
        val connection = FakeConnection()
        var waited = false
        val outcome = async { PairingExchange.pair(connection.clientSide, key, "Pixel 9") { waited = true } }
        connection.expectRequest()
        connection.send("""{"type":"pair_error","message":"This pairing code was already used. Show a new one on the computer."}""")
        assertEquals(PairOutcome.Failed("This pairing code was already used. Show a new one on the computer."), outcome.await())
        assertFalse(waited)
    }

    @Test
    fun theComputerHangingUpEndsIt() = runTest {
        for (waitFirst in listOf(false, true)) {
            val connection = FakeConnection()
            val outcome = async { PairingExchange.pair(connection.clientSide, key, "Pixel 9") {} }
            connection.expectRequest()
            if (waitFirst) connection.send("""{"type":"pair_waiting"}""")
            connection.hangUp()
            val failed = outcome.await()
            assertTrue(failed is PairOutcome.Failed && failed.message.isNotBlank(), "$failed")
        }
    }

    @Test
    fun anUnreadableAnswerEndsIt() = runTest {
        val connection = FakeConnection()
        val outcome = async { PairingExchange.pair(connection.clientSide, key, "Pixel 9") {} }
        connection.expectRequest()
        connection.send("not json")
        assertTrue(outcome.await() is PairOutcome.Failed)
    }

    @Test
    fun theRequestKeepsANameTheComputerWillKeep() {
        val line = PairingExchange.requestLine(key, "x".repeat(100))
        assertTrue(""""name":"${"x".repeat(PairingExchange.MAX_DEVICE_NAME_CHARS)}"""" in line)
        assertTrue(line.startsWith("""{"type":"pair","""))
    }
}
