package dev.cagents.mobile

import dev.cagents.mobile.protocol.Activity
import dev.cagents.mobile.protocol.AgentOwner
import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.ChangeKind
import dev.cagents.mobile.protocol.CommandAction
import dev.cagents.mobile.protocol.DeliveryKind
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.Event
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.NoticeKind
import dev.cagents.mobile.protocol.PlanStatus
import dev.cagents.mobile.protocol.RequestKind
import dev.cagents.mobile.protocol.Role
import dev.cagents.mobile.protocol.RunState
import dev.cagents.mobile.protocol.SendMode
import dev.cagents.mobile.protocol.ServerLine
import dev.cagents.mobile.protocol.Surface
import dev.cagents.mobile.protocol.ToolStatus
import dev.cagents.mobile.protocol.decodeServerLine
import dev.cagents.mobile.protocol.encodeHello
import dev.cagents.mobile.protocol.encodeRequest
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long
import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlin.test.fail

/**
 * Holds the app to crates/mobile-api/fixtures/v1.jsonl, the bridge's
 * recording of every message API version 1 has had: each server message must
 * decode to what it says, and each client message the app sends must encode
 * exactly as recorded.
 */
class RecordedMessagesTest {
    private data class Recording(val name: String, val direction: String, val message: JsonObject)

    private val recordings: List<Recording> by lazy {
        val path = System.getProperty("cagents.mobileApiFixture")
            ?: fail("Run through Gradle, which points cagents.mobileApiFixture at the recording")
        File(path).readLines().filter { it.isNotBlank() }.map { line ->
            val obj = Json.parseToJsonElement(line).jsonObject
            Recording(
                name = obj["name"]!!.jsonPrimitive.content,
                direction = obj["direction"]!!.jsonPrimitive.content,
                message = obj["message"]!!.jsonObject,
            )
        }
    }

    private fun server(name: String): ServerLine {
        val recording = recordings.singleOrNull { it.name == name && it.direction == "server" }
            ?: fail("no server recording named '$name'")
        return decodeServerLine(recording.message.toString())
    }

    private inline fun <reified T : Event> event(name: String): T =
        assertIs<T>(assertIs<ServerLine.EventLine>(server(name)).event)

    @Test
    fun everyServerMessageDecodesToAKnownShape() {
        val servers = recordings.filter { it.direction == "server" }
        assertTrue(servers.isNotEmpty())
        for (recording in servers) {
            val decoded = decodeServerLine(recording.message.toString())
            assertTrue(decoded !is ServerLine.Unknown, "${recording.name} decoded as an unknown message")
            if (decoded is ServerLine.EventLine) {
                assertTrue(decoded.event !is Event.Unknown, "${recording.name} decoded as an unknown event")
            }
        }
    }

    @Test
    fun handshakeAndAnswers() {
        val welcome = assertIs<ServerLine.WelcomeLine>(server("welcome")).welcome
        assertEquals(1, welcome.api)
        assertEquals("0.1.3", welcome.serverVersion)
        assertEquals("desktop", welcome.host)

        assertEquals(ServerLine.Ok(7), server("ok"))

        val error = assertIs<ServerLine.Error>(server("error"))
        assertEquals(8, error.id)
        assertEquals(ErrorCode.NotWatching, error.error.code)
        assertEquals("Open the conversation before acting on it.", error.error.message)

        val refusal = assertIs<ServerLine.Error>(server("error without id"))
        assertNull(refusal.id)
        assertEquals(ErrorCode.UnsupportedApi, refusal.error.code)
    }

    @Test
    fun sessions() {
        val session = event<Event.Sessions>("event sessions").sessions.single()
        assertEquals("8d4f2a8e-3c55-4b8e-9d2a-1f0c6b7e5a10", session.id)
        assertEquals(Surface.Chat, session.surface)
        assertEquals(Activity.Working, session.activity)
        assertEquals("Codex", session.providerLabel)
        assertEquals("Fix the build", session.title)
        assertEquals("cagents", session.project)
        assertEquals("Usage limit reached", session.failure)
        assertEquals(1700000000000, session.activeAt)
        assertTrue(session.watchable)
        val agent = assertNotNull(session.agent)
        assertEquals(AgentOwner.Session, agent.owner)
        assertEquals(RunState.Running, agent.run)
        assertEquals(session.lineage, agent.ownerLineage)
    }

    @Test
    fun notifications() {
        val notice = event<Event.Notifications>("event notifications").notices.single()
        assertEquals(NoticeKind.Waiting, notice.kind)
        assertEquals(RequestKind.Approval, notice.request)
        assertEquals("Agent needs your attention", notice.title)
        assertEquals("Run cargo test?", notice.preview)
        assertEquals("Fix the build", notice.conversation)
    }

    @Test
    fun conversationResetWithStateAndAppends() {
        val update = event<Event.Conversation>("event conversation").update
        assertTrue(update.reset)
        assertEquals(listOf("i1", "i2"), update.order)

        val text = update.items[0]
        assertEquals(Role.User, text.role)
        assertEquals(ItemBody.Text("Fix the build", thinking = false, commentary = false), text.body)
        assertEquals(1L, text.startedAt)
        assertEquals(2L, text.at)

        val command = assertIs<ItemBody.Command>(update.items[1].body)
        assertEquals(CommandAction.Run, command.action)
        assertEquals(ToolStatus.Failed, command.status)
        assertEquals(101, command.exitCode)
        assertEquals(5000L, command.durationMs)
        assertEquals("the tests", command.label)
        assertTrue(command.clipped)

        val append = update.appends.single()
        assertEquals("i3", append.id)
        assertEquals(12L, append.offset)
        assertEquals(" more", append.text)
        assertEquals(3L, append.at)

        val state = update.state
        assertEquals(Activity.Waiting, state.activity)
        assertTrue(state.busy && state.ready && state.hasEarlier)
        assertEquals("Usage limit reached", state.error)
        assertEquals("Saved history", state.readOnly)
        assertEquals("opus", state.model)
        assertEquals(PlanStatus.Active, state.plan.single().status)
        assertEquals("Also run clippy", state.queued.single().text)
        assertEquals(ToolStatus.Running, state.subagents.single().status)

        val request = state.requests.single()
        assertEquals(RequestKind.Approval, request.kind)
        assertEquals("Run cargo test?", request.title)
        assertEquals(listOf("allow", "deny"), request.actions.map { it.id })
        val field = request.fields.single()
        assertEquals("reason", field.id)
        assertEquals(listOf("ci"), field.choices.map { it.id })
    }

    @Test
    fun everyItemKind() {
        val bodies = event<Event.Conversation>("item changes").update.items.associate { it.id to it.body }

        val changes = assertIs<ItemBody.Changes>(bodies["c"])
        val file = changes.files.single()
        assertEquals(ChangeKind.Renamed, file.change)
        assertEquals("src/old.rs", file.from)
        assertEquals(3L, file.addedLines)
        assertEquals(1L, file.removedLines)

        val search = assertIs<ItemBody.WebSearch>(bodies["w"])
        assertEquals("serde flatten", search.query)
        assertEquals("https://serde.rs", search.results.single().url)

        val tool = assertIs<ItemBody.Tool>(bodies["t"])
        assertEquals("mcp__search", tool.name)
        assertEquals(ToolStatus.Waiting, tool.status)

        assertEquals(ItemBody.Image("A screenshot"), bodies["m"])

        val delivery = assertIs<ItemBody.Delivery>(bodies["d"])
        assertEquals(DeliveryKind.Report, delivery.delivery)
        assertEquals("Explore", delivery.from)
        assertEquals("Found it", delivery.body)
    }

    @Test
    fun endings() {
        val ended = event<Event.ConversationEnded>("event conversation_ended")
        assertEquals("The conversation closed.", ended.message)
        assertEquals("The computer's cagents daemon is stopping.", event<Event.Closing>("event closing").message)
    }

    @Test
    fun clientMessagesEncodeAsRecorded() {
        val session = "8d4f2a8e-3c55-4b8e-9d2a-1f0c6b7e5a10"
        val calls: Map<String, Call> = mapOf(
            "ping" to Call.Ping,
            "sessions.watch" to Call.WatchSessions,
            "sessions.unwatch" to Call.UnwatchSessions,
            "notifications.watch" to Call.WatchNotifications,
            "notifications.unwatch" to Call.UnwatchNotifications,
            "notification.acknowledge" to Call.AcknowledgeNotification("0b0e7a52-6f6b-4f7c-8a35-5c9f3f0e2d11"),
            "conversation.watch" to Call.WatchConversation(session),
            "conversation.acknowledge" to Call.AcknowledgeConversation(session, 11),
            "conversation.unwatch" to Call.UnwatchConversation(session),
            "conversation.load_earlier" to Call.LoadEarlier(session),
            "conversation.send" to Call.Send(session, "Run the tests", SendMode.Steer),
            "conversation.interrupt" to Call.Interrupt(session),
            "conversation.answer" to Call.Answer(session, "r1", "allow", listOf(FieldAnswer("reason", listOf("ci")))),
        )
        val clients = recordings.filter { it.direction == "client" }
        val encoded = mutableSetOf<String>()
        for (recording in clients) {
            val expected = recording.message
            val actual = when (recording.name) {
                "hello" -> encodeHello("cagents-android", "1.0.0")
                else -> {
                    val call = calls[recording.name] ?: continue
                    encodeRequest(expected["id"]!!.jsonPrimitive.long, call)
                }
            }
            assertEquals(expected, Json.parseToJsonElement(actual), recording.name)
            encoded += recording.name
        }
        assertEquals(calls.keys + "hello", encoded, "every op the app sends has a recording")
    }
}
