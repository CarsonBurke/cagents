package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.Event
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.SendMode
import dev.cagents.mobile.protocol.Session
import dev.cagents.mobile.protocol.Welcome
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
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
import kotlin.time.TimeMark
import kotlin.time.TimeSource

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
    /** A connection that lasted this long starts the backoff over when it drops. */
    val stableAfter: Duration = 10.seconds,
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
    private val timeSource: TimeSource = TimeSource.Monotonic,
) {
    private val _state = MutableStateFlow(RemoteState())
    val state: StateFlow<RemoteState> = _state.asStateFlow()

    private var loop: Job? = null
    private var backgroundStop: Job? = null
    private val wake = Channel<Unit>(Channel.CONFLATED)

    @kotlin.concurrent.Volatile
    private var current: BridgeSession? = null

    /**
     * The current connection's queue of conversations to bring in line with
     * [RemoteState.conversations]. One coroutine works through it, so the
     * watches and unwatches for a session reach the bridge in the order the
     * app asked for them.
     */
    @kotlin.concurrent.Volatile
    private var syncs: Channel<Sync>? = null

    /** A conversation to watch or unwatch; [force] watches it again even when watched, for a fresh reset. */
    private data class Sync(val session: String, val force: Boolean)

    /** Conversations watched again to recover, whose updates wait for the reset. Guarded by [locked]. */
    private val resyncing = HashSet<String>()

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
        // Closing an SSH channel can block on the network; keep it off the caller's thread.
        withContext(NonCancellable + Dispatchers.Default) { current?.close() }
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
        if (added) sync(session, force = true)
    }

    /** Stops following a conversation. */
    fun close(session: String) {
        _state.update { it.copy(conversations = it.conversations - session) }
        locked { resyncing -= session }
        sync(session, force = false)
    }

    /** Asks for an ended conversation again. */
    fun rewatch(session: String) {
        if (session in _state.value.conversations) sync(session, force = true)
    }

    private fun sync(session: String, force: Boolean) {
        syncs?.trySend(Sync(session, force))
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
            var connectedAt: TimeMark? = null
            val failure: ConnectFailure = try {
                ConnectFailure.Network(connectOnce { connectedAt = timeSource.markNow() })
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

            // Only a connection that held starts over; one that drops straight
            // after connecting, again and again, keeps backing off.
            if (connectedAt?.let { it.elapsedNow() >= config.stableAfter } == true) attempt = 0
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
            val queue = Channel<Sync>(Channel.UNLIMITED)
            return coroutineScope {
                val reader = async { bridge.run { event -> handle(event) } }
                locked { resyncing.clear() }
                syncs = queue
                current = bridge
                _state.update { it.copy(connection = ConnectionState.Connected, welcome = welcome) }
                launch {
                    subscribe(bridge)
                    // Opened before this connection, or while it was being made.
                    _state.value.conversations.keys.forEach { queue.trySend(Sync(it, force = true)) }
                    reconcile(bridge, queue)
                }
                launch { keepalive(bridge) }
                val reason = reader.await()
                coroutineContext.cancelChildren()
                reason
            }
        } finally {
            syncs = null
            current = null
            bridge.close()
        }
    }

    private suspend fun subscribe(bridge: BridgeSession) {
        runCatching { bridge.call(Call.WatchSessions) }
        runCatching { bridge.call(Call.WatchNotifications) }
    }

    /**
     * Works through [queue], one request at a time, watching the conversations
     * the app holds and unwatching the ones it let go, until the connection ends.
     */
    private suspend fun reconcile(bridge: BridgeSession, queue: Channel<Sync>) {
        val watched = HashSet<String>()
        for ((session, force) in queue) {
            // A forced sync watches any held conversation, ended or not; otherwise
            // an ended one, whose updates stopped or can't be read, is let go.
            val held = _state.value.conversations[session]
            val wanted = held != null && (force || held.ended == null)
            try {
                if (wanted && (force || session !in watched)) {
                    watched += session
                    try {
                        bridge.call(Call.WatchConversation(session))
                    } catch (e: RequestFailed) {
                        // Whatever the reason, including `unavailable`, no updates follow.
                        watched -= session
                        locked { resyncing -= session }
                        markEnded(session, e.message ?: "This conversation can't be followed.")
                    }
                } else if (!wanted && session in watched) {
                    watched -= session
                    runCatching { bridge.call(Call.UnwatchConversation(session)) }
                        .onFailure { if (it is ConnectionLost) throw it }
                }
            } catch (e: ConnectionLost) {
                // Every held conversation is watched again after the reconnect.
                return
            }
        }
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

    private fun markEnded(session: String, message: String) {
        _state.update { state ->
            val held = state.conversations[session] ?: return@update state
            state.copy(conversations = state.conversations + (session to held.copy(ended = message)))
        }
    }

    private fun handle(event: Event) {
        when (event) {
            is Event.Sessions -> _state.update { it.copy(sessions = event.sessions) }
            is Event.Notifications -> _state.update { it.copy(notices = event.notices) }
            is Event.Conversation -> {
                val update = event.update
                val session = update.session
                // Recovering: the updates in flight before the reset no longer line up.
                if (!update.reset && locked { session in resyncing }) return
                var desynced = false
                _state.update { state ->
                    val held = state.conversations[session] ?: return@update state
                    val applied = held.apply(update)
                    desynced = applied.desynced
                    if (desynced) return@update state
                    state.copy(conversations = state.conversations + (session to applied.conversation))
                }
                if (desynced) {
                    resync(session)
                } else if (update.reset) {
                    locked { resyncing -= session }
                }
            }
            is Event.Unreadable -> {
                val session = event.session ?: return
                if (session !in _state.value.conversations) return
                // A second unreadable line before the reset means watching again won't help.
                if (locked { session in resyncing }) {
                    locked { resyncing -= session }
                    markEnded(session, "This app can't read part of this conversation. Update the app.")
                    sync(session, force = false)
                } else {
                    resync(session)
                }
            }
            is Event.ConversationEnded -> markEnded(event.session, event.message)
            is Event.Closing, is Event.Unknown -> Unit
        }
    }

    /** Watches [session] again for a fresh reset, unless that is already under way. */
    private fun resync(session: String) {
        if (locked { resyncing.add(session) }) sync(session, force = true)
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
