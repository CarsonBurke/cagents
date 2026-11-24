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
import kotlinx.coroutines.CompletableDeferred
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
            channel.writeLine(encodeRequest(id, call))
        } catch (e: Exception) {
            lock.withLock { pending.remove(id) }
            throw ConnectionLost(e.message ?: "The connection was lost.")
        }
        answer.await()
    }

    /**
     * Reads lines until the connection ends, handing each event to [onEvent]
     * in order. Returns why it ended, failing every request still waiting.
     */
    suspend fun run(onEvent: suspend (Event) -> Unit): String {
        val reason = try {
            readLoop(onEvent)
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) {
                end("The connection was closed.")
                throw e
            }
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
            when (val decoded = decode(line)) {
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

    private suspend fun resolve(id: Long, error: RemoteError?) {
        val answer = lock.withLock { pending.remove(id) } ?: return
        if (error == null) answer.complete(Unit) else answer.completeExceptionally(RequestFailed(error))
    }

    private suspend fun end(reason: String) {
        val waiting = lock.withLock {
            ended = reason
            pending.values.toList().also { pending.clear() }
        }
        waiting.forEach { it.completeExceptionally(ConnectionLost(reason)) }
    }

    fun close() = channel.close()
}
