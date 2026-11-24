package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Activity
import dev.cagents.mobile.protocol.AgentOwner
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.Session

/** A session with the agents it started, which are shown beneath it rather than beside it. */
data class SessionEntry(val session: Session, val agents: List<Session> = emptyList())

sealed interface AttentionItem {
    val key: String

    data class FromNotice(val notice: Notice, val session: Session?) : AttentionItem {
        override val key get() = "notice/${notice.id}"
    }

    data class Waiting(val entry: SessionEntry) : AttentionItem {
        override val key get() = "waiting/${entry.session.id}"
    }
}

/**
 * The sessions list as someone away from their desk wants it: what needs
 * them first, then what is working, then the rest, most recent first.
 */
data class SessionOverview(
    val attention: List<AttentionItem>,
    val working: List<SessionEntry>,
    val recent: List<SessionEntry>,
) {
    val isEmpty: Boolean get() = attention.isEmpty() && working.isEmpty() && recent.isEmpty()

    companion object {
        fun of(sessions: List<Session>, notices: List<Notice>): SessionOverview {
            val lineages = sessions.mapTo(HashSet()) { it.lineage }
            fun ownerOf(session: Session): String? = session.agent
                ?.takeIf { it.owner == AgentOwner.Session }
                ?.ownerLineage
                ?.takeIf { it != session.lineage && it in lineages }

            val agentsByOwner = sessions.filter { ownerOf(it) != null }.groupBy { ownerOf(it)!! }
            val newestFirst = compareByDescending<Session> { it.activeAt ?: 0L }
            val entries = sessions
                .filter { ownerOf(it) == null }
                .map { SessionEntry(it, agentsByOwner[it.lineage].orEmpty().sortedWith(newestFirst)) }

            val bySession = sessions.associateBy { it.id }
            val noticed = notices.mapNotNullTo(HashSet()) { it.session }
            val waiting = sessions
                .filter { it.activity == Activity.Waiting && !it.exited && it.id !in noticed }
                .sortedWith(newestFirst)
                .map { session -> entries.firstOrNull { it.session.id == session.id } ?: SessionEntry(session) }

            val attention = notices.map { AttentionItem.FromNotice(it, it.session?.let(bySession::get)) } +
                waiting.map { AttentionItem.Waiting(it) }

            val listed = entries.filterNot { it.session.activity == Activity.Waiting && !it.session.exited }
            val (working, rest) = listed.partition {
                it.session.activity == Activity.Working && !it.session.exited
            }
            return SessionOverview(
                attention = attention,
                working = working.sortedWith(compareBy(newestFirst) { it.session }),
                recent = rest.sortedWith(
                    compareBy<SessionEntry> { it.session.exited }.thenBy(newestFirst) { it.session },
                ),
            )
        }
    }
}
