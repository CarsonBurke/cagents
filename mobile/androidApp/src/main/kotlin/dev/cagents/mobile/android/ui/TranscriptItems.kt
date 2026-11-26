package dev.cagents.mobile.android.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Block
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.Check
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.EditNote
import androidx.compose.material.icons.outlined.FolderOpen
import androidx.compose.material.icons.outlined.HourglassEmpty
import androidx.compose.material.icons.outlined.Image
import androidx.compose.material.icons.outlined.Inbox
import androidx.compose.material.icons.outlined.Language
import androidx.compose.material.icons.outlined.Psychology
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material.icons.outlined.StopCircle
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import dev.cagents.mobile.client.isBlankThinking
import dev.cagents.mobile.protocol.CommandAction
import dev.cagents.mobile.protocol.DetailKind
import dev.cagents.mobile.protocol.Item
import dev.cagents.mobile.protocol.ItemBody
import dev.cagents.mobile.protocol.Role
import dev.cagents.mobile.protocol.ToolStatus

/** One transcript item. Prose reads in full; tool activity is one line until opened. */
@Composable
internal fun TranscriptItem(item: Item, onDetail: ((TranscriptDetail) -> Unit)? = null) {
    if (item.isBlankThinking) return
    when (val body = item.body) {
        is ItemBody.Text -> when {
            item.role == Role.User -> UserMessage(body.text)
            body.thinking -> Thinking(item.id, body.text)
            item.role == Role.System -> SystemLine(body.text)
            else -> AssistantText(body.text, quiet = body.commentary)
        }
        is ItemBody.Command -> ActivityRow(
            id = item.id,
            icon = when (body.action) {
                CommandAction.Read -> Icons.Outlined.Description
                CommandAction.Search -> Icons.Outlined.Search
                CommandAction.List -> Icons.Outlined.FolderOpen
                else -> Icons.Outlined.Terminal
            },
            title = body.label.ifBlank { body.command },
            monospaceTitle = body.label.isBlank(),
            status = body.status,
            trailing = listOfNotNull(
                body.exitCode?.takeIf { it != 0 }?.let { "exit $it" },
                body.durationMs?.let(::formatDuration),
            ).joinToString(" · "),
        ) {
            if (body.label.isNotBlank()) Code(body.command)
            if (body.output.isNotBlank()) Code(body.output, clipped = body.clipped && onDetail == null)
            if (body.clipped && onDetail != null) {
                TextButton(onClick = { onDetail(TranscriptDetail(item.id, DetailKind.Output, "Command output")) }) {
                    Text("Read full output")
                }
            }
        }
        is ItemBody.Changes -> {
            val added = body.files.sumOf { it.addedLines }
            val removed = body.files.sumOf { it.removedLines }
            ActivityRow(
                id = item.id,
                icon = Icons.Outlined.EditNote,
                title = if (body.files.size == 1) body.files.single().path.substringAfterLast('/') else "Edited ${body.files.size} files",
                status = body.status,
                trailing = "+$added −$removed",
            ) {
                body.files.forEach { file ->
                    Row(
                        Modifier.fillMaxWidth()
                            .then(
                                if (onDetail == null) Modifier else Modifier.clickable {
                                    onDetail(TranscriptDetail(item.id, DetailKind.Diff, file.path, file.path))
                                },
                            )
                            .padding(vertical = 8.dp),
                    ) {
                        Text(
                            buildString {
                                file.from?.let { append(it).append(" → ") }
                                append(file.path)
                            },
                            style = MaterialTheme.typography.bodySmall,
                            fontFamily = FontFamily.Monospace,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            "+${file.addedLines} −${file.removedLines}",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
        }
        is ItemBody.WebSearch -> ActivityRow(
            id = item.id,
            icon = Icons.Outlined.Language,
            title = "Searched “${body.query}”",
            status = null,
            trailing = if (body.results.isEmpty()) "" else "${body.results.size} results",
        ) {
            body.results.forEach { result ->
                Column(Modifier.padding(vertical = 3.dp)) {
                    Text(result.title, style = MaterialTheme.typography.bodySmall, fontWeight = FontWeight.Medium)
                    Text(result.url, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
        is ItemBody.Tool -> ActivityRow(
            id = item.id,
            icon = Icons.Outlined.Build,
            title = toolName(body.name),
            status = body.status,
            trailing = "",
        ) {
            if (body.input.isNotBlank() && body.input != "{}") Code(body.input)
            if (body.output.isNotBlank()) Code(body.output, clipped = body.clipped && onDetail == null)
            if (body.clipped && onDetail != null) {
                TextButton(onClick = { onDetail(TranscriptDetail(item.id, DetailKind.Output, "Tool output")) }) {
                    Text("Read full output")
                }
            }
        }
        is ItemBody.Image -> ActivityRow(
            id = item.id,
            icon = Icons.Outlined.Image,
            title = body.description.ifBlank { "Image" },
            status = null,
            trailing = if (body.available && onDetail != null) "View" else "on the computer",
            expandable = false,
            onClick = if (body.available && onDetail != null) {
                { onDetail(TranscriptDetail(item.id, DetailKind.Image, body.description.ifBlank { "Image" })) }
            } else null,
        ) {}
        is ItemBody.Delivery -> Delivery(item.id, body)
        is ItemBody.Unknown -> SystemLine("Something this app can't show yet (${body.kind.ifBlank { "unknown" }})")
    }
}

@Composable
private fun UserMessage(text: String) {
    Box(Modifier.fillMaxWidth().padding(start = 48.dp, end = 12.dp, top = 10.dp, bottom = 4.dp), contentAlignment = Alignment.CenterEnd) {
        Surface(
            color = MaterialTheme.colorScheme.primaryContainer,
            contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
            shape = RoundedCornerShape(20.dp, 20.dp, 6.dp, 20.dp),
        ) {
            SelectionContainer {
                Text(text, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp))
            }
        }
    }
}

@Composable
private fun AssistantText(text: String, quiet: Boolean) {
    val markdown = remember { IncrementalMarkdown() }
    val blocks = remember(text) { markdown.blocks(text) }
    val color = if (quiet) MaterialTheme.colorScheme.onSurfaceVariant else MaterialTheme.colorScheme.onSurface
    val style = if (quiet) MaterialTheme.typography.bodyMedium else MaterialTheme.typography.bodyLarge
    SelectionContainer {
        Column(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            blocks.forEach { block ->
                when (block) {
                    is MarkdownBlock.Code -> Code(block.text)
                    is MarkdownBlock.Heading -> Text(
                        inlineMarkdown(block.text),
                        style = if (block.level <= 2) MaterialTheme.typography.titleMedium else MaterialTheme.typography.titleSmall,
                        color = color,
                    )
                    is MarkdownBlock.Paragraph -> Text(inlineMarkdown(block.text), style = style, color = color)
                }
            }
        }
    }
}

@Composable
private fun Thinking(id: String, text: String) {
    var open by rememberSaveable(id) { mutableStateOf(false) }
    Column(
        Modifier
            .fillMaxWidth()
            .clickable { open = !open }
            .padding(horizontal = 16.dp, vertical = 6.dp)
            .animateContentSize(),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(Icons.Outlined.Psychology, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.width(8.dp))
            Text(
                if (open) text else text.lineSequence().firstOrNull { it.isNotBlank() }?.trim().orEmpty().ifEmpty { "Thinking" },
                style = MaterialTheme.typography.bodySmall,
                fontStyle = FontStyle.Italic,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = if (open) Int.MAX_VALUE else 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@Composable
private fun SystemLine(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 6.dp),
    )
}

/** A compact, tappable line for tool activity; its detail opens beneath it. */
@Composable
private fun ActivityRow(
    id: String,
    icon: ImageVector,
    title: String,
    status: ToolStatus?,
    trailing: String,
    monospaceTitle: Boolean = false,
    expandable: Boolean = true,
    onClick: (() -> Unit)? = null,
    detail: @Composable () -> Unit,
) {
    var open by rememberSaveable(id) { mutableStateOf(false) }
    Column(
        Modifier
            .fillMaxWidth()
            .then(when {
                onClick != null -> Modifier.clickable(onClick = onClick)
                expandable -> Modifier.clickable { open = !open }
                else -> Modifier
            })
            .padding(horizontal = 16.dp, vertical = 5.dp)
            .animateContentSize(),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(icon, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.width(10.dp))
            Text(
                title,
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = if (monospaceTitle) FontFamily.Monospace else null,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            if (trailing.isNotEmpty()) {
                Spacer(Modifier.width(8.dp))
                Text(trailing, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            status?.let {
                Spacer(Modifier.width(8.dp))
                StatusIcon(it)
            }
        }
        if (open) {
            Column(Modifier.padding(start = 26.dp, top = 6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                detail()
            }
        }
    }
}

@Composable
private fun StatusIcon(status: ToolStatus) {
    val colors = LocalStatusColors.current
    when (status) {
        ToolStatus.Running -> CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 1.5.dp)
        ToolStatus.Succeeded, ToolStatus.Ended -> Icon(Icons.Outlined.Check, "Succeeded", Modifier.size(16.dp), tint = colors.success)
        ToolStatus.Failed -> Icon(Icons.Outlined.Close, "Failed", Modifier.size(16.dp), tint = MaterialTheme.colorScheme.error)
        ToolStatus.Denied -> Icon(Icons.Outlined.Block, "Denied", Modifier.size(16.dp), tint = MaterialTheme.colorScheme.error)
        ToolStatus.Stopped -> Icon(Icons.Outlined.StopCircle, "Stopped", Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        ToolStatus.Waiting -> Icon(Icons.Outlined.HourglassEmpty, "Waiting", Modifier.size(16.dp), tint = colors.attention)
        ToolStatus.Unknown -> Unit
    }
}

@Composable
private fun Delivery(id: String, body: ItemBody.Delivery) {
    var open by rememberSaveable(id) { mutableStateOf(false) }
    OutlinedCard(
        onClick = { open = !open },
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp),
    ) {
        Column(Modifier.padding(12.dp).animateContentSize()) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Inbox, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.secondary)
                Spacer(Modifier.width(8.dp))
                Text(
                    body.heading,
                    style = MaterialTheme.typography.labelLarge,
                    maxLines = if (open) Int.MAX_VALUE else 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (body.body.isNotBlank()) {
                Spacer(Modifier.size(6.dp))
                Text(
                    body.body + if (open && body.clipped) "\n…" else "",
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = if (open) Int.MAX_VALUE else 3,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

@Composable
fun Code(text: String, clipped: Boolean = false) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        shape = RoundedCornerShape(8.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column {
            if (clipped) {
                Text(
                    "Earlier output is on the computer",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(start = 10.dp, end = 10.dp, top = 8.dp),
                )
            }
            Box(
                Modifier
                    .heightIn(max = 360.dp)
                    .verticalScroll(rememberScrollState())
                    .horizontalScroll(rememberScrollState()),
            ) {
                Text(
                    text.trimEnd(),
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace,
                    softWrap = false,
                    modifier = Modifier.padding(10.dp).widthIn(min = 0.dp),
                )
            }
        }
    }
}

private fun toolName(name: String): String =
    if (name.startsWith("mcp__")) name.removePrefix("mcp__").replace("__", " · ") else name

private fun formatDuration(ms: Long): String = when {
    ms < 1000 -> "${ms}ms"
    ms < 60_000 -> "%.1fs".format(ms / 1000.0)
    else -> "${ms / 60_000}m ${(ms % 60_000) / 1000}s"
}

// ---------------------------------------------------------------------------
// A small Markdown subset: fenced code, headings, paragraphs, and inline
// `code`, **bold** and *emphasis*. Lists stay as written, which reads well.
// ---------------------------------------------------------------------------

sealed interface MarkdownBlock {
    data class Paragraph(val text: String) : MarkdownBlock
    data class Heading(val level: Int, val text: String) : MarkdownBlock
    data class Code(val text: String) : MarkdownBlock
}

fun markdownBlocks(text: String): List<MarkdownBlock> {
    val lines = text.lines()
    val reader = BlockReader()
    for (i in 0 until lines.lastIndex) reader.line(lines[i])
    return reader.ending(lines.last())
}

/**
 * Markdown's blocks read a whole line at a time, so text that grows is read
 * once: a paragraph or code block still open keeps growing until a line
 * closes it.
 */
private class BlockReader {
    /** The blocks closed so far. */
    private val closed = mutableListOf<MarkdownBlock>()
    private val paragraph = StringBuilder()
    private val code = StringBuilder()
    private var inCode = false

    fun line(line: String) {
        val trimmed = line.trimStart()
        when {
            isFence(trimmed) -> {
                if (inCode) {
                    closed += codeBlock("")
                    code.clear()
                } else {
                    paragraphBlock("")?.let(closed::add)
                    paragraph.clear()
                }
                inCode = !inCode
            }
            inCode -> code.append(line).append('\n')
            isHeading(trimmed) -> {
                paragraphBlock("")?.let(closed::add)
                paragraph.clear()
                closed += heading(trimmed)
            }
            line.isBlank() -> {
                paragraphBlock("")?.let(closed::add)
                paragraph.clear()
            }
            else -> paragraph.append(line).append('\n')
        }
    }

    /**
     * The blocks of the text read, ending with [last], a line with no line
     * break after it. Leaves this reader as it was, for the text to go on.
     */
    fun ending(last: String): List<MarkdownBlock> {
        val trimmed = last.trimStart()
        val open = when {
            // A fence still open is code still streaming.
            isFence(trimmed) -> if (inCode) listOf(codeBlock("")) else listOfNotNull(paragraphBlock(""), MarkdownBlock.Code(""))
            inCode -> listOf(codeBlock(last))
            isHeading(trimmed) -> listOfNotNull(paragraphBlock(""), heading(trimmed))
            last.isBlank() -> listOfNotNull(paragraphBlock(""))
            else -> listOfNotNull(paragraphBlock(last))
        }
        return if (open.isEmpty()) closed.toList() else closed + open
    }

    private fun paragraphBlock(more: String): MarkdownBlock? {
        val text = if (more.isEmpty()) paragraph.toString() else "$paragraph$more\n"
        return if (text.isBlank()) null else MarkdownBlock.Paragraph(text.trim('\n'))
    }

    private fun codeBlock(more: String) = MarkdownBlock.Code("$code$more".trimEnd('\n'))

    private fun isFence(trimmed: String) = trimmed.startsWith("```")

    private fun isHeading(trimmed: String) = trimmed.startsWith("#") && trimmed.trimStart('#').startsWith(" ")

    private fun heading(trimmed: String): MarkdownBlock {
        val level = trimmed.takeWhile { it == '#' }.length
        return MarkdownBlock.Heading(level, trimmed.drop(level).trim())
    }
}

/**
 * [markdownBlocks] for text that grows, as an answer streams. Each whole
 * line is read once, so an append costs its own length and the block still
 * open, not the whole answer. Text that doesn't extend the last is read
 * again from the start.
 */
class IncrementalMarkdown {
    private var reader = BlockReader()
    private var text = ""

    /** How much of [text] [reader] has read: whole lines, as [String.lines] has them. */
    private var read = 0

    /** Whether the last line read ended in a \r at the end of [text], which a \n may yet join. */
    private var lastEndedInReturn = false

    fun blocks(new: String): List<MarkdownBlock> {
        if (new.length < read || !new.regionMatches(0, text, 0, read)) {
            reader = BlockReader()
            read = 0
            lastEndedInReturn = false
        }
        text = new
        if (lastEndedInReturn && read < new.length) {
            if (new[read] == '\n') read++
            lastEndedInReturn = false
        }
        var start = read
        var i = read
        while (i < new.length) {
            val c = new[i]
            if (c == '\n' || c == '\r') {
                reader.line(new.substring(start, i))
                lastEndedInReturn = c == '\r' && i + 1 == new.length
                i += if (c == '\r' && i + 1 < new.length && new[i + 1] == '\n') 2 else 1
                start = i
            } else {
                i++
            }
        }
        read = start
        return reader.ending(new.substring(read))
    }
}

private val inlinePattern = Regex("""`([^`]+)`|\*\*([^*]+)\*\*|(?<![\w*])\*([^*\s][^*]*)\*(?![\w*])""")

@Composable
private fun inlineMarkdown(text: String): AnnotatedString {
    val codeBackground = MaterialTheme.colorScheme.surfaceContainerHigh
    val contentColor = LocalContentColor.current
    return remember(text, codeBackground, contentColor) {
        buildAnnotatedString {
            var last = 0
            for (match in inlinePattern.findAll(text)) {
                append(text, last, match.range.first)
                val (code, bold, emphasis) = match.destructured
                when {
                    code.isNotEmpty() -> withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = codeBackground)) { append(code) }
                    bold.isNotEmpty() -> withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append(bold) }
                    else -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(emphasis) }
                }
                last = match.range.last + 1
            }
            append(text, last, text.length)
        }
    }
}
