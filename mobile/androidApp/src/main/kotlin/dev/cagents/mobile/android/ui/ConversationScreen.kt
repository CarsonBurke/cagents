package dev.cagents.mobile.android.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.Send
import androidx.compose.material.icons.outlined.ChecklistRtl
import androidx.compose.material.icons.outlined.KeyboardArrowDown
import androidx.compose.material.icons.outlined.Schedule
import androidx.compose.material.icons.outlined.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.cagents.mobile.client.Conversation
import dev.cagents.mobile.client.ConnectionState
import dev.cagents.mobile.protocol.ConversationState
import dev.cagents.mobile.protocol.PlanStatus
import dev.cagents.mobile.protocol.SendMode
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConversationScreen(
    vm: MainViewModel,
    session: String,
    snackbar: SnackbarHostState,
    onBack: () -> Unit,
) {
    DisposableEffect(session) {
        vm.openConversation(session)
        onDispose { vm.closeConversation(session) }
    }
    val remote by vm.remote.collectAsStateWithLifecycle()
    val conversation = remote.conversations[session]
    val listed = remote.sessions?.firstOrNull { it.id == session }
    val state = conversation?.state ?: ConversationState()
    val title = state.title ?: listed?.let(::sessionTitle) ?: "Conversation"

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            Column {
                TopAppBar(
                    navigationIcon = {
                        IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, "Back") }
                    },
                    title = {
                        Column {
                            Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            if (conversation?.loaded == true) {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    ActivityMark(state.activity, state.exited, Modifier.size(14.dp))
                                    Spacer(Modifier.width(6.dp))
                                    Text(
                                        listOfNotNull(
                                            activityLabel(state.activity, state.exited),
                                            listed?.providerLabel,
                                            state.model,
                                        ).joinToString(" · "),
                                        style = MaterialTheme.typography.labelMedium,
                                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                            }
                        }
                    },
                )
                ConnectionBanner(
                    connection = remote.connection,
                    hasData = conversation?.loaded == true,
                    onRetry = vm::retryNow,
                    onSettings = onBack,
                    onReviewHostKey = vm::reviewHostKey,
                )
                if (conversation?.loaded == true) PlanStrip(state)
            }
        },
        bottomBar = {
            Column(Modifier.imePadding().navigationBarsPadding()) {
                if (conversation != null) {
                    BottomPanel(vm, session, conversation, remote.connection == ConnectionState.Connected)
                }
            }
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            when {
                conversation == null || (!conversation.loaded && conversation.ended == null) ->
                    Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }
                else -> Transcript(vm, session, conversation)
            }
        }
    }
}

@Composable
private fun Transcript(vm: MainViewModel, session: String, conversation: Conversation) {
    // Newest at the bottom, anchored there: a reversed list follows the
    // stream by itself while the person is at the end, and keeps their place
    // when they have scrolled back.
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    val reversed = remember(conversation.items) { conversation.items.asReversed() }
    val awayFromEnd by remember { derivedStateOf { listState.firstVisibleItemIndex > 1 } }

    Box(Modifier.fillMaxSize()) {
        LazyColumn(
            state = listState,
            reverseLayout = true,
            contentPadding = PaddingValues(vertical = 8.dp),
            modifier = Modifier.fillMaxSize(),
        ) {
            items(reversed, key = { it.id }, contentType = { it.body::class }) { item ->
                TranscriptItem(item)
            }
            val state = conversation.state
            if (state.hasEarlier || state.loadingEarlier) {
                item(key = "earlier") {
                    Box(Modifier.fillMaxWidth().padding(12.dp), Alignment.Center) {
                        if (state.loadingEarlier) {
                            CircularProgressIndicator(Modifier.size(24.dp))
                        } else {
                            TextButton(onClick = { vm.loadEarlier(session) }) { Text("Load earlier messages") }
                        }
                    }
                }
            }
            if (conversation.items.isEmpty() && !state.hasEarlier) {
                item(key = "empty") {
                    Text(
                        "Nothing here yet.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.fillMaxWidth().padding(32.dp),
                    )
                }
            }
        }
        AnimatedVisibility(
            visible = awayFromEnd,
            enter = fadeIn(),
            exit = fadeOut(),
            modifier = Modifier.align(Alignment.BottomEnd).padding(16.dp),
        ) {
            SmallFloatingActionButton(onClick = { scope.launch { listState.animateScrollToItem(0) } }) {
                Icon(Icons.Outlined.KeyboardArrowDown, "Jump to latest")
            }
        }
    }
}

/** The plan, one line until opened. */
@Composable
private fun PlanStrip(state: ConversationState) {
    val plan = state.plan
    val running = state.subagents.count { !it.waiting && it.status == dev.cagents.mobile.protocol.ToolStatus.Running }
    if (plan.isEmpty() && running == 0) return
    var open by rememberSaveable { mutableStateOf(false) }
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = Modifier.fillMaxWidth()) {
        Column(
            Modifier
                .clickable(enabled = plan.isNotEmpty()) { open = !open }
                .padding(horizontal = 16.dp, vertical = 8.dp)
                .animateContentSize(),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.ChecklistRtl, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.width(8.dp))
                val done = plan.count { it.status == PlanStatus.Done }
                val current = plan.firstOrNull { it.status == PlanStatus.Active } ?: plan.firstOrNull { it.status == PlanStatus.Pending }
                Text(
                    buildList {
                        if (plan.isNotEmpty()) add("$done of ${plan.size}" + (current?.let { " · ${it.text}" } ?: ""))
                        if (running > 0) add(if (running == 1) "1 agent running" else "$running agents running")
                    }.joinToString(" · "),
                    style = MaterialTheme.typography.labelMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (open) {
                plan.forEach { step ->
                    Text(
                        (if (step.status == PlanStatus.Done) "✓  " else if (step.status == PlanStatus.Active) "▸  " else "○  ") + step.text,
                        style = MaterialTheme.typography.bodySmall,
                        color = if (step.status == PlanStatus.Done) MaterialTheme.colorScheme.onSurfaceVariant else MaterialTheme.colorScheme.onSurface,
                        modifier = Modifier.padding(start = 24.dp, top = 4.dp),
                    )
                }
            }
        }
    }
}

/** What waits for the person, what is queued, and the composer. */
@Composable
private fun BottomPanel(vm: MainViewModel, session: String, conversation: Conversation, connected: Boolean) {
    val state = conversation.state
    Surface(tonalElevation = 2.dp) {
        Column {
            conversation.ended?.let { message ->
                Notice(message, action = "Try again") { vm.rewatch(session) }
            }
            state.error?.let { Notice(it, error = true) }

            if (state.requests.isNotEmpty()) {
                Column(Modifier.heightIn(max = 420.dp).verticalScroll(rememberScrollState()).padding(top = 8.dp)) {
                    state.requests.forEach { request ->
                        RequestCard(request) { action, answers, done ->
                            vm.answer(session, request.id, action, answers, done)
                        }
                    }
                }
            }

            if (state.queued.isNotEmpty()) {
                Column(Modifier.padding(horizontal = 16.dp, vertical = 6.dp)) {
                    state.queued.forEach { queued ->
                        Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(vertical = 2.dp)) {
                            Icon(Icons.Outlined.Schedule, null, Modifier.size(14.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                            Spacer(Modifier.width(8.dp))
                            Text(
                                queued.text,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                }
            }

            val blocked = state.readOnly ?: when {
                conversation.ended != null -> "This conversation can't be followed right now."
                state.exited -> "This conversation has ended."
                else -> null
            }
            if (blocked != null) {
                Text(
                    blocked,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.fillMaxWidth().padding(16.dp),
                )
            } else {
                Composer(vm, session, state, connected)
            }
        }
    }
}

@Composable
private fun Notice(text: String, error: Boolean = false, action: String? = null, onAction: () -> Unit = {}) {
    Surface(
        color = if (error) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.secondaryContainer,
        contentColor = if (error) MaterialTheme.colorScheme.onErrorContainer else MaterialTheme.colorScheme.onSecondaryContainer,
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp),
    ) {
        Row(Modifier.padding(start = 14.dp, end = 4.dp, top = 4.dp, bottom = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f).padding(vertical = 8.dp))
            action?.let { TextButton(onClick = onAction) { Text(it) } }
        }
    }
}

/**
 * Writes to the conversation. While a turn runs, a message either steers it
 * or waits in the queue for it to end, and the stop button interrupts it.
 */
@Composable
private fun Composer(vm: MainViewModel, session: String, state: ConversationState, connected: Boolean) {
    var text by rememberSaveable(session) { mutableStateOf("") }
    var mode by rememberSaveable(session) { mutableStateOf(SendMode.Steer) }
    var sending by remember { mutableStateOf(false) }
    val busy = state.busy
    val canSend = connected && text.isNotBlank() && !sending && (state.ready || busy)

    Column(Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
        AnimatedVisibility(visible = busy && text.isNotBlank()) {
            SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth().padding(bottom = 8.dp)) {
                listOf(SendMode.Steer to "Steer now", SendMode.Queue to "After this turn").forEachIndexed { index, (value, label) ->
                    SegmentedButton(
                        selected = mode == value,
                        onClick = { mode = value },
                        shape = SegmentedButtonDefaults.itemShape(index, 2),
                    ) { Text(label) }
                }
            }
        }
        Row(verticalAlignment = Alignment.Bottom) {
            OutlinedTextField(
                value = text,
                onValueChange = { text = it },
                placeholder = {
                    Text(
                        when {
                            !connected -> "Waiting for the connection…"
                            busy -> "Steer or queue a message"
                            !state.ready -> "Starting…"
                            else -> "Message"
                        },
                    )
                },
                maxLines = 6,
                shape = RoundedCornerShape(24.dp),
                modifier = Modifier.weight(1f),
            )
            Spacer(Modifier.width(8.dp))
            if (busy && text.isBlank()) {
                FilledTonalIconButton(
                    onClick = { vm.interrupt(session) },
                    enabled = connected,
                    colors = IconButtonDefaults.filledTonalIconButtonColors(
                        containerColor = MaterialTheme.colorScheme.errorContainer,
                        contentColor = MaterialTheme.colorScheme.onErrorContainer,
                    ),
                    modifier = Modifier.size(52.dp),
                ) { Icon(Icons.Outlined.Stop, "Stop the turn") }
            } else {
                FilledIconButton(
                    onClick = {
                        sending = true
                        val sent = text
                        vm.send(session, sent.trim(), if (busy) mode else SendMode.Send) { delivered ->
                            sending = false
                            if (delivered && text == sent) text = ""
                        }
                    },
                    enabled = canSend,
                    modifier = Modifier.size(52.dp),
                ) { Icon(Icons.AutoMirrored.Outlined.Send, if (busy && mode == SendMode.Queue) "Queue" else "Send") }
            }
        }
    }
}
