package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.Event
import dev.cagents.mobile.protocol.RemoteError
import dev.cagents.mobile.protocol.ServerLine
import dev.cagents.mobile.protocol.Welcome
import dev.cagents.mobile.protocol.decodeServerLine
import dev.cagents.mobile.protocol.encodeHello
import dev.cagents.mobile.protocol.encodeRequest
import dev.cagents.mobile.protocol.WireJson
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.SerializationException

/** The bridge answered a request with an `error`. */
class RequestFailed(val error: RemoteError) : Exception(error.message) {
    val code: ErrorCode get() = error.code
}

/** The connection ended while a request waited for its answer. */
class ConnectionLost(message: String) : Exception(message)

/**
 * One connection's protocol: the handshake, numbered requests and their
 * answers, and the events in between. [run] reads until the connection ends;
 * [call] may be used from any coroutine meanwhile.
 */
class BridgeSession(
    private val channel: LineChannel,
    private val clientName: String,
    private val clientVersion: String,
) {
    private val lock = Mutex()
    private var nextId = 1L
    private val pending = HashMap<Long, CompletableDeferred<Unit>>()
    private var ended: String? = null

    /** Sends `hello` and reads `welcome`, or throws [ConnectFailure.Refused] or [ConnectFailure.Network]. */
    suspend fun handshake(): Welcome {
        channel.writeLine(encodeHello(clientName, clientVersion))
        while (true) {
            val line = channel.readLine()
                ?: throw ConnectFailure.Network("The computer closed the connection before cagents answered.")
            if (line.isBlank()) continue
            return when (val decoded = decode(line)) {
                is ServerLine.WelcomeLine -> decoded.welcome
                is ServerLine.Error -> throw ConnectFailure.Refused(decoded.error.message)
                else -> throw ConnectFailure.Refused("cagents answered unexpectedly; update cagents or the app.")
            }
        }
    }

    /**
     * Sends [call] and waits for its answer. Throws [RequestFailed] for an
     * `error` answer and [ConnectionLost] when the connection ends first.
     */
    suspend fun call(call: Call) {
        val answer = CompletableDeferred<Unit>()
        val id = lock.withLock {
            ended?.let { throw ConnectionLost(it) }
            val id = nextId++
            pending[id] = answer
            id
        }
        try {
            try {
                channel.writeLine(encodeRequest(id, call))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // Half a connection is no connection: end it, so the reader sees it too.
                channel.close()
                throw ConnectionLost(e.message ?: "The connection was lost.")
            }
            answer.await()
        } finally {
            // A caller that stopped waiting, such as a timed-out ping, leaves nothing behind.
            if (!answer.isCompleted) withContext(NonCancellable) { lock.withLock { pending.remove(id) } }
        }
    }

    /**
     * Reads lines until the connection ends, handing each event to [onEvent]
     * in order. Returns why it ended, failing every request still waiting.
     */
    suspend fun run(onEvent: suspend (Event) -> Unit): String {
        val reason = try {
            readLoop(onEvent)
        } catch (e: CancellationException) {
            end("The connection was closed.")
            throw e
        } catch (e: ConnectFailure) {
            end(e.message ?: "cagents sent something this app can't read.")
            throw e
        } catch (e: Exception) {
            e.message ?: "The connection was lost."
        }
        end(reason)
        return reason
    }

    private suspend fun readLoop(onEvent: suspend (Event) -> Unit): String {
        var closing: String? = null
        while (true) {
            val line = channel.readLine() ?: return closing ?: "The computer closed the connection."
            if (line.isBlank()) continue
            val decoded = try {
                decode(line)
            } catch (e: ConnectFailure.Refused) {
                // One unreadable conversation line, such as an item a newer
                // cagents shapes differently, mustn't end the connection; the
                // controller decides what to do about that conversation. Any
                // other line, such as the session list, can't be done without.
                val (session, reset) = conversationOf(line) ?: throw e
                onEvent(Event.Unreadable(session, reset, e.message.orEmpty()))
                continue
            }
            when (decoded) {
                is ServerLine.Ok -> resolve(decoded.id, null)
                is ServerLine.Error -> {
                    val id = decoded.id
                    if (id != null) resolve(id, decoded.error)
                }
                is ServerLine.EventLine -> {
                    val event = decoded.event
                    if (event is Event.Closing) closing = event.message
                    onEvent(event)
                }
                is ServerLine.WelcomeLine, is ServerLine.Unknown -> Unit
            }
        }
    }

    private fun decode(line: String): ServerLine = try {
        decodeServerLine(line)
    } catch (e: SerializationException) {
        throw ConnectFailure.Refused("cagents sent something this app can't read: ${e.message}")
    } catch (e: IllegalArgumentException) {
        throw ConnectFailure.Refused("cagents sent something this app can't read: ${e.message}")
    }

    /** The session and reset flag of a `conversation` event line, if it is one. */
    private fun conversationOf(line: String): Pair<String, Boolean>? = runCatching {
        val root = WireJson.parseToJsonElement(line).jsonObject
        if ((root["event"] as? JsonPrimitive)?.contentOrNull != "conversation") return@runCatching null
        val data = root["data"] as? JsonObject ?: return@runCatching null
        val session = (data["session"] as? JsonPrimitive)?.contentOrNull ?: return@runCatching null
        session to ((data["reset"] as? JsonPrimitive)?.booleanOrNull ?: false)
    }.getOrNull()

    private suspend fun resolve(id: Long, error: RemoteError?) {
        val answer = lock.withLock { pending.remove(id) } ?: return
        if (error == null) answer.complete(Unit) else answer.completeExceptionally(RequestFailed(error))
    }

    private suspend fun end(reason: String) = withContext(NonCancellable) {
        val waiting = lock.withLock {
            ended = reason
            pending.values.toList().also { pending.clear() }
        }
        waiting.forEach { it.completeExceptionally(ConnectionLost(reason)) }
    }

    fun close() = channel.close()
}
