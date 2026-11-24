package dev.cagents.mobile.android.ssh

import com.hierynomus.sshj.key.KeyAlgorithms
import dev.cagents.mobile.android.data.ConnectionSettings
import dev.cagents.mobile.client.BridgeTransport
import dev.cagents.mobile.client.ConnectFailure
import dev.cagents.mobile.client.LineChannel
import kotlinx.coroutines.Dispatchers
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

    override suspend fun open(): LineChannel = withContext(Dispatchers.IO) {
        val target = settings()
        if (!target.isComplete) throw ConnectFailure.Refused("Enter your computer's address in Settings.")
        val key = phoneKey()
        val verifier = PinningVerifier(hostKeys, target.hostKeyId)
        val client = SSHClient(configFor(key))
        client.addHostKeyVerifier(verifier)
        client.connectTimeout = 10_000
        try {
            try {
                client.connect(target.host, target.port)
            } catch (e: TransportException) {
                verifier.failure?.let { throw it }
                throw ConnectFailure.Network(describe(e, target), e)
            } catch (e: IOException) {
                throw ConnectFailure.Network(describe(e, target), e)
            }
            try {
                client.authPublickey(target.user, PhoneKeyProvider(key))
            } catch (e: UserAuthException) {
                throw ConnectFailure.Unauthorized(
                    "${target.user}@${target.host} didn't accept this phone's key. " +
                        "Add it to ~/.ssh/authorized_keys there, as Settings shows.",
                )
            }
            client.connection.keepAlive.keepAliveInterval = KEEPALIVE_SECONDS
            val session = client.startSession()
            val command = session.exec(target.command)
            SshLineChannel(client, session, command)
        } catch (e: ConnectFailure) {
            runCatching { client.disconnect() }
            throw e
        } catch (e: IOException) {
            runCatching { client.disconnect() }
            throw ConnectFailure.Network(describe(e, target), e)
        }
    }

    private fun describe(e: Throwable, target: ConnectionSettings): String {
        val address = "${target.host}:${target.port}"
        return when (e.rootCause()) {
            is UnknownHostException -> "Couldn't find ${target.host}. Check the address in Settings."
            is ConnectException, is NoRouteToHostException ->
                "Couldn't reach $address. Is the computer awake and on the same network?"
            is SocketTimeoutException -> "$address didn't answer in time."
            else -> e.message ?: "The connection to $address failed."
        }
    }

    private fun Throwable.rootCause(): Throwable = generateSequence(this) { it.cause }.last()

    private fun configFor(key: PhoneKey) = DefaultConfig().apply {
        keyAlgorithms = keyAlgorithms.map { factory ->
            if (factory.name == KeyType.ED25519.toString()) {
                KeyAlgorithms.Factory(factory.name, PhoneKeySignature.Factory(key), KeyType.ED25519)
            } else {
                factory
            }
        }
    }

    private companion object {
        const val KEEPALIVE_SECONDS = 20
    }
}

/** Hands sshj the phone's key. The private half is a handle; [PhoneKeySignature] signs with the real one. */
private class PhoneKeyProvider(private val key: PhoneKey) : KeyProvider {
    override fun getPrivate(): PrivateKey = PhoneKeyHandle
    override fun getPublic(): PublicKey = Ed25519PublicKey(key.publicKey)
    override fun getType(): KeyType = KeyType.ED25519
}

private object PhoneKeyHandle : PrivateKey {
    private fun readResolve(): Any = PhoneKeyHandle
    override fun getAlgorithm() = "Ed25519"
    override fun getFormat(): String? = null
    override fun getEncoded(): ByteArray? = null
}

/** An Ed25519 public key in its X.509 encoding, whose last 32 bytes are the key, as sshj reads it. */
internal class Ed25519PublicKey(private val raw: ByteArray) : PublicKey {
    override fun getAlgorithm() = "Ed25519"
    override fun getFormat() = "X.509"
    override fun getEncoded(): ByteArray = X509_PREFIX + raw

    private companion object {
        val X509_PREFIX = byteArrayOf(0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00)
    }
}

/** Checks the computer's host key against the pin, remembering why it refused one. */
private class PinningVerifier(
    private val store: HostKeyStore,
    private val hostKeyId: String,
) : HostKeyVerifier {
    @Volatile
    var failure: ConnectFailure? = null

    override fun verify(hostname: String, port: Int, key: PublicKey): Boolean {
        val fingerprint = HostKeyStore.fingerprint(key)
        val pinned = store.pinned(hostKeyId)
        return when {
            pinned == null -> {
                failure = ConnectFailure.UnknownHostKey(fingerprint, KeyType.fromKey(key).toString())
                false
            }
            pinned != fingerprint -> {
                failure = ConnectFailure.HostKeyChanged(fingerprint, pinned)
                false
            }
            else -> true
        }
    }

    override fun findExistingAlgorithms(hostname: String, port: Int): List<String> =
        store.pinnedType(hostKeyId)?.let(::listOf).orEmpty()
}

/**
 * The bridge's standard input and output as lines. What it writes to
 * standard error before its first line, such as why it refused to start, is
 * reported as the failure when it exits without speaking.
 */
private class SshLineChannel(
    private val client: SSHClient,
    private val session: Session,
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

    override fun close() {
        runCatching { session.close() }
        runCatching { client.disconnect() }
    }
}
