package dev.cagents.mobile.client

import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.DetailPage
import dev.cagents.mobile.protocol.ErrorCode
import dev.cagents.mobile.protocol.ImageAttachment
import dev.cagents.mobile.protocol.RemoteError
import dev.cagents.mobile.protocol.Event
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.Notice
import dev.cagents.mobile.protocol.SendMode
import dev.cagents.mobile.protocol.Session
import dev.cagents.mobile.protocol.Welcome
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
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
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
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
    /**
     * An attempt is under way. [lastFailure] is why the one before it failed,
     * kept in view while this one runs, so the person learns what is wrong
     * rather than watching an attempt that may take half a minute.
     */
    data class Connecting(val lastFailure: String? = null) : ConnectionState
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
    /**
     * How long reaching the computer and starting the bridge may take,
     * whatever it waits on, such as a name lookup that never answers.
     */
    val connectTimeout: Duration = 30.seconds,
    /** How long the bridge, once started, has to answer `hello`. */
    val handshakeTimeout: Duration = 20.seconds,
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

    /**
     * Held while a connection is stopped, so that stops, restarts and the
     * background's stop take turns: each waits for the one before to have
     * ended its connection.
     */
    private val stopping = Mutex()

    /** Restarts under way, each of which starts the loop once done. Guarded by [locked]. */
    private var restarts = 0

    /**
     * Whether the person may be looking at the app. Only then are its
     * conversations watched: a watching phone counts as someone following
     * them on the computer.
     */
    private val foreground = MutableStateFlow(true)
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

    /** A conversation to bring in line, and why. */
    private data class Sync(val session: String, val kind: SyncKind)

    private enum class SyncKind {
        /**
         * Watch it if held and not ended, unwatch it otherwise. In the
         * background every one is unwatched once its requests are answered.
         */
        Settle,

        /**
         * Watch it if held, even when ended or already watched, for a fresh
         * reset. In the background, once the app returns.
         */
        Watch,

        /** Like [Watch], unless the recovery that asked for it was settled meanwhile. */
        Resync,
    }

    /** Conversations watched again to recover, whose updates wait for the reset. Guarded by [locked]. */
    private val resyncing = HashSet<String>()

    /**
     * Recoveries in a row for each conversation, with no update applying
     * cleanly in between. Guarded by [locked].
     */
    private val recoveries = HashMap<String, Int>()

    /**
     * Requests awaiting their answer, for each conversation. It stays
     * watched in the background until they have it: the computer answers
     * a request whose conversation is let go with an error, though it may
     * have done what was asked. Guarded by [locked].
     */
    private val inFlight = HashMap<String, Int>()

    /** Starts connecting, unless already running. */
    fun start() = locked { startLocked() }

    private fun startLocked() {
        // The restart starts it, once the old connection has ended.
        if (restarts > 0) return
        backgroundStop?.cancel()
        backgroundStop = null
        if (loop?.isActive == true) {
            wake.trySend(Unit)
            return
        }
        loop = scope.launch { connectLoop() }
    }

    /** Disconnects and stays offline until [start]. */
    suspend fun stop() {
        locked {
            backgroundStop?.cancel()
            backgroundStop = null
        }
        stopping.withLock { disconnect() }
    }

    /**
     * Ends the connection and waits until it has. Not cancellable: a stop
     * given up halfway would leave the old connection running unseen.
     */
    private suspend fun disconnect(onlyIf: () -> Boolean = { true }) = withContext(NonCancellable) {
        val job = locked {
            if (!onlyIf()) return@withContext
            loop.also { loop = null }
        }
        // Cancelled before the close, so the loop doesn't take the closed
        // connection for a dropped one and connect again meanwhile, by then
        // perhaps to another computer.
        job?.cancel()
        // Closing an SSH channel can block on the network; keep it off the caller's thread.
        withContext(Dispatchers.Default) { current?.close() }
        job?.join()
        // Unless a connection was started meanwhile, such as on returning to the app.
        locked { if (loop == null) _state.update { it.copy(connection = ConnectionState.Offline) } }
    }

    /**
     * Drops the connection and connects afresh, such as after the settings
     * changed. When [computerChanged], it first forgets what the last
     * computer sent, its sessions, notices and name, and the conversations
     * followed there, so none of it shows as the new computer's or is
     * watched there.
     */
    suspend fun restart(computerChanged: Boolean = false) = withContext(NonCancellable) {
        // Not cancellable: given up halfway, it would leave no connection.
        locked {
            restarts++
            backgroundStop?.cancel()
            backgroundStop = null
        }
        // Hidden at once, while the old connection closes, so nothing of it
        // can be opened meanwhile.
        if (computerChanged) _state.value = RemoteState(connection = ConnectionState.Connecting())
        stopping.withLock {
            disconnect()
            locked {
                // Again, for what the old connection sent as it closed.
                if (computerChanged) {
                    resyncing.clear()
                    recoveries.clear()
                    _state.value = RemoteState()
                }
                restarts--
                startLocked()
            }
        }
    }

    /** Skips the remaining backoff, such as when the network comes back. */
    fun retryNow() {
        if (loop?.isActive == true) wake.trySend(Unit) else start()
    }

    /**
     * Follows the app going to the background and back. In the background
     * overview watches stop at once, and open conversations stop once their
     * requests are answered. The connection stays
     * for [ControllerConfig.backgroundGrace], so a quick switch away doesn't
     * drop it, and then closes. Coming back watches them again and, if
     * [connect], connects.
     */
    fun setForeground(foreground: Boolean, connect: Boolean = true) {
        this.foreground.value = foreground
        if (foreground) {
            if (connect) start()
        } else {
            locked {
                backgroundStop?.cancel()
                backgroundStop = scope.launch {
                    delay(config.backgroundGrace)
                    val self = coroutineContext.job
                    // Unless the app came back meanwhile, which replaces or clears it.
                    stopping.withLock { disconnect(onlyIf = { backgroundStop === self }) }
                }
            }
        }
        // Settles each one by whether it is wanted now, after a quick switch
        // away and back, which may be no change at all.
        _state.value.conversations.keys.forEach { sync(it, SyncKind.Settle) }
    }

    /** Follows a conversation, now and after every reconnect. */
    fun open(session: String) {
        var added = false
        _state.update { state ->
            if (session in state.conversations) return@update state
            added = true
            state.copy(conversations = state.conversations + (session to Conversation(session)))
        }
        if (added) sync(session, SyncKind.Watch)
    }

    /** Stops following a conversation. */
    fun close(session: String) {
        _state.update { it.copy(conversations = it.conversations - session) }
        locked {
            resyncing -= session
            recoveries -= session
        }
        sync(session, SyncKind.Settle)
    }

    /** Asks for an ended conversation again. */
    fun rewatch(session: String) {
        if (session !in _state.value.conversations) return
        locked { recoveries -= session }
        sync(session, SyncKind.Watch)
    }

    private fun sync(session: String, kind: SyncKind) {
        syncs?.trySend(Sync(session, kind))
    }

    suspend fun send(session: String, text: String, mode: SendMode, image: ImageAttachment? = null) =
        call(if (image == null) Call.Send(session, text, mode) else Call.SendImage(session, text, mode, image))

    suspend fun readDetail(request: Call.ReadDetail): DetailPage {
        // Reads can end with their watch when the app goes to the background.
        val bridge = current ?: throw ConnectionLost("Not connected to the computer.")
        val page = bridge.call(request)
        if (page == null || page.kind != request.kind || (page.next != null && page.next == request.cursor)) {
            throw RequestFailed(RemoteError(ErrorCode.Internal, "The computer sent an unexpected transcript page."))
        }
        return page
    }

    suspend fun interrupt(session: String) = call(Call.Interrupt(session))
    suspend fun loadEarlier(session: String) = call(Call.LoadEarlier(session))
    suspend fun answer(session: String, request: String, action: String, answers: List<FieldAnswer>) =
        call(Call.Answer(session, request, action, answers))

    /** Refresh the read checkpoint without replacing the existing watch. */
    suspend fun refreshConversationForRead(session: String) {
        if (session in _state.value.conversations) {
            callForVisibleConversation(Call.RefreshReadCheckpoint(session))
        }
    }

    suspend fun readConversation(session: String, checkpoint: String) =
        callForVisibleConversation(Call.ReadConversation(session, checkpoint))

    private suspend fun callForVisibleConversation(request: Call.OnSession) {
        if (!foreground.value) return
        try {
            call(request)
        } catch (_: RequestFailed) {
            // Older bridges and a watch that was just closed can refuse this.
        } catch (_: ConnectionLost) {
            // A newly visible reset on the next connection can acknowledge it.
        }
    }

    suspend fun acknowledge(notice: String) {
        call(Call.AcknowledgeNotification(notice))
        _state.update { state -> state.copy(notices = state.notices.filterNot { it.id == notice }) }
    }

    private suspend fun call(call: Call) {
        val bridge = current ?: throw ConnectionLost("Not connected to the computer.")
        if (call !is Call.OnSession) {
            bridge.call(call)
            return
        }
        val session = call.session
        locked { inFlight[session] = (inFlight[session] ?: 0) + 1 }
        try {
            bridge.call(call)
        } finally {
            val last = locked {
                val left = inFlight.getValue(session) - 1
                if (left == 0) inFlight -= session else inFlight[session] = left
                left == 0
            }
            if (last && !foreground.value) sync(session, SyncKind.Settle)
        }
    }

    // -----------------------------------------------------------------------

    private suspend fun connectLoop() {
        var attempt = 0
        var lastFailure: String? = null
        while (true) {
            _state.update { it.copy(connection = ConnectionState.Connecting(lastFailure)) }
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
                lastFailure = null
                continue
            }
            val reason = sentence(failure.message)
            lastFailure = reason

            // Only a connection that held starts over; one that drops straight
            // after connecting, again and again, keeps backing off.
            if (connectedAt?.let { it.elapsedNow() >= config.stableAfter } == true) attempt = 0
            attempt++
            val backoff = backoff(attempt)
            _state.update {
                it.copy(connection = ConnectionState.Retrying(reason, attempt, backoff))
            }
            withTimeoutOrNull(backoff) { wake.receive() }
        }
    }

    /** Connects, serves the connection until it ends and returns why it ended. */
    private suspend fun connectOnce(onConnected: () -> Unit): String {
        // Kept as it opens: a timeout landing just as open() returns would otherwise drop it unclosed.
        var opened: LineChannel? = null
        val channel = withTimeoutOrNull(config.connectTimeout) { transport.open().also { opened = it } }
            ?: run {
                opened?.close()
                throw ConnectFailure.Network(
                    "The computer didn't answer within ${config.connectTimeout.inWholeSeconds} seconds. " +
                        "Is it awake, and is this phone on its network or on Tailscale?",
                )
            }
        val bridge = BridgeSession(channel, config.clientName, config.clientVersion)
        var queue: Channel<Sync>? = null
        try {
            val welcome = withTimeoutOrNull(config.handshakeTimeout) { bridge.handshake() }
                ?: throw ConnectFailure.Network(
                    "The computer accepted this phone, but cagents there didn't answer within ${config.handshakeTimeout.inWholeSeconds} seconds. " +
                        "Check that cagents is running there, and the command in Settings.",
                )
            onConnected()
            // Drain a wake that arrived while connecting, so it doesn't skip a later backoff.
            wake.tryReceive()
            val syncQueue = Channel<Sync>(Channel.UNLIMITED).also { queue = it }
            return coroutineScope {
                val reader = async { bridge.run { event -> handle(event) } }
                locked {
                    resyncing.clear()
                    recoveries.clear()
                    syncs = syncQueue
                    current = bridge
                }
                _state.update { it.copy(connection = ConnectionState.Connected, welcome = welcome) }
                launch(start = CoroutineStart.UNDISPATCHED) {
                    watchOverview(bridge, Call.WatchSessions, Call.UnwatchSessions)
                }
                launch(start = CoroutineStart.UNDISPATCHED) {
                    watchOverview(bridge, Call.WatchNotifications, Call.UnwatchNotifications)
                }
                launch {
                    // Opened before this connection, or while it was being made.
                    _state.value.conversations.keys.forEach { syncQueue.trySend(Sync(it, SyncKind.Watch)) }
                    reconcile(bridge, syncQueue)
                }
                launch { keepalive(bridge) }
                val reason = reader.await()
                coroutineContext.cancelChildren()
                reason
            }
        } finally {
            // Unless a newer connection has taken their place.
            locked {
                if (syncs === queue) syncs = null
                if (current === bridge) current = null
            }
            bridge.close()
        }
    }

    private suspend fun watchOverview(bridge: BridgeSession, watch: Call, unwatch: Call) {
        var requested = false
        try {
            foreground.collectLatest { looking ->
                if (looking == requested) return@collectLatest
                // A new foreground state supersedes a reply still in flight.
                // Unwatch is sent even if the original watch has not answered.
                requested = looking
                try {
                    bridge.call(if (looking) watch else unwatch)
                } catch (_: RequestFailed) {
                    // The other subscription and conversation watches can continue.
                }
            }
        } catch (_: ConnectionLost) {
            // The next connection starts with fresh subscriptions.
        }
    }

    /**
     * Reconciles the requested watch for each conversation. A new decision
     * for one conversation can supersede a reply that is still pending.
     */
    private suspend fun reconcile(bridge: BridgeSession, queue: Channel<Sync>) = coroutineScope {
        val watched = HashSet<String>()
        val replies = HashMap<String, Job>()
        // Asked for again in the background, to be watched on return.
        val deferred = HashSet<String>()
        for ((session, kind) in queue) {
            // A recovery given up on, or closed, since it was queued.
            if (kind == SyncKind.Resync && locked { session !in resyncing }) continue
            val held = _state.value.conversations[session]
            val looking = foreground.value || locked { session in inFlight }
            val force = when {
                held == null -> {
                    deferred -= session
                    false
                }
                looking -> deferred.remove(session) || kind != SyncKind.Settle
                else -> {
                    if (kind != SyncKind.Settle) deferred += session
                    false
                }
            }
            // An ended conversation, whose updates stopped or can't be read, is
            // let go unless asked for again; in the background, every one is
            // once its requests are answered.
            val wanted = held != null && looking && (force || held.ended == null)
            val call = when {
                wanted && (force || session !in watched) -> {
                    watched += session
                    Call.WatchConversation(session)
                }
                !wanted && watched.remove(session) -> Call.UnwatchConversation(session)
                else -> continue
            }
            replies.entries.removeAll { it.value.isCompleted }
            replies.remove(session)?.cancelAndJoin()
            replies[session] = launch(start = CoroutineStart.UNDISPATCHED) {
                try {
                    bridge.call(call)
                } catch (e: RequestFailed) {
                    ensureActive()
                    if (call is Call.WatchConversation) {
                        // An earlier watch may still stand if a replacement
                        // failed. Leave it eligible for an explicit unwatch.
                        locked { resyncing -= session }
                        markEnded(session, e.message ?: "This conversation can't be followed.")
                    }
                } catch (_: ConnectionLost) {
                    // Every held conversation is watched again after reconnect.
                }
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
                when {
                    desynced -> resync(session, "This conversation keeps falling out of step.")
                    update.reset -> locked { resyncing -= session }
                    // Back in step: a later slip starts the count over.
                    else -> locked { recoveries -= session }
                }
            }
            is Event.Unreadable -> {
                val session = event.session
                if (session !in _state.value.conversations) return
                val recovering = locked { session in resyncing }
                when {
                    // Even a fresh reset can't be read: watching again won't help.
                    recovering && event.reset -> giveUp(session, UNREADABLE)
                    // From before the reset that is on its way.
                    recovering -> Unit
                    else -> resync(session, UNREADABLE)
                }
            }
            is Event.ConversationReadCheckpoint -> _state.update { state ->
                val held = state.conversations[event.session] ?: return@update state
                if (held.ended != null) return@update state
                state.copy(conversations = state.conversations + (
                    event.session to held.copy(readCheckpoint = event.checkpoint)
                ))
            }
            is Event.ConversationEnded -> markEnded(event.session, event.message)
            is Event.TerminalScreenUpdated, is Event.TerminalWatchEnded,
            is Event.Closing, is Event.Unknown -> Unit
        }
    }

    /**
     * Watches [session] again for a fresh reset, unless that is already under
     * way, or gives up with [failure] once that has failed to help a few times in a row.
     */
    private fun resync(session: String, failure: String) {
        val start = locked {
            if (session in resyncing) return@locked null
            val count = (recoveries[session] ?: 0) + 1
            recoveries[session] = count
            if (count > MAX_RECOVERIES) return@locked false
            resyncing += session
            true
        } ?: return
        if (start) sync(session, SyncKind.Resync) else giveUp(session, failure)
    }

    private fun giveUp(session: String, message: String) {
        locked { resyncing -= session }
        markEnded(session, message)
        sync(session, SyncKind.Settle)
    }

    private companion object {
        /** [message] as a sentence to show, with a stand-in when there is none. */
        fun sentence(message: String?): String {
            val text = message?.trim().orEmpty().ifEmpty { return "The connection failed." }
            return if (text.last() in ".?!…") text else "$text."
        }

        const val MAX_RECOVERIES = 3
        const val UNREADABLE = "This app can't read part of this conversation. Update the app."
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
