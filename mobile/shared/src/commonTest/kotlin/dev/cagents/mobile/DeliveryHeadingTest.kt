package dev.cagents.mobile

import dev.cagents.mobile.protocol.DeliveryKind
import dev.cagents.mobile.protocol.ItemBody
import kotlin.test.Test
import kotlin.test.assertEquals

/** The same headings as `delivery_heading` in src/chat.rs, so phone and desktop agree. */
class DeliveryHeadingTest {
    private fun heading(kind: DeliveryKind, from: String = "", sender: String = "", summary: String = "") =
        ItemBody.Delivery(delivery = kind, from = from, sender = sender, summary = summary).heading

    @Test
    fun namesWhoSentItFirst() {
        assertEquals("Message from tester: Suite passes", heading(DeliveryKind.Message, "tester", "a3d7ff73", "Suite passes"))
        assertEquals("Report from Review: Done", heading(DeliveryKind.Report, "Review", summary = "Done"))
        assertEquals("Message from tester", heading(DeliveryKind.Message, "tester", "a3d7ff73"))
    }

    @Test
    fun anUnnamedSenderIsDescribedByItsSummaryOrShortIdentifier() {
        assertEquals("Agent \"Review\" finished", heading(DeliveryKind.Report, summary = "Agent \"Review\" finished"))
        assertEquals("Message from agent a3d7ff73", heading(DeliveryKind.Message, sender = "a3d7ff73-35a8-4f99"))
        assertEquals("Report from another agent", heading(DeliveryKind.Report))
    }

    @Test
    fun aNoticeIsItsOwnSummary() {
        assertEquals("Compacted", heading(DeliveryKind.Notice, from = "x", summary = "Compacted"))
        assertEquals("Background work changed state", heading(DeliveryKind.Notice))
        assertEquals("Background work changed state", heading(DeliveryKind.Unknown))
    }
}
