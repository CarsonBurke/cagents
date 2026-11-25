package dev.cagents.mobile.android

import dev.cagents.mobile.android.ui.IncrementalMarkdown
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

    private val samples = listOf(
        "## Result\nThe build passes.\nAll green.\n\n```rust\nfn main() {}\n```\nDone.",
        "Intro line\n# Heading right after\nMore text\n\n\n  ```\nindented fence\n\nwith a blank line\n  ```\nAfter.\n",
        "Windows\r\nlines\r\n\r\n```\r\ncode\r\n```\r\nend",
        "A lone\rreturn\r```\rcode\r```\rtext\n\nnext",
        "#not a heading\n### Three\n```\nunclosed\n\nstill code",
        "Ends\r\n\r\nin a heading\r\n## Last",
        "Split\r\n\r\r\n\n```\r\nx\r\r\n",
        "",
    )

    @Test
    fun streamedTextGivesTheSameBlocksAsTheWhole() {
        for (sample in samples) {
            for (chunk in listOf(1, 2, 3, 4, 7, 16)) {
                val markdown = IncrementalMarkdown()
                var end = 0
                while (end < sample.length) {
                    end = minOf(sample.length, end + chunk)
                    val text = sample.substring(0, end)
                    assertEquals(markdownBlocks(text), markdown.blocks(text), "${sample.take(20)}… in chunks of $chunk, at $end")
                }
            }
        }
    }

    @Test
    fun textThatChangesRatherThanGrowsStartsOver() {
        val markdown = IncrementalMarkdown()
        markdown.blocks("First paragraph.\n\nSecond.")
        assertEquals(markdownBlocks("Other.\n\nSecond."), markdown.blocks("Other.\n\nSecond."))
        assertEquals(markdownBlocks("Oth"), markdown.blocks("Oth"))
    }
}
