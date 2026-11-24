package dev.cagents.mobile.android.ui

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.LocalActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.ContentPaste
import androidx.compose.material.icons.outlined.Error
import androidx.compose.material.icons.outlined.NoPhotography
import androidx.compose.material.icons.outlined.PhonelinkLock
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.cagents.mobile.android.scan.QrScanner
import dev.cagents.mobile.pairing.PairingCode
import kotlinx.coroutines.launch

/**
 * Pairs the phone with a computer by its pairing code: scanned with the
 * camera, or offered as [link] by another app. [onManual] leads to setting
 * the computer up by hand instead.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PairingScreen(
    main: MainViewModel,
    link: String?,
    onBack: () -> Unit,
    onPaired: () -> Unit,
    onManual: () -> Unit,
    vm: PairingViewModel = viewModel(),
) {
    val state by vm.state.collectAsStateWithLifecycle()
    val hint by vm.hint.collectAsStateWithLifecycle()
    val current by main.settings.collectAsStateWithLifecycle()
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()

    LaunchedEffect(link) {
        if (link != null) vm.linked(link)
    }
    LaunchedEffect(state) {
        if (state is PairingState.Paired) onPaired()
    }

    val paste: () -> Unit = {
        scope.launch {
            val text = clipboard.getClipEntry()?.clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text
            vm.offered(text?.toString().orEmpty())
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Pair with your computer") },
                navigationIcon = { IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, "Back") } },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp, vertical = 16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            when (val s = state) {
                PairingState.Scanning -> Scanning(hint, onScanned = vm::scanned, onPaste = paste, onManual = onManual)
                is PairingState.Confirm -> Confirm(s.code, current.label.takeIf { current.isComplete && current.host !in s.code.hosts }, vm::confirm, vm::scanAgain)
                is PairingState.Connecting -> Progress("Connecting to ${s.code.name}…") {
                    OutlinedButton(onClick = vm::scanAgain) { Text("Cancel") }
                }
                is PairingState.Waiting -> Waiting(s.code, s.fingerprint, onCancel = vm::scanAgain)
                is PairingState.Paired -> Outcome(Icons.Outlined.CheckCircle, "Paired with ${s.code.name}", null) {}
                is PairingState.Declined -> Outcome(
                    Icons.Outlined.Error,
                    "Not allowed",
                    "${s.code.name} didn't allow this phone. Show a new code there to try again.",
                ) {
                    Button(onClick = vm::scanAgain) { Text("Scan again") }
                }
                is PairingState.Failed -> Outcome(Icons.Outlined.Error, "Couldn't pair", s.message, error = true) {
                    Button(onClick = vm::scanAgain) { Text("Scan again") }
                    TextButton(onClick = onManual) { Text("Set up by hand instead") }
                }
            }
        }
    }
}

@Composable
private fun Scanning(hint: String?, onScanned: (String) -> Unit, onPaste: () -> Unit, onManual: () -> Unit) {
    val context = LocalContext.current
    val activity = LocalActivity.current
    fun granted() = context.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
    var camera by remember { mutableStateOf(granted()) }
    // Asked once on arriving: the person just chose to scan, which is when the camera is needed.
    var asked by rememberSaveable { mutableStateOf(false) }
    var refused by rememberSaveable { mutableStateOf(false) }
    val request = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { allowed ->
        camera = allowed
        refused = !allowed
    }
    // Coming back from system settings, where camera access may have been turned on.
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { camera = granted() }
    LaunchedEffect(Unit) {
        if (!camera && !asked) {
            asked = true
            request.launch(Manifest.permission.CAMERA)
        }
    }

    Text(
        "On your computer, click the phone icon in the status bar, then Pair a phone. Point this phone's camera at the code it shows.",
        style = MaterialTheme.typography.bodyLarge,
        textAlign = TextAlign.Center,
    )
    Box(
        Modifier
            .widthIn(max = 360.dp)
            .fillMaxWidth()
            .aspectRatio(1f)
            .clip(RoundedCornerShape(24.dp)),
        contentAlignment = Alignment.Center,
    ) {
        if (camera) {
            QrScanner(onText = onScanned, modifier = Modifier.fillMaxSize())
            Surface(
                color = androidx.compose.ui.graphics.Color.Transparent,
                border = BorderStroke(3.dp, MaterialTheme.colorScheme.primary),
                shape = RoundedCornerShape(16.dp),
                modifier = Modifier.fillMaxSize(0.7f),
            ) {}
        } else {
            // Shown while the system asks, and after a refusal, so the reason is never missing.
            val permanently = refused && activity?.shouldShowRequestPermissionRationale(Manifest.permission.CAMERA) == false
            Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh, modifier = Modifier.fillMaxSize()) {
                Column(
                    Modifier.padding(24.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.CenterVertically),
                ) {
                    Icon(Icons.Outlined.NoPhotography, null, Modifier.size(40.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(
                        "The camera reads the pairing code. It's on only while this screen is open, and nothing is recorded.",
                        style = MaterialTheme.typography.bodyMedium,
                        textAlign = TextAlign.Center,
                    )
                    if (permanently) {
                        Button(onClick = {
                            context.startActivity(
                                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", context.packageName, null)),
                            )
                        }) { Text("Allow in Settings") }
                    } else {
                        Button(onClick = { request.launch(Manifest.permission.CAMERA) }) { Text("Allow camera") }
                    }
                }
            }
        }
    }
    Text(
        hint.orEmpty(),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.error,
        textAlign = TextAlign.Center,
        minLines = 2,
    )
    OutlinedButton(onClick = onPaste) {
        Icon(Icons.Outlined.ContentPaste, null, Modifier.size(18.dp))
        Spacer(Modifier.size(8.dp))
        Text("Paste a pairing code")
    }
    TextButton(onClick = onManual) { Text("Set up by hand instead") }
}

@Composable
private fun Confirm(code: PairingCode, replaces: String?, onPair: () -> Unit, onCancel: () -> Unit) {
    Icon(Icons.Outlined.PhonelinkLock, null, Modifier.size(48.dp), tint = MaterialTheme.colorScheme.primary)
    Text("Pair with ${code.name}?", style = MaterialTheme.typography.headlineSmall, textAlign = TextAlign.Center)
    Text(
        "This phone will ask ${code.user}@${code.hosts.first()} to let it follow and steer its agents. " +
            "Pair only with a computer you're setting up yourself.",
        style = MaterialTheme.typography.bodyMedium,
        textAlign = TextAlign.Center,
    )
    if (replaces != null) {
        Text(
            "It replaces $replaces as the computer this phone connects to.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
    Button(onClick = onPair, modifier = Modifier.fillMaxWidth().height(48.dp)) { Text("Pair") }
    TextButton(onClick = onCancel) { Text("Scan a code instead") }
}

@Composable
private fun Waiting(code: PairingCode, fingerprint: String, onCancel: () -> Unit) {
    CircularProgressIndicator()
    Text("Allow this phone on ${code.name}", style = MaterialTheme.typography.headlineSmall, textAlign = TextAlign.Center)
    Text(
        "The computer is asking whether to allow this phone. Check that the fingerprint it shows is this one, then click Allow there.",
        style = MaterialTheme.typography.bodyMedium,
        textAlign = TextAlign.Center,
    )
    Fingerprint(fingerprint)
    OutlinedButton(onClick = onCancel) { Text("Cancel") }
}

@Composable
private fun Progress(text: String, actions: @Composable () -> Unit) {
    Spacer(Modifier.height(32.dp))
    CircularProgressIndicator()
    Text(text, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
    actions()
}

@Composable
private fun Outcome(
    icon: ImageVector,
    title: String,
    message: String?,
    error: Boolean = false,
    actions: @Composable () -> Unit,
) {
    Spacer(Modifier.height(32.dp))
    Icon(
        icon,
        null,
        Modifier.size(48.dp),
        tint = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
    )
    Text(title, style = MaterialTheme.typography.headlineSmall, textAlign = TextAlign.Center)
    if (message != null) Text(message, style = MaterialTheme.typography.bodyMedium, textAlign = TextAlign.Center)
    actions()
}
