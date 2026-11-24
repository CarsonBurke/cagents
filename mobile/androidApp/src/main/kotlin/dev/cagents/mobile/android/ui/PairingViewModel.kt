package dev.cagents.mobile.android.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import dev.cagents.mobile.android.graph
import dev.cagents.mobile.android.ssh.PhoneKey
import dev.cagents.mobile.pairing.PairOutcome
import dev.cagents.mobile.pairing.PairingCode
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Where pairing stands, from scanning the code to the computer's answer. */
sealed interface PairingState {
    /** Looking for a code with the camera. */
    data object Scanning : PairingState

    /**
     * A code waiting for the person to confirm: one from a link or the
     * clipboard, or one that would replace the computer this phone uses
     * ([replaces]) or trusts a different key at some of its addresses
     * ([conflicts]).
     */
    data class Confirm(val code: PairingCode, val replaces: String?, val conflicts: List<String>) : PairingState

    data class Connecting(val code: PairingCode) : PairingState

    /** The computer is asking the person; [fingerprint] is this phone's key, which it shows beside the question. */
    data class Waiting(val code: PairingCode, val fingerprint: String) : PairingState

    data class Paired(val code: PairingCode) : PairingState

    data class Declined(val code: PairingCode) : PairingState

    data class Failed(val message: String) : PairingState
}

/**
 * Runs one pairing for the pairing screen. Leaving the screen clears this
 * model, which closes the connection and so withdraws a pending request.
 */
class PairingViewModel(app: Application) : AndroidViewModel(app) {
    private val graph = app.graph

    private val _state = MutableStateFlow<PairingState>(PairingState.Scanning)
    val state: StateFlow<PairingState> = _state.asStateFlow()

    private val _hint = MutableStateFlow<String?>(null)
    /** Why the last code the camera saw can't be used, while scanning goes on. */
    val hint: StateFlow<String?> = _hint.asStateFlow()

    private var job: Job? = null
    private var hintJob: Job? = null
    private var lastScanned: String? = null
    private var linked: String? = null

    private fun now() = System.currentTimeMillis() / 1000

    /** A code the camera read. A usable one starts pairing at once: pointing the camera at it was the choice. */
    fun scanned(text: String) {
        if (_state.value != PairingState.Scanning || text == lastScanned) return
        lastScanned = text
        when (val parsed = PairingCode.parse(text, now())) {
            is PairingCode.Parsed.Code -> {
                val confirm = confirmation(parsed.code)
                if (confirm.replaces != null || confirm.conflicts.isNotEmpty()) _state.value = confirm else start(parsed.code)
            }
            is PairingCode.Parsed.Invalid -> showHint(parsed.message)
            PairingCode.Parsed.NotACode -> showHint("That isn't a cagents pairing code.")
        }
    }

    /**
     * A code from somewhere other than the camera: a link another app
     * opened, or the clipboard. It waits for the person to confirm, since
     * they may not have chosen it.
     */
    fun offered(text: String) {
        if (_state.value is PairingState.Connecting || _state.value is PairingState.Waiting) return
        _state.value = when (val parsed = PairingCode.parse(text, now())) {
            is PairingCode.Parsed.Code -> confirmation(parsed.code)
            is PairingCode.Parsed.Invalid -> PairingState.Failed(parsed.message)
            PairingCode.Parsed.NotACode -> PairingState.Failed("That isn't a cagents pairing code. On the computer, click the phone icon in the status bar, then Pair a phone.")
        }
    }

    private fun confirmation(code: PairingCode): PairingState.Confirm {
        val current = graph.settings.settings.value
        val ids = current.copy(port = code.port)
        val conflicts = code.hosts.filter { host ->
            graph.hostKeys.pinned(ids.hostKeyId(host)).let { it != null && it != code.hostKey }
        }
        val replaces = current.label.takeIf { current.isComplete && current.hosts.none { it in code.hosts } }
        return PairingState.Confirm(code, replaces, conflicts)
    }

    /** A link another app opened this one with; offered once, however often the screen is recreated. */
    fun linked(link: String) {
        if (link == linked) return
        linked = link
        offered(link)
    }

    /** A code from the clipboard: the person chose it, but it is confirmed like a link. Anything else is only a hint. */
    fun pasted(text: String) {
        if (_state.value != PairingState.Scanning) return
        when (val parsed = PairingCode.parse(text, now())) {
            is PairingCode.Parsed.Code -> _state.value = confirmation(parsed.code)
            is PairingCode.Parsed.Invalid -> showHint(parsed.message)
            PairingCode.Parsed.NotACode -> showHint("The clipboard has no cagents pairing code.")
        }
    }

    fun confirm() {
        val code = (_state.value as? PairingState.Confirm)?.code ?: return
        if (now() > code.expires + PairingCode.CLOCK_SKEW_SECONDS) {
            _state.value = PairingState.Failed("This pairing code has expired. Show a new one on the computer.")
        } else {
            start(code)
        }
    }

    private fun start(code: PairingCode) {
        _hint.value = null
        _state.value = PairingState.Connecting(code)
        job = viewModelScope.launch {
            val outcome = try {
                val fingerprint = withContext(Dispatchers.IO) { PhoneKey.get(getApplication()).fingerprint }
                graph.pairing.pair(code) { _state.value = PairingState.Waiting(code, fingerprint) }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                PairOutcome.Failed("Couldn't pair: ${e.message ?: e::class.simpleName}")
            }
            _state.value = when (outcome) {
                is PairOutcome.Paired -> {
                    // Connect with the phone's own key to the computer just saved.
                    // In the app's scope: leaving this screen, which comes next, mustn't cancel it.
                    graph.scope.launch { graph.controller.restart() }
                    PairingState.Paired(code)
                }
                PairOutcome.Declined -> PairingState.Declined(code)
                is PairOutcome.Failed -> PairingState.Failed(outcome.message)
            }
        }
    }

    /** Stops a pairing in progress, withdrawing the request, and scans again. */
    fun scanAgain() {
        job?.cancel()
        job = null
        lastScanned = null
        _hint.value = null
        _state.value = PairingState.Scanning
    }

    private fun showHint(message: String) {
        _hint.value = message
        hintJob?.cancel()
        hintJob = viewModelScope.launch {
            delay(4_000)
            _hint.value = null
            // Lets the same code be read again, such as one whose problem the person has fixed.
            lastScanned = null
        }
    }
}
