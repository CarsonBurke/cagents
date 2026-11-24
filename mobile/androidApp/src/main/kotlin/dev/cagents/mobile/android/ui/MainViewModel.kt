package dev.cagents.mobile.android.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.android.graph
import dev.cagents.mobile.android.ssh.PhoneKey
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.ConnectionLost
import dev.cagents.mobile.client.RemoteState
import dev.cagents.mobile.client.RequestFailed
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.SendMode
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

data class PhoneKeyInfo(val openSsh: String, val fingerprint: String, val hardwareBacked: Boolean)

class MainViewModel(app: Application) : AndroidViewModel(app) {
    private val graph = app.graph
    private val controller = graph.controller

    val remote: StateFlow<RemoteState> = controller.state
    val settings: StateFlow<ConnectionSettings> = graph.settings.settings

    private val _phoneKey = MutableStateFlow<Result<PhoneKeyInfo>?>(null)
    val phoneKey: StateFlow<Result<PhoneKeyInfo>?> = _phoneKey.asStateFlow()

    /** The fingerprint trusted for the configured computer, if any. */
    val pinnedFingerprint: StateFlow<String?> = combine(settings, graph.hostKeys.revision) { settings, _ ->
        settings.hostKeyIds.firstNotNullOfOrNull(graph.hostKeys::pinned)
    }.stateIn(viewModelScope, SharingStarted.Eagerly, null)

    private val _manualSetup = MutableStateFlow(false)
    /** Whether Settings shows the fields for setting up a computer by hand, rather than leading with pairing. */
    val manualSetup: StateFlow<Boolean> = _manualSetup.asStateFlow()

    fun showManualSetup(show: Boolean = true) {
        _manualSetup.value = show
    }

    private val _pairingLink = MutableStateFlow<String?>(null)
    /** A pairing code that another app opened this one with, waiting to be shown. */
    val pairingLink: StateFlow<String?> = _pairingLink.asStateFlow()

    fun openPairingLink(link: String?) {
        _pairingLink.value = link
    }

    private val _messages = Channel<String>(Channel.BUFFERED)
    /** Short messages for a snackbar, such as why an action failed. */
    val messages = _messages.receiveAsFlow()

    init {
        viewModelScope.launch {
            _phoneKey.value = withContext(Dispatchers.IO) {
                runCatching { PhoneKey.get(app).let { PhoneKeyInfo(it.openSsh, it.fingerprint, it.hardwareBacked) } }
            }
        }
    }

    fun saveSettings(new: ConnectionSettings) {
        graph.settings.save(new)
        // Closing the old connection may wait on the network.
        viewModelScope.launch(Dispatchers.Default) { controller.restart() }
    }

    fun trustHostKey(failure: ConnectFailure.UnknownHostKey) {
        val settings = settings.value
        graph.hostKeys.pin(listOf(settings.hostKeyId(failure.host.ifEmpty { settings.host })), failure.fingerprint, failure.keyType)
        controller.retryNow()
    }

    /** Forgets the trusted host key at every address, so the next connection asks again. */
    fun forgetHostKey() {
        graph.hostKeys.forget(settings.value.hostKeyIds)
        controller.retryNow()
    }

    private val _dismissedHostKey = MutableStateFlow<String?>(null)
    /** The host key fingerprint whose trust prompt the person put off. */
    val dismissedHostKey: StateFlow<String?> = _dismissedHostKey.asStateFlow()

    fun dismissHostKey(fingerprint: String) {
        _dismissedHostKey.value = fingerprint
    }

    fun reviewHostKey() {
        _dismissedHostKey.value = null
    }

    fun retryNow() = controller.retryNow()

    fun openConversation(session: String) = controller.open(session)
    fun closeConversation(session: String) = controller.close(session)
    fun rewatch(session: String) = controller.rewatch(session)

    /** Sends a message; [done] says whether the computer took it. */
    fun send(session: String, text: String, mode: SendMode, done: (Boolean) -> Unit) =
        act(onSuccess = { done(true) }, onFailure = { done(false) }) { controller.send(session, text, mode) }

    fun interrupt(session: String) = act { controller.interrupt(session) }

    fun loadEarlier(session: String) = act { controller.loadEarlier(session) }

    fun answer(session: String, request: String, action: String, answers: List<FieldAnswer>, done: () -> Unit) =
        act(onSuccess = { done() }, onFailure = { done() }) { controller.answer(session, request, action, answers) }

    fun acknowledge(notice: String) = act { controller.acknowledge(notice) }

    private fun act(
        onSuccess: () -> Unit = {},
        onFailure: () -> Unit = {},
        block: suspend () -> Unit,
    ) = viewModelScope.launch {
        try {
            block()
            onSuccess()
        } catch (e: RequestFailed) {
            onFailure()
            _messages.send(
                if (e.code == ErrorCode.UnknownOp) {
                    "The computer's cagents can't do this yet. Update cagents there."
                } else {
                    e.message ?: "The computer refused."
                },
            )
        } catch (e: ConnectionLost) {
            onFailure()
            _messages.send("Not connected to the computer. Try again once it reconnects.")
        }
    }
}
