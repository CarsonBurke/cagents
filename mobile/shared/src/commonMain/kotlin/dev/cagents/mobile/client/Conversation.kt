package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.ConversationState
import dev.cagents.mobile.protocol.ConversationUpdate
import dev.cagents.mobile.protocol.Item
import dev.cagents.mobile.protocol.ItemBody

/**
 * What the app holds of one watched conversation: its items in display order
 * and its latest state. Immutable; [apply] returns the next one.
 */
data class Conversation(
    val session: String,
    val state: ConversationState = ConversationState(),
    val items: List<Item> = emptyList(),
    /** False until the first reset arrives. */
    val loaded: Boolean = false,
    /** Why the conversation can no longer be followed, when it can't. */
    val ended: String? = null,
) {
    /** The result of applying an update: the next conversation, and whether it fell out of step. */
    data class Applied(val conversation: Conversation, val desynced: Boolean)

    /**
     * Applies [update] as docs/mobile-api.md#conversations describes. An
     * update before the first reset, or an append whose offset doesn't match
     * the held text, means something was missed: the conversation is returned
     * unchanged with `desynced` set, and the caller watches it again.
     */
    fun apply(update: ConversationUpdate): Applied {
        if (!update.reset && !loaded) return Applied(this, desynced = true)

        val byId: MutableMap<String, Item>
        val order: List<String>
        if (update.reset) {
            byId = LinkedHashMap<String, Item>(update.items.size * 2).also { map ->
                update.items.forEach { map[it.id] = it }
            }
            order = update.order ?: update.items.map { it.id }
        } else {
            byId = LinkedHashMap<String, Item>(items.size * 2).also { map -> items.forEach { map[it.id] = it } }
            update.items.forEach { byId[it.id] = it }
            // The server sends `order` whenever it changes; without one, an
            // item it hasn't placed goes last rather than being dropped.
            order = update.order ?: run {
                val known = items.mapTo(HashSet()) { it.id }
                items.map { it.id } + update.items.map { it.id }.filter { known.add(it) }
            }
        }

        for (append in update.appends) {
            val held = byId[append.id] ?: return Applied(this, desynced = true)
            val body = held.body as? ItemBody.Text ?: return Applied(this, desynced = true)
            if (body.text.length.toLong() != append.offset) return Applied(this, desynced = true)
            byId[append.id] = held.copy(body = body.copy(text = body.text + append.text), at = append.at ?: held.at)
        }

        return Applied(
            Conversation(
                session = session,
                state = update.state,
                items = order.mapNotNull { byId[it] },
                loaded = true,
                ended = null,
            ),
            desynced = false,
        )
    }
}
