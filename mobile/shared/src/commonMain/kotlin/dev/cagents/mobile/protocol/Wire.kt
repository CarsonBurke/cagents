package dev.cagents.mobile.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.SerializationException
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildClassSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonObject

/** The JSON settings docs/mobile-api.md#compatibility asks a client to use. */
val WireJson = Json {
    ignoreUnknownKeys = true
    explicitNulls = false
    encodeDefaults = true
}

// ---------------------------------------------------------------------------
// Server to client
// ---------------------------------------------------------------------------

/** One line from the bridge. */
sealed interface ServerLine {
    data class WelcomeLine(val welcome: Welcome) : ServerLine
    data class Ok(val id: Long) : ServerLine

    /** [id] is null for an error that answers no request. */
    data class Error(val id: Long?, val error: RemoteError) : ServerLine
    data class EventLine(val event: Event) : ServerLine

    /** A message type this app doesn't know; ignored. */
    data class Unknown(val type: String?) : ServerLine
}

sealed interface Event {
    data class Sessions(val sessions: List<Session>) : Event
    data class Notifications(val notices: List<Notice>) : Event
    data class Conversation(val update: ConversationUpdate) : Event
    data class ConversationEnded(val session: String, val message: String) : Event
    data class Closing(val message: String) : Event

    /** An event this app doesn't know; ignored. */
    data class Unknown(val name: String?) : Event
}

/** Reads one line from the bridge. Throws [SerializationException] for malformed JSON. */
fun decodeServerLine(line: String): ServerLine {
    val obj = WireJson.parseToJsonElement(line).jsonObject
    return when (val type = obj.string("type")) {
        "welcome" -> ServerLine.WelcomeLine(WireJson.decodeFromJsonElement(obj))
        "ok" -> ServerLine.Ok(obj.requireLong("id"))
        "error" -> ServerLine.Error(
            id = obj.long("id"),
            error = obj["error"]?.let { WireJson.decodeFromJsonElement<RemoteError>(it) } ?: RemoteError(),
        )
        "event" -> ServerLine.EventLine(decodeEvent(obj.string("event"), obj["data"] ?: JsonObject(emptyMap())))
        else -> ServerLine.Unknown(type)
    }
}

private fun decodeEvent(name: String?, data: JsonElement): Event = when (name) {
    "sessions" -> Event.Sessions(WireJson.decodeFromJsonElement<SessionList>(data).sessions)
    "notifications" -> Event.Notifications(WireJson.decodeFromJsonElement<NoticeList>(data).notices)
    "conversation" -> Event.Conversation(WireJson.decodeFromJsonElement(data))
    "conversation_ended" -> data.jsonObject.let {
        Event.ConversationEnded(it.string("session").orEmpty(), it.string("message").orEmpty())
    }
    "closing" -> Event.Closing(data.jsonObject.string("message").orEmpty())
    else -> Event.Unknown(name)
}

private fun JsonObject.string(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull
private fun JsonObject.long(key: String): Long? = (this[key] as? JsonPrimitive)?.longOrNull
private fun JsonObject.requireLong(key: String): Long =
    long(key) ?: throw SerializationException("missing numeric '$key'")

/**
 * An item's `kind` and its fields sit beside `id`, `role` and the times in one
 * object. A kind this app doesn't know becomes [ItemBody.Unknown] rather than
 * failing the whole update.
 */
object ItemSerializer : KSerializer<Item> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("dev.cagents.mobile.Item")

    override fun deserialize(decoder: Decoder): Item {
        val json = (decoder as? JsonDecoder)?.json
            ?: throw SerializationException("Items decode only from JSON")
        val obj = decoder.decodeJsonElement().jsonObject
        val kind = obj.string("kind").orEmpty()
        val body: ItemBody = when (kind) {
            "text" -> json.decodeFromJsonElement<ItemBody.Text>(obj)
            "command" -> json.decodeFromJsonElement<ItemBody.Command>(obj)
            "changes" -> json.decodeFromJsonElement<ItemBody.Changes>(obj)
            "web_search" -> json.decodeFromJsonElement<ItemBody.WebSearch>(obj)
            "tool" -> json.decodeFromJsonElement<ItemBody.Tool>(obj)
            "image" -> json.decodeFromJsonElement<ItemBody.Image>(obj)
            "delivery" -> json.decodeFromJsonElement<ItemBody.Delivery>(obj)
            else -> ItemBody.Unknown(kind)
        }
        return Item(
            id = obj.string("id") ?: throw SerializationException("item without an id"),
            role = obj["role"]?.let { json.decodeFromJsonElement<Role>(it) } ?: Role.Unknown,
            body = body,
            startedAt = obj.long("started_at"),
            at = obj.long("at"),
        )
    }

    override fun serialize(encoder: Encoder, value: Item) =
        throw SerializationException("The app never sends transcript items")
}

// ---------------------------------------------------------------------------
// Client to server
// ---------------------------------------------------------------------------

enum class SendMode(val wire: String) {
    /** Start a turn, or follow the provider's own default while one runs. */
    Send("send"),

    /** Steer the running turn at its next step. */
    Steer("steer"),

    /** Hold the message until the running turn ends. */
    Queue("queue"),
}

data class FieldAnswer(val field: String, val values: List<String>)

/** What the app asks of the bridge: an `op` and its parameters. */
sealed class Call(val op: String) {
    open fun params(): JsonObject? = null

    data object Ping : Call("ping")
    data object WatchSessions : Call("sessions.watch")
    data object UnwatchSessions : Call("sessions.unwatch")
    data object WatchNotifications : Call("notifications.watch")
    data object UnwatchNotifications : Call("notifications.unwatch")

    data class AcknowledgeNotification(val id: String) : Call("notification.acknowledge") {
        override fun params() = buildJsonObject { put("id", id) }
    }

    sealed class OnSession(op: String) : Call(op) {
        abstract val session: String
        override fun params(): JsonObject = buildJsonObject { put("session", session) }
    }

    data class WatchConversation(override val session: String) : OnSession("conversation.watch")
    data class UnwatchConversation(override val session: String) : OnSession("conversation.unwatch")
    data class LoadEarlier(override val session: String) : OnSession("conversation.load_earlier")
    data class Interrupt(override val session: String) : OnSession("conversation.interrupt")

    data class Send(override val session: String, val text: String, val mode: SendMode) :
        OnSession("conversation.send") {
        override fun params() = buildJsonObject {
            put("session", session)
            put("text", text)
            put("mode", mode.wire)
        }
    }

    data class Answer(
        override val session: String,
        val request: String,
        val action: String,
        val answers: List<FieldAnswer>,
    ) : OnSession("conversation.answer") {
        override fun params() = buildJsonObject {
            put("session", session)
            put("request", request)
            put("action", action)
            if (answers.isNotEmpty()) {
                put("answers", buildJsonArray {
                    answers.forEach { answer ->
                        add(buildJsonObject {
                            put("field", answer.field)
                            put("values", buildJsonArray { answer.values.forEach { add(JsonPrimitive(it)) } })
                        })
                    }
                })
            }
        }
    }
}

fun encodeRequest(id: Long, call: Call): String = buildJsonObject {
    put("type", "request")
    put("id", id)
    put("op", call.op)
    call.params()?.let { put("params", it) }
}.toString()

fun encodeHello(clientName: String, clientVersion: String): String = buildJsonObject {
    put("type", "hello")
    put("api_min", MIN_API_VERSION)
    put("api_max", API_VERSION)
    putJsonObject("client") {
        put("name", clientName)
        put("version", clientVersion)
    }
}.toString()

