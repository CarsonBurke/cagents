package dev.cagents.mobile.android.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.automirrored.outlined.HelpOutline
import androidx.compose.material.icons.outlined.MarkChatUnread
import androidx.compose.material.icons.outlined.NotificationsActive
import androidx.compose.material.icons.outlined.PanTool
import androidx.compose.material.icons.outlined.RateReview
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.cagents.mobile.client.AttentionItem
import dev.cagents.mobile.client.ConnectionState
import dev.cagents.mobile.client.SessionEntry
import dev.cagents.mobile.client.SessionOverview
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.NoticeKind
import dev.cagents.mobile.protocol.RequestKind
import dev.cagents.mobile.protocol.Session
import dev.cagents.mobile.protocol.Surface
import kotlinx.coroutines.delay

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SessionsScreen(
    vm: MainViewModel,
    snackbar: SnackbarHostState,
    onOpen: (String) -> Unit,
    onSettings: () -> Unit,
) {
    val remote by vm.remote.collectAsStateWithLifecycle()
    val settings by vm.settings.collectAsStateWithLifecycle()
    val sessions = remote.sessions
    val overview = remember(sessions, remote.notices) {
        SessionOverview.of(sessions.orEmpty(), remote.notices)
    }
    // Ages are recomputed once a minute, not on every frame.
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(60_000)
            now = System.currentTimeMillis()
        }
    }
    val scrollBehavior = TopAppBarDefaults.pinnedScrollBehavior()

    Scaffold(
        modifier = Modifier.nestedScroll(scrollBehavior.nestedScrollConnection),
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            Column {
                TopAppBar(
                    title = {
                        Column {
                            Text(remote.welcome?.host ?: settings.label.ifBlank { "cagents" })
                            val subtitle = when (remote.connection) {
                                ConnectionState.Connected -> remote.welcome?.let { "cagents ${it.serverVersion}" }
                                else -> null
                            }
                            if (subtitle != null) {
                                Text(
                                    subtitle,
                                    style = MaterialTheme.typography.labelMedium,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    },
                    actions = {
                        IconButton(onClick = onSettings) { Icon(Icons.Outlined.Settings, "Settings") }
                    },
                    scrollBehavior = scrollBehavior,
                )
                ConnectionBanner(
                    connection = remote.connection,
                    hasData = sessions != null,
                    onRetry = vm::retryNow,
                    onSettings = onSettings,
                    onReviewHostKey = vm::reviewHostKey,
                )
            }
        },
    ) { padding ->
        when {
            sessions == null -> Box(Modifier.fillMaxSize().padding(padding), Alignment.Center) {
                if (remote.connection == ConnectionState.Connecting || remote.connection == ConnectionState.Connected) {
                    CircularProgressIndicator()
                }
            }
            overview.isEmpty -> EmptySessions(Modifier.padding(padding), remote.welcome?.host ?: settings.label)
            else -> LazyColumn(
                contentPadding = PaddingValues(
                    top = padding.calculateTopPadding(),
                    bottom = padding.calculateBottomPadding() + 16.dp,
                ),
                modifier = Modifier.fillMaxSize(),
            ) {
                if (overview.attention.isNotEmpty()) {
                    section("Needs you", overview.attention.size)
                    items(overview.attention, key = { it.key }) { item ->
                        AttentionCard(item, now, onOpen = onOpen, onDismiss = { ids -> ids.forEach(vm::acknowledge) })
                    }
                }
                if (overview.working.isNotEmpty()) {
                    section("Working", overview.working.size)
                    sessionEntries(overview.working, overview.outcomes, now, onOpen)
                }
                if (overview.recent.isNotEmpty()) {
                    section("Recent", null)
                    sessionEntries(overview.recent, overview.outcomes, now, onOpen)
                }
            }
        }
    }
}

private fun LazyListScope.section(title: String, count: Int?) {
    item(key = "section/$title", contentType = "section") {
        Row(
            Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(title, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
            if (count != null) {
                Spacer(Modifier.width(8.dp))
                Text("$count", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

private fun LazyListScope.sessionEntries(
    entries: List<SessionEntry>,
    outcomes: Map<String, Notice>,
    now: Long,
    onOpen: (String) -> Unit,
) {
    entries.forEach { entry ->
        item(key = "session/${entry.session.id}", contentType = "session") {
            SessionRow(entry.session, outcomes[entry.session.id], now, onOpen)
        }
        items(entry.agents, key = { "agent/${entry.session.id}/${it.id}" }, contentType = { "agent" }) { agent ->
            SessionRow(agent, outcomes[agent.id], now, onOpen, nested = true)
        }
    }
}

@Composable
private fun SessionRow(
    session: Session,
    outcome: Notice?,
    now: Long,
    onOpen: (String) -> Unit,
    nested: Boolean = false,
) {
    val terminal = session.surface == Surface.Terminal
    val openable = session.watchable
    val details = buildList {
        if (nested) add("Agent")
        session.providerLabel?.let(::add)
        if (!nested) session.project?.let(::add)
        if (terminal) add("Terminal")
        relativeAge(session.activeAt, now)?.let(::add)
    }.joinToString(" · ")

    ListItem(
        modifier = Modifier
            .then(if (openable) Modifier.clickable { onOpen(session.id) } else Modifier)
            .padding(start = if (nested) 24.dp else 0.dp),
        colors = ListItemDefaults.colors(
            headlineColor = if (openable) MaterialTheme.colorScheme.onSurface else MaterialTheme.colorScheme.onSurfaceVariant,
        ),
        leadingContent = {
            if (terminal) {
                Icon(Icons.Outlined.Terminal, null, Modifier.size(20.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            } else {
                ActivityMark(session.activity, session.exited)
            }
        },
        headlineContent = {
            Text(
                sessionTitle(session),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                style = if (nested) MaterialTheme.typography.bodyMedium else MaterialTheme.typography.bodyLarge,
            )
        },
        supportingContent = {
            Column {
                Text(details, maxLines = 1, overflow = TextOverflow.Ellipsis)
                outcome?.title?.takeIf { it.isNotBlank() }?.let {
                    val failed = outcome.kind == NoticeKind.Failed
                    Text(
                        it,
                        color = if (failed) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
                        style = MaterialTheme.typography.labelLarge,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                session.failure?.let {
                    Text(it, color = MaterialTheme.colorScheme.error, maxLines = 2, overflow = TextOverflow.Ellipsis)
                }
            }
        },
        trailingContent = if (session.exited) {
            { Text("Exited", style = MaterialTheme.typography.labelSmall) }
        } else {
            null
        },
    )
}

@Composable
private fun AttentionCard(item: AttentionItem, now: Long, onOpen: (String) -> Unit, onDismiss: (List<String>) -> Unit) {
    val status = LocalStatusColors.current
    val (icon, title, subtitle, body, target) = when (item) {
        is AttentionItem.FromNotice -> {
            val notice = item.notice
            AttentionContent(
                icon = noticeIcon(notice),
                title = notice.conversation ?: item.session?.let(::sessionTitle) ?: notice.title,
                subtitle = notice.title,
                body = notice.preview,
                target = item.session?.takeIf { it.watchable }?.id,
            )
        }
        is AttentionItem.Waiting -> AttentionContent(
            icon = Icons.Outlined.PanTool,
            title = sessionTitle(item.entry.session),
            subtitle = listOfNotNull(
                "Waiting for you",
                item.entry.session.providerLabel,
                relativeAge(item.entry.session.activeAt, now),
            ).joinToString(" · "),
            body = item.entry.session.failure,
            target = item.entry.session.takeIf { it.watchable }?.id,
        )
        is AttentionItem.Reviews -> AttentionContent(
            icon = Icons.Outlined.RateReview,
            title = if (item.notices.size == 1) "1 review request" else "${item.notices.size} review requests",
            subtitle = "Your review was requested",
            body = "Open them in cagents on the computer.",
            target = null,
        )
    }
    val content: @Composable () -> Unit = {
        Row(Modifier.padding(start = 16.dp, top = 12.dp, bottom = 12.dp, end = 4.dp)) {
            Icon(icon, null, tint = status.attention, modifier = Modifier.padding(top = 2.dp).size(22.dp))
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(title, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(subtitle, style = MaterialTheme.typography.labelLarge, color = status.attention, fontWeight = FontWeight.Medium)
                body?.takeIf { it.isNotBlank() }?.let {
                    Spacer(Modifier.height(2.dp))
                    Text(it, style = MaterialTheme.typography.bodyMedium, maxLines = 3, overflow = TextOverflow.Ellipsis)
                }
            }
            if (item is AttentionItem.FromNotice) {
                IconButton(onClick = { onDismiss(item.covers) }) {
                    Icon(Icons.Outlined.Close, "Dismiss notification", tint = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }
    val modifier = Modifier
        .fillMaxWidth()
        .padding(horizontal = 12.dp, vertical = 4.dp)
    // A card with nothing to open on the phone isn't a button.
    if (target != null) {
        ElevatedCard(onClick = { onOpen(target) }, modifier = modifier) { content() }
    } else {
        ElevatedCard(modifier = modifier) { content() }
    }
}

private data class AttentionContent(
    val icon: androidx.compose.ui.graphics.vector.ImageVector,
    val title: String,
    val subtitle: String,
    val body: String?,
    val target: String?,
)

private fun noticeIcon(notice: Notice) = when (notice.kind) {
    NoticeKind.Waiting -> when (notice.request) {
        RequestKind.Question -> Icons.AutoMirrored.Outlined.HelpOutline
        else -> Icons.Outlined.PanTool
    }
    NoticeKind.Succeeded -> Icons.Outlined.CheckCircle
    NoticeKind.Failed, NoticeKind.AutomationBlocked -> Icons.Outlined.ErrorOutline
    NoticeKind.ReviewRequested -> Icons.Outlined.RateReview
    NoticeKind.MessageReceived -> Icons.Outlined.MarkChatUnread
    else -> Icons.Outlined.NotificationsActive
}

fun sessionTitle(session: Session): String =
    session.title?.takeIf { it.isNotBlank() }
        ?: session.project?.takeIf { it.isNotBlank() }?.let { "Untitled · $it" }
        ?: session.providerLabel
        ?: "Untitled session"

@Composable
private fun EmptySessions(modifier: Modifier, host: String) {
    Column(
        modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("No sessions on $host", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            "Sessions you start in cagents on the computer appear here.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
