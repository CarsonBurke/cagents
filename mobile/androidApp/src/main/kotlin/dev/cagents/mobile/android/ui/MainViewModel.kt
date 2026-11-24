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
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

data class PhoneKeyInfo(val openSsh: String, val hardwareBacked: Boolean)

class MainViewModel(app: Application) : AndroidViewModel(app) {
    private val graph = app.graph
    private val controller = graph.controller

    val remote: StateFlow<RemoteState> = controller.state
    val settings: StateFlow<ConnectionSettings> = graph.settings.settings

    private val _phoneKey = MutableStateFlow<Result<PhoneKeyInfo>?>(null)
    val phoneKey: StateFlow<Result<PhoneKeyInfo>?> = _phoneKey.asStateFlow()

    private val _pinned = MutableStateFlow(graph.hostKeys.pinned(settings.value.hostKeyId))
    /** The fingerprint trusted for the configured computer, if any. */
    val pinnedFingerprint: StateFlow<String?> = _pinned.asStateFlow()

    private val _messages = Channel<String>(Channel.BUFFERED)
    /** Short messages for a snackbar, such as why an action failed. */
    val messages = _messages.receiveAsFlow()

    init {
        viewModelScope.launch {
            _phoneKey.value = withContext(Dispatchers.IO) {
                runCatching { PhoneKey.get(app).let { PhoneKeyInfo(it.openSsh, it.hardwareBacked) } }
            }
        }
    }

    fun saveSettings(new: ConnectionSettings) {
        graph.settings.save(new)
        _pinned.value = graph.hostKeys.pinned(settings.value.hostKeyId)
        // Closing the old connection may wait on the network.
        viewModelScope.launch(Dispatchers.Default) { controller.restart() }
    }

    fun trustHostKey(failure: ConnectFailure.UnknownHostKey) {
        graph.hostKeys.pin(settings.value.hostKeyId, failure.fingerprint, failure.keyType)
        _pinned.value = failure.fingerprint
        controller.retryNow()
    }

    /** Forgets the trusted host key, so the next connection asks again. */
    fun forgetHostKey() {
        graph.hostKeys.forget(settings.value.hostKeyId)
        _pinned.value = null
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
