package dev.cagents.mobile.android.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CloudOff
import androidx.compose.material.icons.outlined.Error
import androidx.compose.material.icons.outlined.Key
import androidx.compose.material.icons.outlined.WarningAmber
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.ConnectionState
import dev.cagents.mobile.protocol.Activity

/**
 * Says what the connection is doing when it isn't simply connected, with
 * the one thing the person can do about it.
 */
@Composable
fun ConnectionBanner(
    connection: ConnectionState,
    hasData: Boolean,
    onRetry: () -> Unit,
    onSettings: () -> Unit,
    onReviewHostKey: () -> Unit,
) {
    AnimatedVisibility(
        visible = connection != ConnectionState.Connected,
        enter = expandVertically(),
        exit = shrinkVertically(),
    ) {
        when (connection) {
            ConnectionState.Connected -> Unit
            ConnectionState.Connecting ->
                if (hasData) {
                    LinearProgressIndicator(Modifier.fillMaxWidth())
                } else {
                    Banner(icon = null, text = "Connecting…", progress = true)
                }
            ConnectionState.Offline ->
                Banner(Icons.Outlined.CloudOff, "Offline", action = "Connect" to onRetry)
            is ConnectionState.Retrying ->
                Banner(
                    Icons.Outlined.CloudOff,
                    connection.message.ifBlank { "Connection lost." } + " Reconnecting…",
                    action = "Retry now" to onRetry,
                )
            is ConnectionState.NeedsAction -> when (val failure = connection.failure) {
                is ConnectFailure.UnknownHostKey, is ConnectFailure.HostKeyChanged ->
                    Banner(Icons.Outlined.Key, failure.message.orEmpty(), action = "Review" to onReviewHostKey)
                is ConnectFailure.Unauthorized ->
                    Banner(Icons.Outlined.Key, failure.message.orEmpty(), action = "Settings" to onSettings)
                else ->
                    Banner(Icons.Outlined.Error, failure.message.orEmpty(), action = "Settings" to onSettings, error = true)
            }
        }
    }
}

@Composable
private fun Banner(
    icon: ImageVector?,
    text: String,
    action: Pair<String, () -> Unit>? = null,
    progress: Boolean = false,
    error: Boolean = false,
) {
    val container = if (error) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.secondaryContainer
    val content = if (error) MaterialTheme.colorScheme.onErrorContainer else MaterialTheme.colorScheme.onSecondaryContainer
    Surface(color = container, contentColor = content, modifier = Modifier.fillMaxWidth()) {
        Row(
            Modifier.padding(start = 16.dp, end = 8.dp, top = 6.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            when {
                progress -> CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = content)
                icon != null -> Icon(icon, contentDescription = null, Modifier.size(20.dp))
            }
            Spacer(Modifier.width(12.dp))
            Text(text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f).padding(vertical = 6.dp))
            action?.let { (label, onClick) ->
                TextButton(onClick = onClick) { Text(label, color = content) }
            }
        }
    }
}

/**
 * Asks the person to trust a computer seen for the first time, or warns when
 * a trusted one presents a different key.
 */
@Composable
fun HostKeyDialogs(vm: MainViewModel, onPairAgain: () -> Unit) {
    val remote by vm.remote.collectAsStateWithLifecycle()
    val settings by vm.settings.collectAsStateWithLifecycle()
    val failure = (remote.connection as? ConnectionState.NeedsAction)?.failure
    val dismissedFor by vm.dismissedHostKey.collectAsStateWithLifecycle()

    when (failure) {
        is ConnectFailure.UnknownHostKey -> if (dismissedFor != failure.fingerprint) {
            AlertDialog(
                onDismissRequest = { vm.dismissHostKey(failure.fingerprint) },
                icon = { Icon(Icons.Outlined.Key, contentDescription = null) },
                title = { Text("Trust ${failure.host.ifEmpty { settings.host }}?") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                        Text("This is the first connection to this computer. Check that its fingerprint matches before trusting it.")
                        Fingerprint(failure.fingerprint)
                        Text(
                            "On the computer: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                },
                confirmButton = { TextButton(onClick = { vm.trustHostKey(failure) }) { Text("Trust") } },
                dismissButton = { TextButton(onClick = { vm.dismissHostKey(failure.fingerprint) }) { Text("Not now") } },
            )
        }
        is ConnectFailure.HostKeyChanged -> if (dismissedFor != failure.fingerprint) {
            AlertDialog(
                onDismissRequest = { vm.dismissHostKey(failure.fingerprint) },
                icon = { Icon(Icons.Outlined.WarningAmber, contentDescription = null, tint = MaterialTheme.colorScheme.error) },
                title = { Text("${failure.host.ifEmpty { settings.host }} changed its identity") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                        Text("Its host key differs from the one you trusted. If you didn't reinstall the computer or its SSH server, someone may be impersonating it.")
                        if (settings.name.isNotEmpty()) {
                            Text("If you did, pair again from the computer to trust its new key.")
                        }
                        Text("Now", style = MaterialTheme.typography.labelMedium)
                        Fingerprint(failure.fingerprint)
                        Text("Trusted", style = MaterialTheme.typography.labelMedium)
                        Fingerprint(failure.pinned)
                    }
                },
                confirmButton = {
                    TextButton(onClick = { vm.dismissHostKey(failure.fingerprint) }) { Text("Keep the old key") }
                },
                dismissButton = {
                    // A paired computer's key came from its pairing code, so a new one comes from a new code.
                    if (settings.name.isNotEmpty()) {
                        TextButton(onClick = {
                            vm.dismissHostKey(failure.fingerprint)
                            onPairAgain()
                        }) { Text("Pair again") }
                    } else {
                        TextButton(onClick = { vm.forgetHostKey(failure.host.ifEmpty { null }) }) {
                            Text("Forget old key", color = MaterialTheme.colorScheme.error)
                        }
                    }
                },
            )
        }
        else -> Unit
    }
}

@Composable
fun Fingerprint(fingerprint: String) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHighest, shape = RoundedCornerShape(8.dp)) {
        SelectionContainer {
            Text(
                fingerprint,
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(10.dp),
            )
        }
    }
}

/** A small mark for what a session or conversation is doing. */
@Composable
fun ActivityMark(activity: Activity, exited: Boolean, modifier: Modifier = Modifier) {
    val status = LocalStatusColors.current
    Box(modifier.size(20.dp), contentAlignment = Alignment.Center) {
        when {
            exited -> Dot(MaterialTheme.colorScheme.outlineVariant)
            activity == Activity.Working -> CircularProgressIndicator(
                Modifier.size(16.dp),
                strokeWidth = 2.dp,
                color = status.working,
            )
            activity == Activity.Waiting -> Dot(status.attention, size = 12)
            activity == Activity.Idle -> Dot(MaterialTheme.colorScheme.outline)
            else -> Dot(MaterialTheme.colorScheme.outlineVariant)
        }
    }
}

@Composable
private fun Dot(color: Color, size: Int = 10) {
    Box(Modifier.size(size.dp).background(color, CircleShape))
}

fun activityLabel(activity: Activity, exited: Boolean): String = when {
    exited -> "Exited"
    activity == Activity.Working -> "Working"
    activity == Activity.Waiting -> "Needs you"
    activity == Activity.Idle -> "Idle"
    else -> "Unknown"
}

/** A compact age such as `now`, `5m`, `3h` or `2d`. */
fun relativeAge(millis: Long?, now: Long = System.currentTimeMillis()): String? {
    if (millis == null || millis <= 0) return null
    val seconds = (now - millis) / 1000
    return when {
        seconds < 60 -> "now"
        seconds < 3600 -> "${seconds / 60}m"
        seconds < 86_400 -> "${seconds / 3600}h"
        seconds < 7 * 86_400 -> "${seconds / 86_400}d"
        else -> "${seconds / (7 * 86_400)}w"
    }
}
