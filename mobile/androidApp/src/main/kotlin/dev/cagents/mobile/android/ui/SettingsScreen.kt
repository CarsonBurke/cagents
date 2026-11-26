package dev.cagents.mobile.android.ui

import android.content.ClipData
import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.relocation.BringIntoViewRequester
import androidx.compose.foundation.relocation.bringIntoViewRequester
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
import androidx.compose.material.icons.outlined.Key
import androidx.compose.material.icons.outlined.QrCodeScanner
import androidx.compose.material.icons.outlined.Share
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material3.AlertDialog
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
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
import dev.cagents.mobile.android.ssh.PhoneKeyUnusable
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(
    vm: MainViewModel,
    snackbar: SnackbarHostState,
    canGoBack: Boolean,
    onBack: () -> Unit,
    onConnected: () -> Unit,
    onScan: () -> Unit,
) {
    val saved by vm.settings.collectAsStateWithLifecycle()
    val phoneKey by vm.phoneKey.collectAsStateWithLifecycle()
    val phoneKeyBusy by vm.phoneKeyBusy.collectAsStateWithLifecycle()
    // A key that failed may load now; the person has come to see.
    LaunchedEffect(vm) { vm.recheckPhoneKey() }
    val keySection = remember { BringIntoViewRequester() }
    LaunchedEffect(vm) { vm.newPhoneKeys.collect { keySection.bringIntoView() } }
    val pinned by vm.pinnedFingerprints.collectAsStateWithLifecycle()
    var host by rememberSaveable { mutableStateOf(saved.hosts.joinToString(", ")) }
    var port by rememberSaveable { mutableStateOf(saved.port.toString()) }
    var user by rememberSaveable { mutableStateOf(saved.user) }
    var command by rememberSaveable { mutableStateOf(saved.command) }
    var advanced by rememberSaveable { mutableStateOf(saved.command != ConnectionSettings.DEFAULT_COMMAND) }
    val firstRun = !saved.isComplete
    val manualSetup by vm.manualSetup.collectAsStateWithLifecycle()
    // A first run leads with pairing, and a paired computer needs no fields;
    // they are for setting a computer up by hand.
    val paired = !firstRun && saved.name.isNotEmpty()
    val manual = (!firstRun && !paired) || manualSetup
    val hosts = ConnectionSettings.parseHosts(host)
    val draft = ConnectionSettings(
        hosts = hosts,
        port = port.toIntOrNull() ?: 0,
        user = user.trim(),
        command = command,
        // The paired computer's name goes with its addresses.
        name = if (hosts == saved.hosts) saved.name else "",
    )
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

            // Nothing connects without the key, so its problem comes first, set up by hand or not.
            phoneKey?.exceptionOrNull()?.let { problem ->
                PhoneKeyProblem(
                    problem,
                    busy = phoneKeyBusy,
                    onTryAgain = vm::reloadPhoneKey,
                    onMakeNew = vm::makeNewPhoneKey,
                )
            }

            if (firstRun) {
                Button(onClick = onScan, modifier = Modifier.fillMaxWidth().height(56.dp)) {
                    Icon(Icons.Outlined.QrCodeScanner, null, Modifier.size(20.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Scan pairing code")
                }
                Text(
                    "On your computer, click the phone icon in the status bar, then Pair a phone.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (!manual) {
                    TextButton(onClick = { vm.showManualSetup() }) { Text("Set up by hand instead") }
                }
            }
            if (paired && !manual) {
                SectionTitle("Computer")
                Text("Paired with ${saved.name}", style = MaterialTheme.typography.titleMedium)
                Text(
                    "${saved.user}@${saved.hosts.joinToString(", ")}, port ${saved.port}",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                OutlinedButton(onClick = onScan, modifier = Modifier.fillMaxWidth()) {
                    Icon(Icons.Outlined.QrCodeScanner, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("Pair again or with another computer")
                }
                TextButton(onClick = { vm.showManualSetup() }) { Text("Edit the connection by hand") }
            }
            if (manual) {
                SectionTitle("Computer")
                if (!firstRun) {
                    OutlinedButton(onClick = onScan, modifier = Modifier.fillMaxWidth()) {
                        Icon(Icons.Outlined.QrCodeScanner, null, Modifier.size(18.dp))
                        Spacer(Modifier.width(8.dp))
                        Text("Pair by scanning a code")
                    }
                }
                OutlinedTextField(
                    value = host,
                    onValueChange = { host = it },
                    label = { Text("Address") },
                    placeholder = { Text("192.168.1.20 or workstation.local") },
                    supportingText = if (hosts.size > 1 || saved.hosts.size > 1) {
                        { Text("Tried in order. Separate addresses with commas.") }
                    } else {
                        null
                    },
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
                Column(Modifier.bringIntoViewRequester(keySection), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    PhoneKeySection(phoneKey, user.trim(), snackbar)
                }
            }

            if (pinned.isNotEmpty() && saved.isComplete) {
                HorizontalDivider(Modifier.padding(vertical = 8.dp))
                SectionTitle("Trusted computer")
                val keys = pinned.values.distinct()
                if (keys.size == 1) {
                    Text(
                        "${saved.label} (${pinned.keys.joinToString(", ")}, port ${saved.port}) is trusted with this host key fingerprint:",
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    Fingerprint(keys.single())
                } else {
                    // Trusted on first use one address at a time, so they may not all be the same computer.
                    Text(
                        "${saved.label}'s addresses are trusted with different host keys. Each should be the computer's own:",
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    for ((host, key) in pinned) {
                        Text("$host, port ${saved.port}", style = MaterialTheme.typography.labelMedium)
                        Fingerprint(key)
                    }
                }
                if (paired) {
                    // Forgetting it would trust whichever key answers next, instead of the one its code named.
                    Text(
                        "The key came from its pairing code. If ${saved.label} was reinstalled, pair with it again.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    OutlinedButton(onClick = { vm.forgetHostKey() }) { Text("Forget this computer's key") }
                }
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
        phoneKey == null -> Text("Loading this phone's key…", style = MaterialTheme.typography.bodyMedium)
        // Shown at the top of Settings, by PhoneKeyProblem.
        key == null -> Text("See the problem above.", style = MaterialTheme.typography.bodyMedium)
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

/**
 * Why the phone's key didn't load, and the way out. A key that is damaged or
 * gone leads with making a new one; any other failure may pass, so it leads
 * with trying again, and offers a new key only as a last resort.
 */
@Composable
private fun PhoneKeyProblem(problem: Throwable, busy: Boolean, onTryAgain: () -> Unit, onMakeNew: () -> Unit) {
    val unusable = problem is PhoneKeyUnusable
    var confirming by rememberSaveable { mutableStateOf(false) }
    var details by rememberSaveable { mutableStateOf(false) }
    val onCard = MaterialTheme.colorScheme.onErrorContainer
    Surface(
        color = MaterialTheme.colorScheme.errorContainer,
        contentColor = onCard,
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Key, null, Modifier.size(20.dp))
                Spacer(Modifier.width(8.dp))
                Text(
                    if (unusable) "This phone's key can't be used" else "Couldn't load this phone's key",
                    style = MaterialTheme.typography.titleMedium,
                )
            }
            Text(
                if (unusable) {
                    "The key this phone signs in with is damaged or gone, so no computer will accept it. " +
                        "Make a new key, then pair again with your computer, or add the new key there."
                } else {
                    "This phone's secure storage didn't answer. Try again in a moment. " +
                        "If it keeps failing, you can make a new key, which computers will need to be given."
                },
                style = MaterialTheme.typography.bodyMedium,
            )
            if (!unusable && details) {
                SelectionContainer {
                    Text(problem.toString(), style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                if (unusable) {
                    Button(onClick = { confirming = true }, enabled = !busy) { Text("Make a new key") }
                    TextButton(onClick = onTryAgain, enabled = !busy) { Text("Try again", color = onCard) }
                } else {
                    Button(onClick = onTryAgain, enabled = !busy) { Text("Try again") }
                    TextButton(onClick = { confirming = true }, enabled = !busy) { Text("Make a new key", color = onCard) }
                    TextButton(onClick = { details = !details }) { Text(if (details) "Hide details" else "Details", color = onCard) }
                }
            }
        }
    }
    if (confirming) {
        AlertDialog(
            onDismissRequest = { confirming = false },
            title = { Text("Make a new key?") },
            text = {
                Text(
                    if (unusable) {
                        "The old key is deleted. Computers that accepted it won't accept this phone " +
                            "until you pair again, or add the new key there."
                    } else {
                        "The current key may only be unavailable for now, and a new key deletes it for good. " +
                            "Computers that accepted it won't accept this phone until you pair again, or add the new key there."
                    },
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    confirming = false
                    onMakeNew()
                }) { Text("Make a new key", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = { TextButton(onClick = { confirming = false }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
}
