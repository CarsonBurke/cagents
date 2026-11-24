package dev.cagents.mobile.android.ui

import android.content.ClipData
import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Share
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.cagents.mobile.android.BuildConfig
import dev.cagents.mobile.android.data.ConnectionSettings
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(
    vm: MainViewModel,
    snackbar: SnackbarHostState,
    canGoBack: Boolean,
    onBack: () -> Unit,
    onConnected: () -> Unit,
) {
    val saved by vm.settings.collectAsStateWithLifecycle()
    val phoneKey by vm.phoneKey.collectAsStateWithLifecycle()
    val pinned by vm.pinnedFingerprint.collectAsStateWithLifecycle()
    var host by rememberSaveable { mutableStateOf(saved.host) }
    var port by rememberSaveable { mutableStateOf(saved.port.toString()) }
    var user by rememberSaveable { mutableStateOf(saved.user) }
    var command by rememberSaveable { mutableStateOf(saved.command) }
    var advanced by rememberSaveable { mutableStateOf(saved.command != ConnectionSettings.DEFAULT_COMMAND) }
    val firstRun = !saved.isComplete
    val draft = ConnectionSettings(host.trim(), port.toIntOrNull() ?: 0, user.trim(), command)
    val changed = draft.copy(command = draft.command.trim()) != saved

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            TopAppBar(
                title = { Text(if (firstRun) "Connect to your computer" else "Settings") },
                navigationIcon = {
                    if (canGoBack) IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, "Back") }
                },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (firstRun) {
                Text(
                    "Follow and steer your computer's agents from here. The phone connects over SSH and runs cagents there; nothing goes through a server.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            SectionTitle("Computer")
            OutlinedTextField(
                value = host,
                onValueChange = { host = it },
                label = { Text("Address") },
                placeholder = { Text("192.168.1.20 or workstation.local") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next),
                modifier = Modifier.fillMaxWidth(),
            )
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedTextField(
                    value = user,
                    onValueChange = { user = it },
                    label = { Text("User") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii, imeAction = ImeAction.Next),
                    modifier = Modifier.weight(2f),
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { value -> port = value.filter(Char::isDigit).take(5) },
                    label = { Text("Port") },
                    singleLine = true,
                    isError = port.toIntOrNull()?.let { it !in 1..65535 } ?: true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number, imeAction = ImeAction.Done),
                    modifier = Modifier.weight(1f),
                )
            }
            if (advanced) {
                OutlinedTextField(
                    value = command,
                    onValueChange = { command = it },
                    label = { Text("Remote command") },
                    supportingText = { Text("Ignored when authorized_keys sets command= for this key, as recommended.") },
                    singleLine = true,
                    textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
                    modifier = Modifier.fillMaxWidth(),
                )
            } else {
                TextButton(onClick = { advanced = true }) { Text("Advanced") }
            }

            Button(
                onClick = {
                    vm.saveSettings(draft)
                    onConnected()
                },
                enabled = draft.isComplete && (changed || firstRun),
                modifier = Modifier.fillMaxWidth().height(48.dp),
            ) { Text(if (firstRun) "Connect" else "Save and reconnect") }

            HorizontalDivider(Modifier.padding(vertical = 8.dp))
            PhoneKeySection(phoneKey, user.trim(), snackbar)

            if (pinned != null && saved.isComplete) {
                HorizontalDivider(Modifier.padding(vertical = 8.dp))
                SectionTitle("Trusted computer")
                Text(
                    "${saved.host}:${saved.port} is trusted with this host key fingerprint:",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Fingerprint(pinned!!)
                OutlinedButton(onClick = vm::forgetHostKey) { Text("Forget this computer's key") }
            }

            Spacer(Modifier.height(8.dp))
            Text(
                "cagents for Android ${BuildConfig.VERSION_NAME}",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(16.dp))
        }
    }
}

@Composable
private fun PhoneKeySection(phoneKey: Result<PhoneKeyInfo>?, user: String, snackbar: SnackbarHostState) {
    val clipboard = LocalClipboard.current
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    SectionTitle("This phone's key")
    val key = phoneKey?.getOrNull()
    when {
        phoneKey == null -> Text("Making the key…", style = MaterialTheme.typography.bodyMedium)
        key == null -> Text(
            "Couldn't make this phone's key: ${phoneKey.exceptionOrNull()?.message}",
            color = MaterialTheme.colorScheme.error,
        )
        else -> {
            // The line authorized_keys needs, restricted to the bridge as docs/mobile-api.md prescribes.
            val home = if (user.isEmpty() || user == "root") "~" else "/home/$user"
            val line = "command=\"$home/.local/bin/cagents daemon bridge --mobile\",restrict ${key.openSsh}"
            Text(
                "Add this line to ~/.ssh/authorized_keys on the computer. It lets this key run only the cagents bridge. Change the path if cagents is installed elsewhere; it must be absolute.",
                style = MaterialTheme.typography.bodyMedium,
            )
            Surface(color = MaterialTheme.colorScheme.surfaceContainerHighest, shape = RoundedCornerShape(8.dp)) {
                SelectionContainer {
                    Text(
                        line,
                        fontFamily = FontFamily.Monospace,
                        style = MaterialTheme.typography.bodySmall,
                        modifier = Modifier.padding(12.dp),
                    )
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = {
                    scope.launch {
                        clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("authorized_keys line", line)))
                        snackbar.showSnackbar("Copied")
                    }
                }) {
                    Icon(Icons.Outlined.ContentCopy, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Copy line")
                }
                OutlinedButton(onClick = {
                    val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, line)
                    context.startActivity(Intent.createChooser(send, "Share the phone's key"))
                }) {
                    Icon(Icons.Outlined.Share, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Share")
                }
            }
            TextButton(onClick = {
                scope.launch {
                    clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("public key", key.openSsh)))
                    snackbar.showSnackbar("Copied the public key")
                }
            }) { Text("Copy the public key alone") }
            if (key.hardwareBacked) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Outlined.VerifiedUser, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Spacer(Modifier.width(8.dp))
                    Text(
                        "The private key stays in this phone's secure hardware.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
}
