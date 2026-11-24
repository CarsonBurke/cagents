package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Activity
import dev.cagents.mobile.protocol.AgentOwner
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.NoticeKind
import dev.cagents.mobile.protocol.Session

/** A session with the agents it started, which are shown beneath it rather than beside it. */
data class SessionEntry(val session: Session, val agents: List<Session> = emptyList())

sealed interface AttentionItem {
    val key: String

    /** [notice] is the newest for its session; [covers] also holds the IDs of older ones it stands for. */
    data class FromNotice(
        val notice: Notice,
        val session: Session?,
        val covers: List<String> = listOf(notice.id),
    ) : AttentionItem {
        override val key get() = "notice/${notice.id}"
    }

    data class Waiting(val entry: SessionEntry) : AttentionItem {
        override val key get() = "waiting/${entry.session.id}"
    }

    /**
     * Pull requests awaiting the person's review. The phone can't open them,
     * and a busy account has hundreds, so they are counted on one card
     * rather than listed ahead of the sessions.
     */
    data class Reviews(val notices: List<Notice>) : AttentionItem {
        override val key get() = "reviews"
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
    /** The newest unread outcome (finished, failed, stopped) of each listed session that isn't working again, by session ID. */
    val outcomes: Map<String, Notice> = emptyMap(),
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
            // Notices arrive newest first. Only those asking something of the
            // person are cards; an outcome is marked on its session's row, and
            // one notice per session stands for the older ones.
            val (asks, outcomeNotices) = notices.partition { it.asks }
            val (reviews, requests) = asks.partition { it.kind == NoticeKind.ReviewRequested }
            // A request whose session has moved on (answered, or closed)
            // no longer asks anything.
            val asking = requests
                .filter { notice ->
                    val session = notice.session ?: return@filter true
                    val current = bySession[session] ?: return@filter false
                    notice.kind != NoticeKind.Waiting || (current.activity == Activity.Waiting && !current.exited)
                }
                .groupBy { it.session ?: "notice/${it.id}" }
                .values
            // A session that is working again has moved past its last outcome.
            val outcomes = outcomeNotices
                .filter { notice -> bySession[notice.session]?.let { it.activity != Activity.Working || it.exited } == true }
                .distinctBy { it.session }
                .associateBy { it.session!! }

            val noticed = asking.mapNotNullTo(HashSet()) { it.first().session }
            val waiting = sessions
                .filter { it.activity == Activity.Waiting && !it.exited && it.id !in noticed }
                .sortedWith(newestFirst)
                .map { session -> entries.firstOrNull { it.session.id == session.id } ?: SessionEntry(session) }

            val attention = asking.map { group ->
                val newest = group.first()
                AttentionItem.FromNotice(newest, newest.session?.let(bySession::get), group.map { it.id })
            } +
                waiting.map { AttentionItem.Waiting(it) } +
                listOfNotNull(reviews.takeIf { it.isNotEmpty() }?.let { AttentionItem.Reviews(it) })

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
                outcomes = outcomes,
            )
        }
    }
}
