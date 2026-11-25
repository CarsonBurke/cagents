package dev.cagents.mobile.android.ui

import android.app.Application
import android.os.SystemClock
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
     * clipboard, or one the camera read ([fromCamera]) that would replace
     * the computer this phone uses or trusts a different key at some of
     * its addresses; see [warnings].
     */
    data class Confirm(val code: PairingCode, val warnings: PairingWarnings, val fromCamera: Boolean = false) : PairingState

    data class Connecting(val code: PairingCode) : PairingState

    /** The computer is asking the person; [fingerprint] is this phone's key, which it shows beside the question. */
    data class Waiting(val code: PairingCode, val fingerprint: String) : PairingState

    data class Paired(val code: PairingCode) : PairingState

    data class Declined(val code: PairingCode) : PairingState

    /** Pairing failed; [code], when it may work on another try, such as after the network came back. */
    data class Failed(val message: String, val code: PairingCode? = null) : PairingState
}

/**
 * Runs one pairing for the pairing screen, starting from [link] when another
 * app opened this one with a code. Leaving the screen clears this model,
 * which closes the connection and so withdraws a pending request.
 */
class PairingViewModel(app: Application, link: String? = null) : AndroidViewModel(app) {
    private val graph = app.graph

    private val _state = MutableStateFlow<PairingState>(PairingState.Scanning)
    val state: StateFlow<PairingState> = _state.asStateFlow()

    private val _hint = MutableStateFlow<String?>(null)
    /** Why the last code the camera saw can't be used, while scanning goes on. */
    val hint: StateFlow<String?> = _hint.asStateFlow()

    private var job: Job? = null
    private var hintJob: Job? = null
    private val seen = SeenCodes()
    private var hintUntil = 0L

    /** What to say when the camera is still on the code from before scanning again. */
    private var stillThere = ""

    init {
        link?.let(::offered)
    }

    private fun now() = System.currentTimeMillis() / 1000

    /**
     * A frame the camera analyzed, with the code it read, if any. A usable
     * code coming into view starts pairing at once: pointing the camera at
     * it was the choice.
     */
    fun scanned(text: String?) {
        if (_state.value != PairingState.Scanning) return
        when (seen.frame(text, SystemClock.elapsedRealtime())) {
            SeenCodes.Sighting.Nothing -> return
            SeenCodes.Sighting.StillThere -> {
                showHint(stillThere)
                return
            }
            SeenCodes.Sighting.New -> {}
        }
        val read = text ?: return
        when (val parsed = PairingCode.parse(read, now())) {
            is PairingCode.Parsed.Code -> {
                val confirm = confirmation(parsed.code, fromCamera = true)
                if (confirm.warnings.any) _state.value = confirm else start(parsed.code)
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

    private fun confirmation(code: PairingCode, fromCamera: Boolean = false) = PairingState.Confirm(
        code,
        PairingWarnings.of(code, graph.settings.settings.value, graph.hostKeys::pinnedKey),
        fromCamera,
    )

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
        pair((_state.value as? PairingState.Confirm)?.code ?: return)
    }

    /** Tries a failed pairing's code again. */
    fun retry() {
        pair((_state.value as? PairingState.Failed)?.code ?: return)
    }

    private fun pair(code: PairingCode) {
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
            val retryable = now() <= code.expires + PairingCode.CLOCK_SKEW_SECONDS
            _state.value = when (outcome) {
                is PairOutcome.Paired -> {
                    // Connect with the phone's own key to the computer just saved.
                    // In the app's scope: leaving this screen, which comes next, mustn't cancel it.
                    graph.scope.launch { graph.controller.restart() }
                    PairingState.Paired(code)
                }
                PairOutcome.Declined -> PairingState.Declined(code)
                is PairOutcome.Failed -> PairingState.Failed(outcome.message, code.takeIf { retryable })
            }
        }
    }

    /**
     * Stops a pairing in progress, withdrawing the request, and scans again.
     * The camera, likely still pointed at the code just used, ignores it
     * until it has looked away.
     */
    fun scanAgain() {
        stillThere = if (_state.value is PairingState.Declined) {
            "That code was used. Show a new one on the computer."
        } else {
            "That's the code from before. Point the camera away and back to use it again."
        }
        job?.cancel()
        job = null
        seen.resume()
        hintJob?.cancel()
        _hint.value = null
        _state.value = PairingState.Scanning
    }

    /** Shows [message] for a few seconds, or for as long as it is shown again meanwhile, as with every frame of a code. */
    private fun showHint(message: String) {
        _hint.value = message
        hintUntil = SystemClock.elapsedRealtime() + HINT_MILLIS
        if (hintJob?.isActive == true) return
        hintJob = viewModelScope.launch {
            while (true) {
                val left = hintUntil - SystemClock.elapsedRealtime()
                if (left <= 0) break
                delay(left)
            }
            _hint.value = null
        }
    }

    private companion object {
        const val HINT_MILLIS = 4_000L
    }
}
