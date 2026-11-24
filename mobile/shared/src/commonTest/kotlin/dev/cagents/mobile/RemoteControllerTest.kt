package dev.cagents.mobile

import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.ConnectionLost
import dev.cagents.mobile.client.ConnectionState
import dev.cagents.mobile.client.ControllerConfig
import dev.cagents.mobile.client.RemoteController
import dev.cagents.mobile.client.RemoteState
import dev.cagents.mobile.client.RequestFailed
import dev.cagents.mobile.protocol.Activity
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.SendMode
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runTest
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlin.random.Random
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.hours
import kotlin.time.Duration.Companion.seconds

private const val S = "8d4f2a8e-3c55-4b8e-9d2a-1f0c6b7e5a10"

private fun sessionsEvent(activity: String) =
    """{"type":"event","event":"sessions","data":{"sessions":[{"id":"$S","lineage":"$S","surface":"chat","title":"Fix the build","activity":"$activity","exited":false,"watchable":true,"new_field":{"x":1}}]}}"""

private fun conversation(
    reset: Boolean,
    order: String?,
    items: String,
    appends: String = "[]",
    busy: Boolean = true,
    watchId: Long? = null,
) =
    """{"type":"event","event":"conversation","data":{"session":"$S","reset":$reset,""" +
        (watchId?.let { """"watch_id":$it,""" } ?: "") +
        """"state":{"activity":"working","busy":$busy,"ready":true,"exited":false,"has_earlier":false,"loading_earlier":false},""" +
        (order?.let { """"order":$it,""" } ?: "") +
        """"items":$items,"appends":$appends}}"""

private fun text(id: String, text: String, role: String = "assistant") =
    """{"id":"$id","role":"$role","kind":"text","text":"$text","thinking":false,"commentary":false}"""

class RemoteControllerTest {
    private val bridge = FakeBridge()

    private fun TestScope.controller(config: ControllerConfig = ControllerConfig(pingInterval = 10.hours)) =
        RemoteController(backgroundScope, bridge, config, Random(1), testScheduler.timeSource)

    private suspend fun RemoteController.await(predicate: (RemoteState) -> Boolean): RemoteState =
        state.first(predicate)

    @Test
    fun connectsAndFollowsSessionsAndNotifications() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.ok(connection.expect("sessions.watch"))
        connection.send(sessionsEvent("working"))
        connection.ok(connection.expect("notifications.watch"))
        connection.send("""{"type":"event","event":"notifications","data":{"notices":[{"id":"n1","kind":"waiting","title":"Agent needs your attention","session":"$S","request":"approval"}]}}""")
        connection.send("""{"type":"event","event":"from_the_future","data":{}}""")

        val state = controller.await { it.sessions != null && it.notices.isNotEmpty() }
        assertEquals(ConnectionState.Connected, state.connection)
        assertEquals("workstation", state.welcome?.host)
        assertEquals(Activity.Working, state.sessions!!.single().activity)
        assertEquals("n1", state.notices.single().id)

        connection.send(sessionsEvent("waiting"))
        controller.await { it.sessions?.single()?.activity == Activity.Waiting }
    }

    @Test
    fun followsAStreamingConversation() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        controller.open(S)
        val watch = connection.expect("conversation.watch")
        assertEquals(S, watch.params["session"]!!.jsonPrimitive.content)
        connection.ok(watch)
        connection.send(conversation(reset = true, order = """["u","a"]""", items = "[${text("u", "Fix it", "user")},${text("a", "On it")}]"))
        connection.send(conversation(reset = false, order = null, items = "[]", appends = """[{"id":"a","offset":5,"text":", done.","at":9}]"""))
        connection.send(conversation(reset = false, order = """["u","a","c"]""", items = """[{"id":"c","role":"assistant","kind":"command","label":"","command":"ls","action":"list","status":"running","output":"","clipped":false}]"""))

        val held = controller.await { (it.conversations[S]?.items?.size ?: 0) == 3 }.conversations.getValue(S)
        assertEquals(listOf("u", "a", "c"), held.items.map { it.id })
        assertEquals("On it, done.", (held.items[1].body as ItemBody.Text).text)
        assertEquals(9L, held.items[1].at)
        assertIs<ItemBody.Command>(held.items[2].body)
    }

    @Test
    fun acknowledgesOnlyAfterApplyingTheFirstAvailableReset() = runTest {
        val controller = controller()
        controller.open(S)
        controller.start()
        val first = bridge.acceptWelcomed()
        first.ok(first.expect("conversation.watch"))
        first.hangUp()

        controller.await { it.connection is ConnectionState.Retrying }
        assertEquals(false, controller.state.value.conversations[S]?.loaded)
        assertTrue(first.isClosedByClient(), "a watch reply alone must not acknowledge the conversation")

        val second = bridge.acceptWelcomed()
        second.ok(second.expect("conversation.watch"))
        second.send(conversation(reset = true, order = "[]", items = "[]", watchId = 11))
        val acknowledgement = second.nextRequest()
        assertEquals("conversation.acknowledge", acknowledgement.op)
        assertEquals(S, acknowledgement.params["session"]!!.jsonPrimitive.content)
        assertEquals("11", acknowledgement.params["watch_id"]!!.jsonPrimitive.content)
        assertEquals(true, controller.state.value.conversations[S]?.loaded)
        second.ok(acknowledgement)
        testScheduler.runCurrent()
        second.hangUp()

        // A reconnect refreshes the same open conversation without reading new notices.
        val third = bridge.acceptWelcomed()
        third.ok(third.expect("conversation.watch"))
        third.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Back")}]", watchId = 12))
        controller.await { it.conversations[S]?.items?.singleOrNull()?.id == "a" }
        testScheduler.runCurrent()
        controller.close(S)
        assertEquals("conversation.unwatch", third.nextRequest().op)
    }

    @Test
    fun aResetThatCannotBeAppliedDoesNotAcknowledgeTheConversation() = runTest {
        val controller = controller()
        controller.open(S)
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.ok(connection.expect("conversation.watch"))
        connection.send(
            conversation(
                reset = true,
                order = "[]",
                items = "[]",
                appends = """[{"id":"missing","offset":5,"text":"!"}]""",
                watchId = 10,
            ),
        )
        val recovery = connection.nextRequest()
        assertEquals("conversation.watch", recovery.op)
        connection.ok(recovery)
        connection.send(conversation(reset = true, order = "[]", items = "[]", watchId = 11))
        val acknowledgement = connection.nextRequest()
        assertEquals("conversation.acknowledge", acknowledgement.op)
        assertEquals("11", acknowledgement.params["watch_id"]!!.jsonPrimitive.content)
    }

    @Test
    fun reconnectDoesNotAcknowledgeAgainWhenTheReplyWasLost() = runTest {
        val controller = controller()
        controller.open(S)
        controller.start()
        val first = bridge.acceptWelcomed()
        first.ok(first.expect("conversation.watch"))
        first.send(conversation(reset = true, order = "[]", items = "[]", watchId = 11))
        assertEquals("conversation.acknowledge", first.nextRequest().op)
        // The bridge might have cleared the notices before the connection was lost.
        first.hangUp()

        val second = bridge.acceptWelcomed()
        second.ok(second.expect("conversation.watch"))
        second.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Back")}]", watchId = 12))
        controller.await { it.conversations[S]?.items?.singleOrNull()?.id == "a" }
        testScheduler.runCurrent()
        controller.close(S)
        assertEquals("conversation.unwatch", second.nextRequest().op)
    }

    @Test
    fun aStaleWatchRefusalLetsTheReplacementAcknowledgeTheOpen() = runTest {
        val controller = controller()
        controller.open(S)
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.ok(connection.expect("conversation.watch"))
        connection.send(conversation(reset = true, order = "[]", items = "[]", watchId = 11))
        val refused = connection.nextRequest()
        assertEquals("conversation.acknowledge", refused.op)
        connection.error(refused, "not_watching", "That watch ended.")
        controller.rewatch(S)
        val newWatch = connection.nextRequest()
        assertEquals("conversation.watch", newWatch.op)
        connection.ok(newWatch)
        connection.send(conversation(reset = true, order = "[]", items = "[]", watchId = 12))
        val acknowledgement = connection.nextRequest()
        assertEquals("conversation.acknowledge", acknowledgement.op)
        assertEquals("12", acknowledgement.params["watch_id"]!!.jsonPrimitive.content)
    }

    @Test
    fun watchesAgainWhenAnAppendDoesNotLineUp() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        controller.open(S)
        connection.ok(connection.expect("conversation.watch"))
        connection.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        controller.await { it.conversations[S]?.loaded == true }

        // The held text is 5 code units long, not 7: an update was missed.
        connection.send(conversation(reset = false, order = null, items = "[]", appends = """[{"id":"a","offset":7,"text":"!"}]"""))
        val again = connection.expect("conversation.watch")
        assertEquals(S, again.params["session"]!!.jsonPrimitive.content)
        assertEquals("Hello", (controller.state.value.conversations.getValue(S).items.single().body as ItemBody.Text).text)
    }

    @Test
    fun sendsSteersAnswersAndReportsErrors() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.ok(connection.expect("sessions.watch"))
        connection.ok(connection.expect("notifications.watch"))

        val send = async { controller.send(S, "Run clippy too", SendMode.Steer) }
        val request = connection.expect("conversation.send")
        assertEquals("steer", request.params["mode"]!!.jsonPrimitive.content)
        assertEquals("Run clippy too", request.params["text"]!!.jsonPrimitive.content)
        connection.ok(request)
        send.await()

        val answer = async { runCatching { controller.answer(S, "r1", "deny", listOf(FieldAnswer("reason", listOf("later")))) } }
        val answerRequest = connection.expect("conversation.answer")
        assertEquals("deny", answerRequest.params["action"]!!.jsonPrimitive.content)
        val answers = answerRequest.params["answers"]!!.jsonArray
        assertEquals("later", answers.single().jsonObject["values"]!!.jsonArray.single().jsonPrimitive.content)
        connection.error(answerRequest, "not_watching", "Open the conversation before acting on it.")
        val failure = assertIs<RequestFailed>(answer.await().exceptionOrNull())
        assertEquals(ErrorCode.NotWatching, failure.code)
        assertEquals("Open the conversation before acting on it.", failure.message)

        val interrupt = async { runCatching { controller.interrupt(S) } }
        connection.error(connection.expect("conversation.interrupt"), "something_new", "Not now.")
        assertEquals(ErrorCode.Unknown, assertIs<RequestFailed>(interrupt.await().exceptionOrNull()).code)
    }

    @Test
    fun reconnectsWithBackoffAndWatchesAgain() = runTest {
        val controller = controller()
        controller.open(S)
        controller.start()
        val first = bridge.acceptWelcomed()
        first.ok(first.expect("conversation.watch"))
        first.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        controller.await { it.conversations[S]?.loaded == true }

        first.send("""{"type":"event","event":"closing","data":{"message":"The computer's cagents daemon is stopping."}}""")
        first.hangUp()
        val retrying = controller.await { it.connection is ConnectionState.Retrying }.connection as ConnectionState.Retrying
        assertEquals("The computer's cagents daemon is stopping.", retrying.message)
        assertEquals(1, retrying.attempt)
        assertTrue(retrying.delay in 0.8.seconds..1.2.seconds)

        bridge.refusals += ConnectFailure.Network("No route to host")
        val second = controller.await { (it.connection as? ConnectionState.Retrying)?.attempt == 2 }.connection
        assertTrue((second as ConnectionState.Retrying).delay in 1.6.seconds..2.4.seconds)

        val reconnected = bridge.acceptWelcomed()
        reconnected.ok(reconnected.expect("conversation.watch"))
        reconnected.send(conversation(reset = true, order = """["a","b"]""", items = "[${text("a", "Hello")},${text("b", "Back")}]"))
        val state = controller.await { it.conversations[S]?.items?.size == 2 }
        assertEquals(ConnectionState.Connected, state.connection)
        assertEquals(3, bridge.opens)
    }

    @Test
    fun waitsForThePersonWhenTheHostKeyIsUnknown() = runTest {
        val controller = controller()
        bridge.refusals += ConnectFailure.UnknownHostKey("SHA256:abc", "ssh-ed25519")
        controller.start()
        val state = controller.await { it.connection is ConnectionState.NeedsAction }
        assertIs<ConnectFailure.UnknownHostKey>((state.connection as ConnectionState.NeedsAction).failure)
        assertEquals(1, bridge.opens)

        controller.retryNow()
        bridge.acceptWelcomed()
        controller.await { it.connection == ConnectionState.Connected }
    }

    @Test
    fun refusesAnUnsupportedApiWithoutRetrying() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.accept()
        connection.next()
        connection.send("""{"type":"error","error":{"code":"unsupported_api","message":"Update the app."}}""")
        val state = controller.await { it.connection is ConnectionState.NeedsAction }
        val failure = (state.connection as ConnectionState.NeedsAction).failure
        assertIs<ConnectFailure.Refused>(failure)
        assertEquals("Update the app.", failure.message)
    }

    @Test
    fun aMissedPingDropsTheConnection() = runTest {
        val controller = controller(ControllerConfig(pingInterval = 25.seconds, pingTimeout = 5.seconds))
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.ok(connection.expect("sessions.watch"))
        connection.ok(connection.expect("notifications.watch"))
        // Never answer the ping.
        assertEquals("ping", connection.next()["op"]!!.jsonPrimitive.content)
        controller.await { it.connection is ConnectionState.Retrying }
        assertTrue(connection.isClosedByClient())
    }

    @Test
    fun anEndedConversationSaysWhy() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        controller.open(S)
        connection.error(connection.expect("conversation.watch"), "not_watchable", "This session runs in a terminal.")
        val ended = controller.await { it.conversations[S]?.ended != null }
        assertEquals("This session runs in a terminal.", ended.conversations.getValue(S).ended)

        // Nothing to unwatch: opening it again goes straight to a new watch.
        controller.close(S)
        assertNull(controller.state.value.conversations[S])
        controller.open(S)
        assertEquals("conversation.watch", connection.nextRequest().op)
    }

    @Test
    fun aWatchAnsweredUnavailableEndsTheConversation() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        controller.open(S)
        connection.error(connection.expect("conversation.watch"), "unavailable", "The conversation couldn't be loaded.")
        val ended = controller.await { it.conversations[S]?.ended != null }
        assertEquals("The conversation couldn't be loaded.", ended.conversations.getValue(S).ended)

        controller.rewatch(S)
        connection.ok(connection.expect("conversation.watch"))
        connection.send(conversation(reset = true, order = "[]", items = "[]"))
        controller.await { it.conversations[S]?.let { c -> c.loaded && c.ended == null } == true }
    }

    @Test
    fun closingAndReopeningQuicklyWatchesAfresh() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        controller.open(S)
        connection.ok(connection.expect("conversation.watch"))
        connection.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        controller.await { it.conversations[S]?.loaded == true }

        controller.close(S)
        controller.open(S)
        // The reopened conversation needs a reset, and no unwatch may land after its watch.
        val again = connection.nextRequest()
        assertEquals("conversation.watch", again.op)
        connection.ok(again)
        connection.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        controller.await { it.conversations[S]?.loaded == true }

        controller.close(S)
        assertEquals("conversation.unwatch", connection.nextRequest().op)
    }

    private fun unreadable(reset: Boolean) =
        """{"type":"event","event":"conversation","data":{"session":"$S","reset":$reset,"state":"from a newer cagents","items":[],"appends":[]}}"""

    /** Opens the conversation and loads it with one text item, "Hello". */
    private suspend fun FakeConnection.loadHello(controller: RemoteController) {
        controller.open(S)
        ok(expect("conversation.watch"))
        send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        controller.await { it.conversations[S]?.loaded == true }
    }

    @Test
    fun anUnreadableUpdateWatchesAgainAndSetsAsideWhatWasInFlight() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.loadHello(controller)

        connection.send(unreadable(reset = false))
        val again = connection.expect("conversation.watch")
        // Sent before the reset: neither applied nor a reason to give up.
        connection.send(unreadable(reset = false))
        connection.send(conversation(reset = false, order = null, items = "[]", appends = """[{"id":"a","offset":5,"text":" there"}]"""))
        connection.ok(again)
        connection.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello there")}]"))
        connection.send(conversation(reset = false, order = null, items = "[]", appends = """[{"id":"a","offset":11,"text":"!"}]"""))

        val held = controller.await {
            (it.conversations[S]?.items?.singleOrNull()?.body as? ItemBody.Text)?.text == "Hello there!"
        }.conversations.getValue(S)
        assertNull(held.ended)
        assertEquals(ConnectionState.Connected, controller.state.value.connection)
    }

    @Test
    fun anUnreadableResetGivesUp() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.loadHello(controller)

        connection.send(unreadable(reset = false))
        connection.ok(connection.expect("conversation.watch"))
        connection.send(unreadable(reset = true))
        val ended = controller.await { it.conversations[S]?.ended != null }.conversations.getValue(S)
        assertEquals("Hello", (ended.items.single().body as ItemBody.Text).text)
        assertTrue(ended.ended!!.contains("Update the app"))
        assertEquals("conversation.unwatch", connection.nextRequest().op)
        assertEquals(ConnectionState.Connected, controller.state.value.connection)
    }

    @Test
    fun aConversationThatKeepsFallingOutOfStepEnds() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.loadHello(controller)

        val misaligned = conversation(reset = false, order = null, items = "[]", appends = """[{"id":"a","offset":7,"text":"!"}]""")
        repeat(3) {
            connection.send(misaligned)
            connection.ok(connection.expect("conversation.watch"))
            connection.send(conversation(reset = true, order = """["a"]""", items = "[${text("a", "Hello")}]"))
        }
        connection.send(misaligned)
        assertEquals("conversation.unwatch", connection.nextRequest().op)
        assertTrue(controller.state.value.conversations.getValue(S).ended!!.contains("out of step"))
    }

    @Test
    fun aFailedWatchAgainStillUnwatchesOnClose() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.loadHello(controller)

        controller.rewatch(S)
        connection.error(connection.expect("conversation.watch"), "unavailable", "Busy.")
        controller.await { it.conversations[S]?.ended != null }
        // The first watch may still stand on the server.
        controller.close(S)
        assertEquals("conversation.unwatch", connection.nextRequest().op)
    }

    @Test
    fun anUnreadableSessionListAsksForAnUpdate() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        connection.send("""{"type":"event","event":"sessions","data":{"sessions":"from a newer cagents"}}""")
        val state = controller.await { it.connection is ConnectionState.NeedsAction }
        assertIs<ConnectFailure.Refused>((state.connection as ConnectionState.NeedsAction).failure)
        // Past the requests already sent, the app has closed the connection.
        while (!connection.isClosedByClient()) continue
    }

    @Test
    fun aConnectionThatDropsAtOnceKeepsBackingOff() = runTest {
        val controller = controller()
        controller.start()
        bridge.acceptWelcomed().hangUp()
        controller.await { (it.connection as? ConnectionState.Retrying)?.attempt == 1 }
        bridge.acceptWelcomed().hangUp()
        controller.await { (it.connection as? ConnectionState.Retrying)?.attempt == 2 }

        // One that held for a while starts over.
        val held = bridge.acceptWelcomed()
        controller.await { it.connection == ConnectionState.Connected }
        delay(11.seconds)
        held.hangUp()
        controller.await { (it.connection as? ConnectionState.Retrying)?.attempt == 1 }
    }

    @Test
    fun aRequestInFlightFailsWhenTheConnectionDrops() = runTest {
        val controller = controller()
        controller.start()
        val connection = bridge.acceptWelcomed()
        val send = async { runCatching { controller.send(S, "hello", SendMode.Queue) } }
        connection.expect("conversation.send")
        connection.hangUp()
        assertIs<ConnectionLost>(send.await().exceptionOrNull())
    }

    @Test
    fun goingToTheBackgroundDisconnectsAfterTheGrace() = runTest {
        val controller = controller(ControllerConfig(pingInterval = 10.hours, backgroundGrace = 30.seconds))
        controller.start()
        bridge.acceptWelcomed()
        controller.await { it.connection == ConnectionState.Connected }
        controller.setForeground(false)
        controller.await { it.connection == ConnectionState.Offline }

        controller.setForeground(true)
        bridge.acceptWelcomed()
        controller.await { it.connection == ConnectionState.Connected }
    }
}
