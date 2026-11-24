package dev.cagents.mobile.android

import dev.cagents.mobile.android.ui.MarkdownBlock
import dev.cagents.mobile.android.ui.markdownBlocks
import kotlin.test.Test
import kotlin.test.assertEquals

class MarkdownTest {
    @Test
    fun splitsProseHeadingsAndCode() {
        val text = "## Result\nThe build passes.\nAll green.\n\n```rust\nfn main() {}\n```\nDone."
        assertEquals(
            listOf(
                MarkdownBlock.Heading(2, "Result"),
                MarkdownBlock.Paragraph("The build passes.\nAll green."),
                MarkdownBlock.Code("fn main() {}"),
                MarkdownBlock.Paragraph("Done."),
            ),
            markdownBlocks(text),
        )
    }

    @Test
    fun anUnclosedFenceIsCodeStillStreaming() {
        assertEquals(
            listOf(MarkdownBlock.Paragraph("Here:"), MarkdownBlock.Code("cargo test")),
            markdownBlocks("Here:\n```\ncargo test"),
        )
    }
}
