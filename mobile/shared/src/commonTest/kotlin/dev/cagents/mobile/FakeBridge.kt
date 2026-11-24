package dev.cagents.mobile

import dev.cagents.mobile.client.BridgeTransport
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.LineChannel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ClosedReceiveChannelException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long
import kotlin.test.assertEquals

/**
 * Stands in for SSH and `cagents daemon bridge --mobile`: each connection the
 * app opens arrives at [accept], where the test plays the bridge's part line
 * by line.
 */
class FakeBridge : BridgeTransport {
    private val incoming = Channel<FakeConnection>(Channel.UNLIMITED)

    /** Failures the next opens throw, in order, before connections succeed again. */
    val refusals = ArrayDeque<ConnectFailure>()
    var opens = 0
        private set

    override suspend fun open(): LineChannel {
        opens++
        refusals.removeFirstOrNull()?.let { throw it }
        return FakeConnection().also { incoming.send(it) }.clientSide
    }

    suspend fun accept(): FakeConnection = incoming.receive()

    /** Accepts a connection and completes the handshake. */
    suspend fun acceptWelcomed(): FakeConnection = accept().apply {
        val hello = next()
        assertEquals("hello", hello["type"]!!.jsonPrimitive.content)
        assertEquals(1, hello["api_min"]!!.jsonPrimitive.int)
        send("""{"type":"welcome","api":1,"server_version":"0.2.3","host":"workstation","later_field":true}""")
    }
}

class FakeConnection {
    private val toServer = Channel<String>(Channel.UNLIMITED)
    private val toClient = Channel<String>(Channel.UNLIMITED)

    val clientSide = object : LineChannel {
        override suspend fun readLine(): String? = toClient.receiveCatching().getOrNull()
        override suspend fun writeLine(line: String) {
            toServer.send(line)
        }

        override fun close() {
            toClient.close()
            toServer.close()
        }
    }

    /** The app's next line, as JSON. */
    suspend fun next(): JsonObject = Json.parseToJsonElement(toServer.receive()).jsonObject

    /** The app's next request, answering any ping on the way with `ok`. */
    suspend fun nextRequest(): JsonObject {
        while (true) {
            val line = next()
            assertEquals("request", line["type"]!!.jsonPrimitive.content)
            if (line.op == "ping") ok(line) else return line
        }
    }

    /** Reads requests until one has [op], answering every request on the way with `ok`. */
    suspend fun expect(op: String): JsonObject {
        while (true) {
            val request = nextRequest()
            if (request.op == op) return request
            ok(request)
        }
    }

    fun send(line: String) {
        toClient.trySend(line)
    }

    fun ok(request: JsonObject) = send("""{"type":"ok","id":${request.id}}""")

    fun error(request: JsonObject, code: String, message: String) =
        send("""{"type":"error","id":${request.id},"error":{"code":"$code","message":"$message"}}""")

    /** The bridge exits: the app reads the end of its output. */
    fun hangUp() {
        toClient.close()
    }

    suspend fun isClosedByClient(): Boolean = try {
        toServer.receive()
        false
    } catch (e: ClosedReceiveChannelException) {
        true
    }
}

val JsonObject.op: String get() = this["op"]!!.jsonPrimitive.content
val JsonObject.id: Long get() = this["id"]!!.jsonPrimitive.long
val JsonObject.params: JsonObject get() = this["params"]!!.jsonObject
