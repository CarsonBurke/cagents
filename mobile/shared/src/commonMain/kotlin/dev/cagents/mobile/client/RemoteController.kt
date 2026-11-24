package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.Event
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.SendMode
import dev.cagents.mobile.protocol.Session
import dev.cagents.mobile.protocol.Welcome
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.cancelChildren
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.atomicfu.locks.SynchronizedObject
import kotlinx.atomicfu.locks.synchronized
import kotlin.random.Random
import kotlin.time.Duration
import kotlin.time.Duration.Companion.milliseconds
import kotlin.time.Duration.Companion.seconds

sealed interface ConnectionState {
    /** Not connecting: not started, or stopped while the app is in the background. */
    data object Offline : ConnectionState
    data object Connecting : ConnectionState
    data object Connected : ConnectionState

    /** The last attempt failed; another follows after a backoff. */
    data class Retrying(val message: String, val attempt: Int, val delay: Duration) : ConnectionState

    /** Trying again won't help until the person acts, such as trusting a host key. */
    data class NeedsAction(val failure: ConnectFailure) : ConnectionState
}

data class RemoteState(
    val connection: ConnectionState = ConnectionState.Offline,
    /** The last computer that answered, kept while reconnecting. */
    val welcome: Welcome? = null,
    /** Null until the first list arrives. */
    val sessions: List<Session>? = null,
    /** Unacknowledged notices, newest first. */
    val notices: List<Notice> = emptyList(),
    /** The conversations the app follows, by session ID. They are watched again after a reconnect. */
    val conversations: Map<String, Conversation> = emptyMap(),
)

data class ControllerConfig(
    val clientName: String = "cagents-android",
    val clientVersion: String = "0.1.0",
    val pingInterval: Duration = 25.seconds,
    val pingTimeout: Duration = 15.seconds,
    val backoffStart: Duration = 1.seconds,
    val backoffMax: Duration = 30.seconds,
    /** How long the connection outlives the app going to the background. */
    val backgroundGrace: Duration = 30.seconds,
)

/**
 * Keeps one connection to the bridge, follows the sessions, notifications and
 * open conversations through it, and reconnects with backoff when it drops.
 * Every public function may be called from any thread.
 */
class RemoteController(
    private val scope: CoroutineScope,
    private val transport: BridgeTransport,
    private val config: ControllerConfig = ControllerConfig(),
    private val random: Random = Random.Default,
) {
    private val _state = MutableStateFlow(RemoteState())
    val state: StateFlow<RemoteState> = _state.asStateFlow()

    private var loop: Job? = null
    private var backgroundStop: Job? = null
    private val wake = Channel<Unit>(Channel.CONFLATED)

    @kotlin.concurrent.Volatile
    private var current: BridgeSession? = null

    /** Starts connecting, unless already running. */
    fun start() = locked {
        backgroundStop?.cancel()
        backgroundStop = null
        if (loop?.isActive == true) {
            wake.trySend(Unit)
            return@locked
        }
        loop = scope.launch { connectLoop() }
    }

    /** Disconnects and stays offline until [start]. */
    suspend fun stop() {
        locked {
            backgroundStop?.cancel()
            backgroundStop = null
        }
        disconnect()
    }

    private suspend fun disconnect() {
        val job = locked { loop.also { loop = null } }
        current?.close()
        job?.cancelAndJoin()
        _state.update { it.copy(connection = ConnectionState.Offline) }
    }

    /** Drops the connection and connects afresh, such as after the settings changed. */
    suspend fun restart() {
        stop()
        start()
    }

    /** Skips the remaining backoff, such as when the network comes back. */
    fun retryNow() {
        if (loop?.isActive == true) wake.trySend(Unit) else start()
    }

    /**
     * Follows the app going to the background and back. In the background the
     * connection stays for [ControllerConfig.backgroundGrace], so a quick
     * switch away doesn't drop it, and then closes; it no longer counts as the
     * person looking at the open conversations.
     */
    fun setForeground(foreground: Boolean) {
        if (foreground) {
            start()
            return
        }
        locked {
            backgroundStop?.cancel()
            backgroundStop = scope.launch {
                delay(config.backgroundGrace)
                disconnect()
            }
        }
    }

    /** Follows a conversation, now and after every reconnect. */
    fun open(session: String) {
        var added = false
        _state.update { state ->
            if (session in state.conversations) return@update state
            added = true
            state.copy(conversations = state.conversations + (session to Conversation(session)))
        }
        if (added) current?.let { watch(it, session) }
    }

    /** Stops following a conversation. */
    fun close(session: String) {
        _state.update { it.copy(conversations = it.conversations - session) }
        current?.let { bridge ->
            scope.launch { runCatching { bridge.call(Call.UnwatchConversation(session)) } }
        }
    }

    /** Asks for an ended conversation again. */
    fun rewatch(session: String) {
        current?.let { watch(it, session) }
    }

    suspend fun send(session: String, text: String, mode: SendMode) = call(Call.Send(session, text, mode))
    suspend fun interrupt(session: String) = call(Call.Interrupt(session))
    suspend fun loadEarlier(session: String) = call(Call.LoadEarlier(session))
    suspend fun answer(session: String, request: String, action: String, answers: List<FieldAnswer>) =
        call(Call.Answer(session, request, action, answers))

    suspend fun acknowledge(notice: String) {
        call(Call.AcknowledgeNotification(notice))
        _state.update { state -> state.copy(notices = state.notices.filterNot { it.id == notice }) }
    }

    private suspend fun call(call: Call) {
        val bridge = current ?: throw ConnectionLost("Not connected to the computer.")
        bridge.call(call)
    }

    // -----------------------------------------------------------------------

    private suspend fun connectLoop() {
        var attempt = 0
        while (true) {
            _state.update { it.copy(connection = ConnectionState.Connecting) }
            val failure: ConnectFailure = try {
                ConnectFailure.Network(connectOnce { attempt = 0 })
            } catch (e: CancellationException) {
                throw e
            } catch (e: ConnectFailure) {
                e
            } catch (e: Exception) {
                ConnectFailure.Network(e.message ?: e::class.simpleName ?: "The connection failed.", e)
            }

            if (!failure.retry) {
                _state.update { it.copy(connection = ConnectionState.NeedsAction(failure)) }
                wake.receive()
                attempt = 0
                continue
            }

            attempt++
            val backoff = backoff(attempt)
            _state.update {
                it.copy(connection = ConnectionState.Retrying(failure.message.orEmpty(), attempt, backoff))
            }
            withTimeoutOrNull(backoff) { wake.receive() }
        }
    }

    /** Connects, serves the connection until it ends and returns why it ended. */
    private suspend fun connectOnce(onConnected: () -> Unit): String {
        val channel = transport.open()
        val bridge = BridgeSession(channel, config.clientName, config.clientVersion)
        try {
            val welcome = bridge.handshake()
            onConnected()
            // Drain a wake that arrived while connecting, so it doesn't skip a later backoff.
            wake.tryReceive()
            return coroutineScope {
                val reader = async { bridge.run { event -> handle(bridge, event) } }
                current = bridge
                _state.update { it.copy(connection = ConnectionState.Connected, welcome = welcome) }
                launch { subscribe(bridge) }
                launch { keepalive(bridge) }
                val reason = reader.await()
                coroutineContext.cancelChildren()
                reason
            }
        } finally {
            current = null
            bridge.close()
        }
    }

    private suspend fun subscribe(bridge: BridgeSession) {
        runCatching { bridge.call(Call.WatchSessions) }
        runCatching { bridge.call(Call.WatchNotifications) }
        _state.value.conversations.keys.forEach { watch(bridge, it) }
    }

    private suspend fun keepalive(bridge: BridgeSession) {
        while (true) {
            delay(config.pingInterval)
            val answered = try {
                withTimeout(config.pingTimeout) { bridge.call(Call.Ping) }
                true
            } catch (e: RequestFailed) {
                true
            } catch (e: CancellationException) {
                // withTimeout's own cancellation is a missed ping; an outer one ends the loop.
                if (e is kotlinx.coroutines.TimeoutCancellationException) false else throw e
            } catch (e: ConnectionLost) {
                return
            }
            if (!answered) {
                bridge.close()
                return
            }
        }
    }

    private fun watch(bridge: BridgeSession, session: String) {
        scope.launch {
            try {
                bridge.call(Call.WatchConversation(session))
            } catch (e: RequestFailed) {
                // A watch that a newer watch replaced is answered `unavailable`; the newer one stands.
                if (e.code == ErrorCode.Unavailable) return@launch
                markEnded(session, e.message ?: "This conversation can't be followed.")
            } catch (e: ConnectionLost) {
                // Watched again after the reconnect.
            }
        }
    }

    private fun markEnded(session: String, message: String) {
        _state.update { state ->
            val held = state.conversations[session] ?: return@update state
            state.copy(conversations = state.conversations + (session to held.copy(ended = message)))
        }
    }

    private fun handle(bridge: BridgeSession, event: Event) {
        when (event) {
            is Event.Sessions -> _state.update { it.copy(sessions = event.sessions) }
            is Event.Notifications -> _state.update { it.copy(notices = event.notices) }
            is Event.Conversation -> {
                var desynced = false
                _state.update { state ->
                    val held = state.conversations[event.update.session] ?: return@update state
                    val applied = held.apply(event.update)
                    desynced = applied.desynced
                    if (desynced) return@update state
                    state.copy(conversations = state.conversations + (held.session to applied.conversation))
                }
                // Watching again resets it; `watch` doesn't wait for the answer the reader would read.
                if (desynced) watch(bridge, event.update.session)
            }
            is Event.ConversationEnded -> markEnded(event.session, event.message)
            is Event.Closing, is Event.Unknown -> Unit
        }
    }

    private fun backoff(attempt: Int): Duration {
        val base = config.backoffStart * (1 shl (attempt - 1).coerceIn(0, 10))
        val capped = if (base > config.backoffMax) config.backoffMax else base
        // Up to a fifth either way, so several phones don't retry in step.
        val jitter = 0.8 + random.nextDouble() * 0.4
        return (capped.inWholeMilliseconds * jitter).toLong().milliseconds
    }

    private val lockObject = SynchronizedObject()
    private inline fun <T> locked(block: () -> T): T = synchronized(lockObject, block)
}
