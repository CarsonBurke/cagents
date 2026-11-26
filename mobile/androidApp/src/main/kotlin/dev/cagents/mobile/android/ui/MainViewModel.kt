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
import kotlin.random.Random

data class PhoneKeyInfo(val openSsh: String, val fingerprint: String, val hardwareBacked: Boolean)

class MainViewModel(app: Application) : AndroidViewModel(app) {
    private val graph = app.graph
    private val controller = graph.controller

    val remote: StateFlow<RemoteState> = controller.state
    val settings: StateFlow<ConnectionSettings> = graph.settings.settings

    private val _phoneKey = MutableStateFlow<Result<PhoneKeyInfo>?>(null)
    val phoneKey: StateFlow<Result<PhoneKeyInfo>?> = _phoneKey.asStateFlow()

    /** The fingerprint trusted at each of the configured computer's addresses that has one, in their order. */
    val pinnedFingerprints: StateFlow<Map<String, String>> = combine(settings, graph.hostKeys.revision) { settings, _ ->
        settings.hosts.mapNotNull { host -> graph.hostKeys.pinned(settings.hostKeyId(host))?.let { host to it } }.toMap()
    }.stateIn(viewModelScope, SharingStarted.Eagerly, emptyMap())

    private val _manualSetup = MutableStateFlow(false)
    /** Whether Settings shows the fields for setting up a computer by hand, rather than leading with pairing. */
    val manualSetup: StateFlow<Boolean> = _manualSetup.asStateFlow()

    fun showManualSetup(show: Boolean = true) {
        _manualSetup.value = show
    }

    /**
     * Pairing codes other apps opened this one with, by offer, until a
     * pairing screen takes them. Kept here rather than in the screen's
     * route, which is saved with the activity, so a code's one-time key is
     * never written out.
     */
    private val offeredLinks = mutableMapOf<Long, String>()

    private val _pairingOffer = MutableStateFlow<Long?>(null)
    /** The newest pairing code offered, to open a pairing screen for. */
    val pairingOffer: StateFlow<Long?> = _pairingOffer.asStateFlow()

    fun openPairingLink(link: String) {
        // Random, so no offer matches a pairing screen restored from before
        // the process was restarted.
        val offer = Random.nextLong(1, Long.MAX_VALUE)
        offeredLinks[offer] = link
        _pairingOffer.value = offer
    }

    /** The pairing code for [offer], once; null after that, or after the process was restarted. */
    fun takePairingLink(offer: Long): String? {
        if (_pairingOffer.value == offer) _pairingOffer.value = null
        return offeredLinks.remove(offer)
    }

    private val _messages = Channel<String>(Channel.BUFFERED)
    /** Short messages for a snackbar, such as why an action failed. */
    val messages = _messages.receiveAsFlow()

    init {
        loadPhoneKey { PhoneKey.get(getApplication()) }
    }

    /** Loads the phone's key again, after a failure that may have passed. */
    fun reloadPhoneKey() {
        loadPhoneKey { PhoneKey.get(getApplication()) }
    }

    /**
     * Replaces the phone's key with a new one, when the old one can't be
     * used. Computers that trusted the old key must be told of the new one.
     */
    fun makeNewPhoneKey() {
        loadPhoneKey { PhoneKey.replace(getApplication()) }.invokeOnCompletion {
            if (_phoneKey.value?.isSuccess == true) {
                _messages.trySend("Made a new key. Pair again with your computer, or add the new key there.")
            }
        }
    }

    private fun loadPhoneKey(load: () -> PhoneKey) = viewModelScope.launch {
        _phoneKey.value = null
        val loaded = withContext(Dispatchers.IO) {
            runCatching { load().let { PhoneKeyInfo(it.openSsh, it.fingerprint, it.hardwareBacked) } }
        }
        _phoneKey.value = loaded
        // A connection waiting for a usable key tries again with it.
        if (loaded.isSuccess) controller.retryNow()
    }

    fun saveSettings(new: ConnectionSettings) {
        val old = graph.settings.settings.value
        graph.settings.save(new)
        val changed = !old.sameComputer(graph.settings.settings.value)
        // Closing the old connection may wait on the network.
        viewModelScope.launch(Dispatchers.Default) { controller.restart(computerChanged = changed) }
    }

    fun trustHostKey(failure: ConnectFailure.UnknownHostKey) {
        val settings = settings.value
        graph.hostKeys.pin(listOf(settings.hostKeyId(failure.host.ifEmpty { settings.host })), failure.fingerprint, failure.keyType)
        controller.retryNow()
    }

    /**
     * Forgets the trusted host key at [host], or at every address when null,
     * so the next connection there asks again.
     */
    fun forgetHostKey(host: String? = null) {
        val settings = settings.value
        graph.hostKeys.forget(host?.let { listOf(settings.hostKeyId(it)) } ?: settings.hostKeyIds)
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
