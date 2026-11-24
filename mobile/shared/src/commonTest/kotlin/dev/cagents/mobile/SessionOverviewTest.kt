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
        val notice = Notice(id = "n", kind = NoticeKind.Waiting, session = "noticed")
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
}
