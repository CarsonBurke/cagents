package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.ConversationState
import dev.cagents.mobile.protocol.ConversationUpdate
import dev.cagents.mobile.protocol.Item
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.TextAppend

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
    /** The presentation this watch permits a visible reader to acknowledge. */
    val readCheckpoint: String? = null,
) {
    /** The result of applying an update: the next conversation, and whether it fell out of step. */
    data class Applied(val conversation: Conversation, val desynced: Boolean)

    /**
     * Applies [update] as docs/mobile-api.md#conversations describes. An
     * update before the first reset, an append whose offset doesn't match the
     * held text, or an `order` naming an item neither held nor sent means
     * something was missed: the conversation is returned unchanged with
     * `desynced` set, and the caller watches it again.
     */
    fun apply(update: ConversationUpdate): Applied {
        if (!update.reset && !loaded) return Applied(this, desynced = true)
        if (!update.reset && update.order == null) return applyInPlace(update)

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
            order = checkNotNull(update.order) { "applied in place without one" }
        }

        for (append in update.appends) {
            byId[append.id] = appended(byId[append.id] ?: return Applied(this, desynced = true), append)
                ?: return Applied(this, desynced = true)
        }

        val placed = ArrayList<Item>(order.size)
        for (id in order) {
            // A reset holds only what it sends, so there is nothing to have missed.
            placed += byId[id] ?: if (update.reset) continue else return Applied(this, desynced = true)
        }
        return Applied(
            copy(
                state = update.state,
                items = placed,
                loaded = true,
                ended = null,
                readCheckpoint = if (update.reset) update.readCheckpoint else readCheckpoint,
            ),
            desynced = false,
        )
    }

    /**
     * An update that keeps the order, as nearly every one while an answer
     * streams does: its items replace the held ones in place, and one the
     * server hasn't placed goes last rather than being dropped. Searched for
     * from the end, where streaming happens, so an append doesn't cost a pass
     * over the whole conversation.
     */
    private fun applyInPlace(update: ConversationUpdate): Applied {
        val next = ArrayList<Item>(items.size + update.items.size)
        next.addAll(items)
        for (item in update.items) {
            val at = next.indexOfLast { it.id == item.id }
            if (at >= 0) next[at] = item else next += item
        }
        for (append in update.appends) {
            val at = next.indexOfLast { it.id == append.id }
            if (at < 0) return Applied(this, desynced = true)
            next[at] = appended(next[at], append) ?: return Applied(this, desynced = true)
        }
        return Applied(
            copy(state = update.state, items = next, loaded = true, ended = null),
            desynced = false,
        )
    }

    /** [held] with [append]'s text added, or null when it doesn't follow on from the held text. */
    private fun appended(held: Item, append: TextAppend): Item? {
        val body = held.body as? ItemBody.Text ?: return null
        if (body.text.length.toLong() != append.offset) return null
        return held.copy(body = body.copy(text = body.text + append.text), at = append.at ?: held.at)
    }
}

/**
 * Whether [this] has nothing for a person to read: reasoning whose text the
 * provider withheld or hasn't streamed yet. It is shown once text arrives.
 */
val Item.isBlankThinking: Boolean
    get() = (body as? ItemBody.Text)?.let { it.thinking && it.text.isBlank() } == true
