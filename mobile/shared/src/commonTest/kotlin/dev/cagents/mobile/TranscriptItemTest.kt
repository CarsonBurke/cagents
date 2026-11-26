package dev.cagents.mobile

import dev.cagents.mobile.client.isBlankThinking
import dev.cagents.mobile.protocol.Item
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.Role
import kotlin.test.Test
import kotlin.test.assertEquals

class TranscriptItemTest {
    @Test
    fun hidesOnlyReasoningWithNothingToRead() {
        fun text(text: String, thinking: Boolean) = Item(id = text, role = Role.Assistant, body = ItemBody.Text(text, thinking = thinking))
        assertEquals(
            listOf(true, true, false, false),
            listOf(text("", true), text(" \n", true), text("Weighing it", true), text("", false)).map { it.isBlankThinking },
        )
    }
}
