package dev.cagents.mobile

import dev.cagents.mobile.client.AttentionItem
import dev.cagents.mobile.client.SessionOverview
import dev.cagents.mobile.protocol.Activity
import dev.cagents.mobile.protocol.Agent
import dev.cagents.mobile.protocol.AgentOwner
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.NoticeKind
import dev.cagents.mobile.protocol.Session
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs

class SessionOverviewTest {
    private fun session(
        id: String,
        activity: Activity,
        at: Long,
        exited: Boolean = false,
        owner: String? = null,
    ) = Session(
        id = id,
        lineage = "L$id",
        activity = activity,
        activeAt = at,
        exited = exited,
        agent = owner?.let { Agent(id = "agent-$id", owner = AgentOwner.Session, ownerLineage = "L$it") },
    )

    @Test
    fun ordersByWhatNeedsThePersonFirst() {
        val sessions = listOf(
            session("idle-old", Activity.Idle, 1),
            session("idle-new", Activity.Idle, 5),
            session("exited", Activity.Unknown, 9, exited = true),
            session("work", Activity.Working, 3),
            session("wait", Activity.Waiting, 2),
            session("noticed", Activity.Waiting, 4),
        )
        val notice = Notice(id = "n", kind = NoticeKind.Waiting, asks = true, session = "noticed")
        val overview = SessionOverview.of(sessions, listOf(notice))

        assertEquals(listOf("notice/n", "waiting/wait"), overview.attention.map { it.key })
        assertEquals("noticed", assertIs<AttentionItem.FromNotice>(overview.attention[0]).session?.id)
        assertEquals(listOf("work"), overview.working.map { it.session.id })
        assertEquals(listOf("idle-new", "idle-old", "exited"), overview.recent.map { it.session.id })
    }

    @Test
    fun nestsAgentsUnderTheSessionThatStartedThem() {
        val sessions = listOf(
            session("lead", Activity.Working, 1),
            session("helper", Activity.Working, 2, owner = "lead"),
            session("orphan", Activity.Idle, 3, owner = "gone"),
        )
        val overview = SessionOverview.of(sessions, emptyList())
        assertEquals(listOf("lead"), overview.working.map { it.session.id })
        assertEquals(listOf("helper"), overview.working.single().agents.map { it.id })
        // An agent whose owner isn't listed stands on its own.
        assertEquals(listOf("orphan"), overview.recent.map { it.session.id })
    }

    @Test
    fun keepsTheSessionsInReachOfAnAccountFullOfNotices() {
        val sessions = listOf(
            session("work", Activity.Working, 3),
            session("done", Activity.Idle, 2),
            session("asks", Activity.Waiting, 1),
            session("answered", Activity.Working, 4),
        )
        fun notice(id: String, kind: NoticeKind, session: String? = null, asks: Boolean = false) =
            Notice(id = id, kind = kind, asks = asks, title = kind.name, session = session)
        // Newest first, as the bridge sends them.
        val notices = (1..500).map { notice("r$it", NoticeKind.ReviewRequested, asks = true) } +
            notice("a2", NoticeKind.Waiting, "asks", asks = true) +
            notice("a1", NoticeKind.Waiting, "asks", asks = true) +
            notice("gone", NoticeKind.Waiting, "closed", asks = true) +
            notice("stale", NoticeKind.Waiting, "answered", asks = true) +
            notice("f2", NoticeKind.Failed, "done") +
            notice("f1", NoticeKind.Succeeded, "done") +
            notice("w1", NoticeKind.Succeeded, "work") +
            notice("x", NoticeKind.Succeeded, "closed")
        val overview = SessionOverview.of(sessions, notices)

        assertEquals(listOf("notice/a2", "reviews"), overview.attention.map { it.key })
        assertEquals(listOf("a2", "a1"), assertIs<AttentionItem.FromNotice>(overview.attention[0]).covers)
        assertEquals(500, assertIs<AttentionItem.Reviews>(overview.attention[1]).notices.size)
        // The working session's "finished" belongs to an earlier turn.
        assertEquals(mapOf("done" to "f2"), overview.outcomes.mapValues { it.value.id })
        // "answered" is working again, so its old request is no longer a card.
        assertEquals(listOf("answered", "work"), overview.working.map { it.session.id })
        assertEquals(listOf("done"), overview.recent.map { it.session.id })
    }

    @Test
    fun takesTheAskingClassificationFromTheBridge() {
        val notices = listOf(
            Notice(id = "new-kind", asks = true),
            Notice(id = "quiet", kind = NoticeKind.AutomationPaused, asks = false),
        )
        val overview = SessionOverview.of(emptyList(), notices)
        assertEquals(listOf("notice/new-kind"), overview.attention.map { it.key })
    }
}
