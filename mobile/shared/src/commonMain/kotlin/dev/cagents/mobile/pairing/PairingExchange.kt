package dev.cagents.mobile.pairing

import dev.cagents.mobile.client.LineChannel
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put

/** How a pairing request ended. */
sealed interface PairOutcome {
    /** The computer authorized the phone's key. [host] is its host name, when it said. */
    data class Paired(val host: String?) : PairOutcome

    /** The person chose not to allow the phone. */
    data object Declined : PairOutcome

    /** Pairing didn't happen; [message] is written for the person. */
    data class Failed(val message: String) : PairOutcome
}

/**
 * The phone's side of `cagents daemon bridge --pair`, over a connection made
 * with the code's one-time key: one `pair` line out, then `pair_waiting` and
 * the decision back. Closing [channel] before the decision withdraws the
 * request, which is what cancelling this does.
 */
object PairingExchange {
    /** The longest device name the computer keeps. */
    const val MAX_DEVICE_NAME_CHARS = 48

    /** The line the phone sends: its own OpenSSH public key, without a comment, and its name. */
    fun requestLine(publicKey: String, deviceName: String): String = buildJsonObject {
        put("type", "pair")
        put("key", publicKey.trim())
        put("name", deviceName.trim().take(MAX_DEVICE_NAME_CHARS))
    }.toString()

    /**
     * Sends the request and waits for the person's decision, calling
     * [onWaiting] once the computer is asking them. Lines it doesn't know
     * are skipped, as the API's messages may grow.
     */
    suspend fun pair(
        channel: LineChannel,
        publicKey: String,
        deviceName: String,
        onWaiting: () -> Unit,
    ): PairOutcome {
        channel.writeLine(requestLine(publicKey, deviceName))
        var waiting = false
        while (true) {
            val line = channel.readLine() ?: return PairOutcome.Failed(
                if (waiting) {
                    "The computer stopped asking before anyone answered. Show a new code there."
                } else {
                    "The computer closed the connection without answering. Show a new code there and try again."
                },
            )
            val message = runCatching { Json.parseToJsonElement(line).jsonObject }.getOrNull()
                ?: return PairOutcome.Failed("The computer's answer wasn't understood. Update cagents there or on this phone.")
            when (message.string("type")) {
                "pair_waiting" -> if (!waiting) {
                    waiting = true
                    onWaiting()
                }
                "paired" -> return PairOutcome.Paired(message.string("host")?.takeIf { it.isNotBlank() })
                "pair_declined" -> return PairOutcome.Declined
                "pair_error" -> return PairOutcome.Failed(
                    message.string("message")?.takeIf { it.isNotBlank() } ?: "The computer couldn't pair this phone.",
                )
            }
        }
    }

    private fun JsonObject.string(name: String): String? =
        (this[name] as? JsonPrimitive)?.takeIf { it.isString }?.contentOrNull
}
