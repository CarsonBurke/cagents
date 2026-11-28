package dev.cagents.mobile.client

import kotlinx.coroutines.CancellableContinuation
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/** Adapter for platforms whose SSH implementation lives outside Kotlin. */
class NativeBridgeTransport(private val dialer: NativeSshDialer) : BridgeTransport {
    override suspend fun open(): LineChannel = suspendCancellableCoroutine { continuation ->
        val operation = dialer.open(NativeSshOpening(continuation))
        continuation.invokeOnCancellation { operation.cancel() }
    }
}

fun interface NativeSshDialer {
    fun open(completion: NativeSshOpening): NativeSshOperation
}

fun interface NativeSshOperation {
    fun cancel()
}

/** Each operation calls exactly one completion method, including on failure. */
interface NativeSshChannel {
    fun read(completion: NativeSshReading)
    fun write(line: String, completion: NativeSshWriting)
    fun close()
}

class NativeSshOpening internal constructor(private val continuation: CancellableContinuation<LineChannel>) {
    fun opened(channel: NativeSshChannel) {
        continuation.resume(NativeLineChannel(channel)) { _, result, _ -> result.close() }
    }

    fun network(message: String) = fail(ConnectFailure.Network(message))
    fun unauthorized(message: String) = fail(ConnectFailure.Unauthorized(message))
    fun refused(message: String) = fail(ConnectFailure.Refused(message))
    fun hostKeyChanged(fingerprint: String, pinned: String, host: String) =
        fail(ConnectFailure.HostKeyChanged(fingerprint, pinned, host))

    private fun fail(error: ConnectFailure) = continuation.resumeWithException(error)
}

class NativeSshReading internal constructor(private val continuation: CancellableContinuation<String?>) {
    fun received(line: String?) = continuation.resume(line)
    fun failed(message: String) = continuation.resumeWithException(ConnectionLost(message))
}

class NativeSshWriting internal constructor(private val continuation: CancellableContinuation<Unit>) {
    fun written() = continuation.resume(Unit)
    fun failed(message: String) = continuation.resumeWithException(ConnectionLost(message))
}

private class NativeLineChannel(private val channel: NativeSshChannel) : LineChannel {
    override suspend fun readLine(): String? = suspendCancellableCoroutine { continuation ->
        channel.read(NativeSshReading(continuation))
        continuation.invokeOnCancellation { channel.close() }
    }

    override suspend fun writeLine(line: String): Unit = suspendCancellableCoroutine { continuation ->
        channel.write(line, NativeSshWriting(continuation))
        continuation.invokeOnCancellation { channel.close() }
    }

    override fun close() = channel.close()
}
