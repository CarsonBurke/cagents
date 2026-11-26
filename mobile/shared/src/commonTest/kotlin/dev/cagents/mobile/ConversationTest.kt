package dev.cagents.mobile

import dev.cagents.mobile.client.Conversation
import dev.cagents.mobile.protocol.ConversationUpdate
import dev.cagents.mobile.protocol.Item
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.Role
import dev.cagents.mobile.protocol.TextAppend
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertSame
import kotlin.test.assertTrue

class ConversationTest {
    private val s = "session"
    private fun text(id: String, text: String) = Item(id, Role.Assistant, ItemBody.Text(text))
    private val Conversation.texts get() = items.map { it.id to (it.body as ItemBody.Text).text }

    private fun loaded(vararg items: Item) =
        Conversation(s).apply(ConversationUpdate(s, reset = true, items = items.toList())).conversation

    @Test
    fun aReadCheckpointChangesOnlyWhenTheConversationResets() {
        val held = Conversation(s).apply(ConversationUpdate(s, reset = true, readCheckpoint = "first")).conversation
        val updated = held.apply(ConversationUpdate(s, items = listOf(text("a", "Working")))).conversation
        val reattached = updated.apply(ConversationUpdate(s, reset = true, readCheckpoint = "second")).conversation
        assertEquals("first", held.readCheckpoint)
        assertEquals(held.readCheckpoint, updated.readCheckpoint)
        assertEquals("second", reattached.readCheckpoint)
        assertEquals(null, reattached.apply(ConversationUpdate(s, reset = true)).conversation.readCheckpoint)
    }

    @Test
    fun appendsStreamIntoTheirItemAndLeaveTheRestAlone() {
        val held = loaded(text("a", "Earlier"), text("b", "On it"))
        val next = held.apply(ConversationUpdate(s, appends = listOf(TextAppend("b", 5, ", done.", at = 9)))).conversation
        assertEquals(listOf("a" to "Earlier", "b" to "On it, done."), next.texts)
        assertEquals(9L, next.items[1].at)
        assertSame(held.items[0], next.items[0], "an untouched item stays the same object, so it isn't drawn again")
    }

    @Test
    fun anItemWithoutAnOrderReplacesItsOwnOrGoesLast() {
        val held = loaded(text("a", "Earlier"), text("b", "On it"))
        val next = held.apply(
            ConversationUpdate(
                s,
                items = listOf(text("a", "Edited"), text("c", "New")),
                appends = listOf(TextAppend("c", 3, "er")),
            ),
        ).conversation
        assertEquals(listOf("a" to "Edited", "b" to "On it", "c" to "Newer"), next.texts)
    }

    @Test
    fun anOrderPlacesEveryItem() {
        val held = loaded(text("a", "A"), text("b", "B"))
        val next = held.apply(ConversationUpdate(s, order = listOf("b", "c", "a"), items = listOf(text("c", "C")))).conversation
        assertEquals(listOf("b", "c", "a"), next.items.map { it.id })
    }

    @Test
    fun anOrderNamingAnItemNeverSentIsOutOfStep() {
        val held = loaded(text("a", "A"), text("b", "B"))
        // "c" was in an update the app missed.
        val applied = held.apply(ConversationUpdate(s, order = listOf("a", "c", "b", "d"), items = listOf(text("d", "D"))))
        assertTrue(applied.desynced)
        assertSame(held, applied.conversation)
        // An order that drops an item removes it; that is no sign of a miss.
        val removed = held.apply(ConversationUpdate(s, order = listOf("b")))
        assertFalse(removed.desynced)
        assertEquals(listOf("b"), removed.conversation.items.map { it.id })
        // A reset holds only what it sends, so it has nothing to have missed.
        val reset = held.apply(ConversationUpdate(s, reset = true, order = listOf("x", "c"), items = listOf(text("c", "C"))))
        assertFalse(reset.desynced)
        assertEquals(listOf("c"), reset.conversation.items.map { it.id })
    }

    @Test
    fun anAppendThatDoesNotFollowOnIsOutOfStep() {
        val held = loaded(text("a", "Hello"))
        for (append in listOf(TextAppend("a", 3, "x"), TextAppend("missing", 0, "x"))) {
            val applied = held.apply(ConversationUpdate(s, appends = listOf(append)))
            assertTrue(applied.desynced)
            assertSame(held, applied.conversation)
        }
        assertFalse(held.apply(ConversationUpdate(s, appends = listOf(TextAppend("a", 5, "!")))).desynced)
    }
}
