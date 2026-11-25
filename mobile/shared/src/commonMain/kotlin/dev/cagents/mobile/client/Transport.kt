package dev.cagents.mobile.client

/**
 * A way to reach `cagents daemon bridge --mobile`, such as an SSH connection
 * running it as the remote command.
 */
fun interface BridgeTransport {
    /** Opens a connection, or throws a [ConnectFailure]. */
    suspend fun open(): LineChannel
}

/** A bidirectional stream of text lines. */
interface LineChannel {
    /** The next line without its newline, or null once the other side closed. */
    suspend fun readLine(): String?

    /** Writes [line] and a newline. */
    suspend fun writeLine(line: String)

    /** Closes the connection; a pending [readLine] then returns null or throws. */
    fun close()
}

/**
 * Why a connection couldn't be opened. [retry] says whether trying again
 * without the person changing anything may help.
 */
sealed class ConnectFailure(message: String, cause: Throwable? = null) : Exception(message, cause) {
    abstract val retry: Boolean

    /** The computer couldn't be reached or dropped the connection. */
    class Network(message: String, cause: Throwable? = null) : ConnectFailure(message, cause) {
        override val retry get() = true
    }

    /**
     * The computer's host key isn't pinned yet; the person must decide
     * whether to trust it. [host] is the address that presented it.
     */
    class UnknownHostKey(val fingerprint: String, val keyType: String, val host: String = "") :
        ConnectFailure("This computer's identity isn't trusted yet.") {
        override val retry get() = false
    }

    /** The computer at [host] presented a different host key from the one pinned. */
    class HostKeyChanged(val fingerprint: String, val pinned: String, val host: String = "") :
        ConnectFailure("This computer's identity changed since you trusted it.") {
        override val retry get() = false
    }

    /** The computer refused the phone's key. */
    class Unauthorized(message: String) : ConnectFailure(message) {
        override val retry get() = false
    }

    /** The bridge refused to start or speak with this app. */
    class Refused(message: String) : ConnectFailure(message) {
        override val retry get() = false
    }
}
