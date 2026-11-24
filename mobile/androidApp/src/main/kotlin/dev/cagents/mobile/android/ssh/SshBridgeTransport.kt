package dev.cagents.mobile.android.ssh

import com.hierynomus.sshj.key.KeyAlgorithms
import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.client.AllFailed
import dev.cagents.mobile.client.BridgeTransport
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.LineChannel
import dev.cagents.mobile.client.firstToSucceed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.runInterruptible
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.common.KeyType
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.transport.TransportException
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import net.schmizz.sshj.userauth.UserAuthException
import net.schmizz.sshj.userauth.keyprovider.KeyProvider
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.security.PrivateKey
import java.security.PublicKey
import kotlin.concurrent.thread
import kotlin.time.Duration.Companion.milliseconds

/**
 * Reaches the bridge by SSH: authenticates with the [PhoneKey], checks the
 * computer's host key against the one pinned for it, and runs the bridge as
 * the remote command, its standard input and output carrying the API.
 */
class SshBridgeTransport(
    private val phoneKey: () -> PhoneKey,
    private val settings: () -> ConnectionSettings,
    private val hostKeys: HostKeyStore,
) : BridgeTransport {

    override suspend fun open(): LineChannel {
        val target = settings()
        if (!target.isComplete) throw ConnectFailure.Refused("Enter your computer's address in Settings.")
        return openSsh(
            SshTarget(
                name = target.label,
                hosts = target.hosts,
                port = target.port,
                user = target.user,
                key = phoneKey(),
                hostKeys = StoredHostKeys(hostKeys, target),
                command = target.command,
                unauthorized = { host ->
                    ConnectFailure.Unauthorized(
                        "${target.user}@$host didn't accept this phone's key. " +
                            "Pair again from the computer, or add the key to ~/.ssh/authorized_keys there, as Settings shows.",
                    )
                },
            ),
        )
    }
}

/** How to recognize the computer. */
internal interface HostKeyCheck {
    /** Null to accept the key [host] presented; otherwise why it isn't accepted. */
    fun check(host: String, fingerprint: String, keyType: String): ConnectFailure?

    /** The host key algorithms to ask [host] for first. */
    fun preferred(host: String): List<String>

    /** Whether to offer only `ssh-ed25519` host keys, so that an Ed25519 fingerprint applies. */
    val ed25519Only: Boolean get() = false
}

/** The keys trusted in the [HostKeyStore]. */
private class StoredHostKeys(private val store: HostKeyStore, private val target: ConnectionSettings) : HostKeyCheck {
    override fun check(host: String, fingerprint: String, keyType: String): ConnectFailure? {
        val pinned = store.pinned(target.hostKeyId(host))
        return when {
            pinned == null -> ConnectFailure.UnknownHostKey(fingerprint, keyType, host)
            pinned != fingerprint -> ConnectFailure.HostKeyChanged(fingerprint, pinned, host)
            else -> null
        }
    }

    override fun preferred(host: String) = store.pinnedType(target.hostKeyId(host))?.let(::listOf).orEmpty()
}

internal class SshTarget(
    /** What to call the computer in messages. */
    val name: String,
    /** Tried in order, each given a head start; see [firstToSucceed]. */
    val hosts: List<String>,
    val port: Int,
    val user: String,
    val key: SigningKey,
    val hostKeys: HostKeyCheck,
    val command: String,
    /** The failure when [host] refuses [key]. */
    val unauthorized: (host: String) -> ConnectFailure,
)

/** How long a preferred address has before the next one is tried as well. */
private val ADDRESS_HEAD_START = 1_500.milliseconds
private const val KEEPALIVE_SECONDS = 20

/**
 * Connects to the first of the target's hosts to answer with an acceptable
 * host key, authenticates there, and runs its command.
 */
internal suspend fun openSsh(target: SshTarget): LineChannel = withContext(Dispatchers.IO) {
    // Everything here may touch the network, disconnecting included, which
    // Android refuses on the main thread.
    val client = try {
        firstToSucceed(target.hosts, ADDRESS_HEAD_START, discard = { it.disconnectQuietly() }) { host ->
            connect(target, host)
        }
    } catch (e: AllFailed) {
        throw mostTelling(e.failures, target)
    }
    // Set once the channel exists, so whatever happens after, including a
    // cancellation that discards the result, the connection is closed.
    var made: SshLineChannel? = null
    try {
        blocking(client) {
            val host = client.remoteHostname
            try {
                client.authPublickey(target.user, SigningKeyProvider(target.key))
            } catch (e: UserAuthException) {
                throw target.unauthorized(host)
            }
            client.connection.keepAlive.keepAliveInterval = KEEPALIVE_SECONDS
            val session = client.startSession()
            SshLineChannel(client, session.exec(target.command)).also { made = it }
        }
    } catch (e: Throwable) {
        made?.close() ?: client.disconnectQuietly()
        throw when (e) {
            is CancellationException, is ConnectFailure -> e
            is Exception -> ConnectFailure.Network(describe(e, client.remoteHostname, target.port), e)
            else -> e
        }
    }
}

/** A connected client whose host key [HostKeyCheck] accepted, not yet authenticated. */
private suspend fun connect(target: SshTarget, host: String): SSHClient {
    val verifier = CheckingVerifier(target.hostKeys)
    val client = SSHClient(configFor(target.key, target.hostKeys.ed25519Only))
    client.addHostKeyVerifier(verifier)
    client.connectTimeout = 10_000
    // Bounds every read, including waiting for the server's banner. sshj's
    // reader carries on past a read timeout once connected, so it doesn't
    // limit how long the connection stays idle.
    client.timeout = 15_000
    try {
        blocking(client) { client.connect(host, target.port) }
    } catch (e: CancellationException) {
        // Cancelled even as it connected: nobody else will close it.
        client.disconnectQuietly()
        throw e
    } catch (e: TransportException) {
        client.disconnectQuietly()
        throw verifier.failure ?: ConnectFailure.Network(describe(e, host, target.port), e)
    } catch (e: Exception) {
        client.disconnectQuietly()
        throw ConnectFailure.Network(describe(e, host, target.port), e)
    }
    return client
}

/**
 * Runs blocking sshj [work] off the main thread. Socket I/O ignores
 * interrupts, so a cancellation disconnects [client] to end it.
 */
private suspend fun <T> blocking(client: SSHClient, work: () -> T): T = coroutineScope {
    val running = async(Dispatchers.IO) { work() }
    try {
        running.await()
    } catch (e: CancellationException) {
        client.disconnectQuietly()
        throw e
    }
}

private fun SSHClient.disconnectQuietly() {
    runCatching { disconnect() }
}

/**
 * Of every address's failure, the one that says most: a host key that
 * doesn't match above all, since it may mean an impersonator; then one to
 * trust; then a refusal; and only then that nothing answered.
 */
private fun mostTelling(failures: List<Throwable>, target: SshTarget): Throwable =
    failures.firstOrNull { it is ConnectFailure.HostKeyChanged }
        ?: failures.firstOrNull { it is ConnectFailure.UnknownHostKey }
        ?: failures.firstOrNull { it is ConnectFailure && it !is ConnectFailure.Network }
        ?: failures.singleOrNull()
        ?: ConnectFailure.Network(
            "Couldn't reach ${target.name} at any of its ${target.hosts.size} addresses. " +
                "Is it awake, and is this phone on its network or on Tailscale?",
            failures.first(),
        )

private fun describe(e: Throwable, host: String, port: Int): String {
    val address = "$host:$port"
    return when (generateSequence(e) { it.cause }.last()) {
        is UnknownHostException -> "Couldn't find $host. Check the address."
        is ConnectException, is NoRouteToHostException ->
            "Couldn't reach $address. Is the computer awake and on the same network?"
        is SocketTimeoutException -> "$address didn't answer in time."
        else -> e.message ?: "The connection to $address failed."
    }
}

private fun configFor(key: SigningKey, ed25519Only: Boolean) = DefaultConfig().apply {
    keyAlgorithms = keyAlgorithms.mapNotNull { factory ->
        when {
            factory.name == KeyType.ED25519.toString() ->
                KeyAlgorithms.Factory(factory.name, Ed25519Signature.Factory(key), KeyType.ED25519)
            ed25519Only -> null
            else -> factory
        }
    }
}

/** Hands sshj a [SigningKey]. The private half is a handle; [Ed25519Signature] signs with the real one. */
private class SigningKeyProvider(private val key: SigningKey) : KeyProvider {
    override fun getPrivate(): PrivateKey = SigningKeyHandle
    override fun getPublic(): PublicKey = Ed25519PublicKey(key.publicKey)
    override fun getType(): KeyType = KeyType.ED25519
}

private object SigningKeyHandle : PrivateKey {
    private fun readResolve(): Any = SigningKeyHandle
    override fun getAlgorithm() = "Ed25519"
    override fun getFormat(): String? = null
    override fun getEncoded(): ByteArray? = null
}

/** An Ed25519 public key in its X.509 encoding, whose last 32 bytes are the key, as sshj reads it. */
internal class Ed25519PublicKey(private val raw: ByteArray) : PublicKey {
    override fun getAlgorithm() = "Ed25519"
    override fun getFormat() = "X.509"
    override fun getEncoded(): ByteArray = X509_PREFIX + raw

    companion object {
        val X509_PREFIX = byteArrayOf(0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00)
    }
}

/** Asks the [HostKeyCheck] about the computer's host key, remembering why it refused one. */
private class CheckingVerifier(private val hostKeys: HostKeyCheck) : HostKeyVerifier {
    @Volatile
    var failure: ConnectFailure? = null

    override fun verify(hostname: String, port: Int, key: PublicKey): Boolean {
        failure = hostKeys.check(hostname, HostKeyStore.fingerprint(key), KeyType.fromKey(key).toString())
        return failure == null
    }

    override fun findExistingAlgorithms(hostname: String, port: Int): List<String> = hostKeys.preferred(hostname)
}

/**
 * The bridge's standard input and output as lines. What it writes to
 * standard error before its first line, such as why it refused to start, is
 * reported as the failure when it exits without speaking.
 */
private class SshLineChannel(
    private val client: SSHClient,
    command: Session.Command,
) : LineChannel {
    private val reader = BufferedReader(InputStreamReader(command.inputStream, Charsets.UTF_8))
    private val output: OutputStream = command.outputStream
    private val writeLock = Mutex()
    private val stderr = StringBuilder()
    private val stderrReader = thread(name = "cagents-bridge-stderr", isDaemon = true) {
        runCatching {
            command.errorStream.bufferedReader(Charsets.UTF_8).useLines { lines ->
                lines.forEach { line ->
                    synchronized(stderr) { if (stderr.length < 4096) stderr.appendLine(line) }
                }
            }
        }
    }

    @Volatile
    private var spoke = false

    override suspend fun readLine(): String? {
        val line = try {
            runInterruptible(Dispatchers.IO) { reader.readLine() }
        } catch (e: IOException) {
            if (!spoke) refusal()?.let { throw it }
            throw ConnectFailure.Network(e.message ?: "The connection was lost.", e)
        }
        if (line == null && !spoke) refusal()?.let { throw it }
        if (line != null) spoke = true
        return line
    }

    private fun refusal(): ConnectFailure? {
        stderrReader.join(1_000)
        val text = synchronized(stderr) { stderr.toString().trim() }
        return text.takeIf { it.isNotEmpty() }?.let { ConnectFailure.Refused(it) }
    }

    override suspend fun writeLine(line: String) = writeLock.withLock {
        withContext(Dispatchers.IO) {
            output.write((line + "\n").toByteArray(Charsets.UTF_8))
            output.flush()
        }
    }

    /**
     * Drops the whole connection without waiting for the computer: closing the
     * session channel first would wait for its reply, which a dead network never sends.
     */
    override fun close() {
        runCatching { client.disconnect() }
    }
}
